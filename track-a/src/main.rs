//! Speed Demon 2026, Track A (Ingest).
//!
//! 1. Map `requests.log` and parse it in parallel chunks. Each thread interns endpoints into its
//!    own table and appends one packed `u64` record per line: hour, endpoint, 5xx flag, latency.
//! 2. Merge the endpoint tables into one sorted list, so that a group id
//!    `(hour - first_hour) * endpoints + endpoint_rank` is already in output order.
//! 3. Counting sort: per-thread group counts give every thread its own slots, then each thread
//!    scatters its latencies into one array where every group is contiguous.
//! 4. Nearest-rank percentiles with `select_nth_unstable` (no full sort), formatted in parallel.
use std::collections::HashMap;
use std::hash::{BuildHasherDefault, Hasher};
use std::io::Write;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

unsafe extern "C" {
    fn mmap(addr: *mut u8, len: usize, prot: i32, flags: i32, fd: i32, off: i64) -> *mut u8;
}

/// Maps a whole file read-only for the life of the process.
fn map_file(path: &str) -> &'static [u8] {
    use std::os::fd::AsRawFd;
    let file = std::fs::File::open(path).expect("cannot open input");
    let len = file.metadata().expect("cannot stat input").len() as usize;
    if len == 0 {
        return &[];
    }
    // SAFETY: PROT_READ + MAP_PRIVATE of a file on a read-only mount; it is never unmapped.
    let ptr = unsafe { mmap(std::ptr::null_mut(), len, 1, 2, file.as_raw_fd(), 0) };
    assert!(ptr as isize != -1, "mmap failed");
    unsafe { std::slice::from_raw_parts(ptr, len) }
}

/// FxHash: one rotate, xor and multiply per 8 bytes. Endpoints are short, so this beats SipHash.
#[derive(Default)]
struct Fx(u64);
impl Hasher for Fx {
    fn write(&mut self, bytes: &[u8]) {
        for chunk in bytes.chunks(8) {
            let mut word = [0u8; 8];
            word[..chunk.len()].copy_from_slice(chunk);
            self.0 =
                (self.0.rotate_left(5) ^ u64::from_le_bytes(word)).wrapping_mul(0x517cc1b727220a95);
        }
    }
    fn finish(&self) -> u64 {
        self.0
    }
}

/// Days since 1970-01-01 for a proleptic Gregorian date (Howard Hinnant's algorithm).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * ((m + 9) % 12) + 2) / 5 + d - 1;
    era * 146097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719468
}

/// The inverse of `days_from_civil`: (year, month, day).
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (
        yoe + era * 400 + (m <= 2) as i64,
        m,
        doy - (153 * mp + 2) / 5 + 1,
    )
}

fn digits(bytes: &[u8]) -> i64 {
    bytes.iter().fold(0, |acc, &b| acc * 10 + (b - b'0') as i64)
}

/// Hours since the epoch from the first 13 bytes of a timestamp, `YYYY-MM-DDTHH`.
fn hour_of(ts: &[u8]) -> i64 {
    days_from_civil(digits(&ts[0..4]), digits(&ts[5..7]), digits(&ts[8..10])) * 24
        + digits(&ts[11..13])
}

// Record layout: latency in bits 0-31, thread-local endpoint id in 32-47, the 5xx flag in bit 48,
// and the hour relative to the first line's hour, biased by HOUR_BIAS, in bits 49-63.
const HOUR_BIAS: i64 = 1 << 14;

struct Shard {
    endpoints: Vec<&'static [u8]>,
    records: Vec<u64>,
}

fn parse(
    chunk: &'static [u8],
    base_hour: i64,
    shard: &mut Shard,
    ids: &mut HashMap<&'static [u8], u16, BuildHasherDefault<Fx>>,
) {
    let (mut p, mut last_prefix, mut hour_bits) = (0, &chunk[..0], 0u64);
    while p < chunk.len() {
        let line = &chunk[p..];
        if &line[..13] != last_prefix {
            last_prefix = &line[..13];
            let rel = hour_of(line) - base_hour + HOUR_BIAS;
            assert!(
                (0..2 * HOUR_BIAS).contains(&rel),
                "timestamps span more than 1.8 years"
            );
            hour_bits = (rel as u64) << 49;
        }
        // The timestamp is 27 bytes, then a space and a method of 3 to 7 bytes.
        let mut i = 31;
        while line[i] != b' ' {
            i += 1;
        }
        let mut j = i + 1;
        while line[j] != b' ' {
            j += 1;
        }
        let endpoint = &line[i + 1..j];
        let next_id = shard.endpoints.len();
        let id = *ids.entry(endpoint).or_insert_with(|| {
            shard.endpoints.push(endpoint);
            u16::try_from(next_id).expect("more than 65536 endpoints in one chunk")
        });
        let error = (line[j + 1] == b'5') as u64;
        let mut k = j + 5;
        let mut latency = 0u64;
        while line[k] != b'\n' {
            latency = latency * 10 + (line[k] - b'0') as u64;
            k += 1;
        }
        shard
            .records
            .push(hour_bits | error << 48 | (id as u64) << 32 | latency);
        p += k + 1;
    }
}

/// Splits `data` into about `parts` pieces that each end with a newline.
fn split_lines(data: &'static [u8], parts: usize) -> Vec<&'static [u8]> {
    let mut chunks = Vec::new();
    let mut start = 0;
    for i in 1..=parts {
        let mut end = (data.len() * i / parts).max(start);
        while end < data.len() && data[end - 1] != b'\n' {
            end += 1;
        }
        if end > start {
            chunks.push(&data[start..end]);
            start = end;
        }
    }
    chunks
}

#[derive(Clone, Copy)]
struct SharedPtr(*mut u32);
// SAFETY: every thread writes a disjoint set of indices (its own counting-sort slots).
unsafe impl Send for SharedPtr {}
unsafe impl Sync for SharedPtr {}

fn push_num(out: &mut Vec<u8>, mut value: u64) {
    let mut buf = [0u8; 20];
    let mut i = buf.len();
    loop {
        i -= 1;
        buf[i] = b'0' + (value % 10) as u8;
        value /= 10;
        if value == 0 {
            break;
        }
    }
    out.extend_from_slice(&buf[i..]);
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let data_dir = args.get(1).map_or("/data", |s| s.as_str());
    let out_dir = args.get(2).map_or("/out", |s| s.as_str());
    let data = map_file(&format!("{data_dir}/requests.log"));
    let mut out =
        std::fs::File::create(format!("{out_dir}/summary.txt")).expect("cannot create output");
    if data.is_empty() {
        return;
    }
    let threads = std::thread::available_parallelism().map_or(1, |n| n.get());
    let base_hour = hour_of(data);

    // Phase 1: parse. Threads pull chunks from a shared counter so that they finish together.
    let chunks = split_lines(data, threads * 16);
    let next = AtomicUsize::new(0);
    let mut shards: Vec<Shard> = std::thread::scope(|scope| {
        let workers: Vec<_> = (0..threads)
            .map(|_| {
                scope.spawn(|| {
                    let mut shard = Shard {
                        endpoints: Vec::new(),
                        records: Vec::with_capacity(data.len() / 70 / threads),
                    };
                    let mut ids = HashMap::default();
                    while let Some(chunk) = chunks.get(next.fetch_add(1, Ordering::Relaxed)) {
                        parse(chunk, base_hour, &mut shard, &mut ids);
                    }
                    shard
                })
            })
            .collect();
        workers.into_iter().map(|w| w.join().unwrap()).collect()
    });

    // Phase 2: one sorted endpoint list (byte order), the hour range, and dense group ids.
    let mut names: Vec<&[u8]> = shards
        .iter()
        .flat_map(|s| s.endpoints.iter().copied())
        .collect();
    names.sort_unstable();
    names.dedup();
    let ranks: Vec<Vec<u32>> = shards
        .iter()
        .map(|s| {
            s.endpoints
                .iter()
                .map(|e| names.binary_search(e).unwrap() as u32)
                .collect()
        })
        .collect();
    let (lo, hi) = shards
        .iter()
        .flat_map(|s| s.records.iter())
        .fold((u64::MAX, 0), |(lo, hi), r| {
            (lo.min(r >> 49), hi.max(r >> 49))
        });
    let groups = (hi - lo + 1) as usize * names.len();
    let group_of = |t: usize, r: u64| {
        ((r >> 49) - lo) as usize * names.len() + ranks[t][(r >> 32) as u16 as usize] as usize
    };

    // Phase 3: counting sort into one latency array, group by group, plus 5xx counts.
    let errors: Vec<AtomicU32> = (0..groups).map(|_| AtomicU32::new(0)).collect();
    let mut slots: Vec<Vec<u32>> = std::thread::scope(|scope| {
        let workers: Vec<_> = (0..shards.len())
            .map(|t| {
                let (shard, errors, group_of) = (&shards[t], &errors, &group_of);
                scope.spawn(move || {
                    let mut counts = vec![0u32; groups];
                    for &r in &shard.records {
                        let g = group_of(t, r);
                        counts[g] += 1;
                        if r >> 48 & 1 == 1 {
                            errors[g].fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    counts
                })
            })
            .collect();
        workers.into_iter().map(|w| w.join().unwrap()).collect()
    });
    let mut starts = vec![0u32; groups + 1];
    let mut position = 0u32;
    for g in 0..groups {
        starts[g] = position;
        for shard_slots in slots.iter_mut() {
            let count = shard_slots[g];
            shard_slots[g] = position;
            position += count;
        }
    }
    starts[groups] = position;
    let mut values = vec![0u32; position as usize];
    let values_ptr = SharedPtr(values.as_mut_ptr());
    std::thread::scope(|scope| {
        for (t, shard_slots) in slots.iter_mut().enumerate() {
            let (shard, group_of) = (&shards[t], &group_of);
            scope.spawn(move || {
                let ptr = values_ptr;
                for &r in &shard.records {
                    let slot = &mut shard_slots[group_of(t, r)];
                    // SAFETY: slot indices are unique across threads by construction of the prefix sums.
                    unsafe { *ptr.0.add(*slot as usize) = r as u32 };
                    *slot += 1;
                }
            });
        }
    });
    drop(slots);
    shards.clear();

    // Phase 4: percentiles and formatting, in batches of groups pulled from a counter.
    let hour_text: Vec<String> = (lo..=hi)
        .map(|h| {
            let hour = h as i64 - HOUR_BIAS + base_hour;
            let (y, m, d) = civil_from_days(hour.div_euclid(24));
            format!("{y:04}-{m:02}-{d:02}T{:02}:00:00Z ", hour.rem_euclid(24))
        })
        .collect();
    let batch = groups.div_ceil(threads * 32).max(1);
    let next = AtomicUsize::new(0);
    let parts = Mutex::new(Vec::new());
    std::thread::scope(|scope| {
        for _ in 0..threads {
            scope.spawn(|| {
                let ptr = values_ptr;
                loop {
                    let first = next.fetch_add(1, Ordering::Relaxed) * batch;
                    if first >= groups {
                        break;
                    }
                    let mut text = Vec::new();
                    for g in first..(first + batch).min(groups) {
                        let (start, end) = (starts[g] as usize, starts[g + 1] as usize);
                        let n = end - start;
                        if n == 0 {
                            continue;
                        }
                        // SAFETY: groups are disjoint ranges of `values`, and each batch is taken once.
                        let v = unsafe { std::slice::from_raw_parts_mut(ptr.0.add(start), n) };
                        let rank = |p: usize| (p * n + 99) / 100 - 1;
                        let (r50, r95, r99) = (rank(50), rank(95), rank(99));
                        // After selecting rank r, the r smallest values sit in v[..r], so each
                        // lower rank is selected inside the previous prefix.
                        let p99 = *v.select_nth_unstable(r99).1;
                        let p95 = if r95 < r99 {
                            *v[..r99].select_nth_unstable(r95).1
                        } else {
                            p99
                        };
                        let p50 = if r50 < r95 {
                            *v[..r95].select_nth_unstable(r50).1
                        } else {
                            p95
                        };
                        let errs = errors[g].load(Ordering::Relaxed) as u64;
                        text.extend_from_slice(hour_text[g / names.len()].as_bytes());
                        text.extend_from_slice(names[g % names.len()]);
                        for value in [
                            n as u64,
                            p50 as u64,
                            p95 as u64,
                            p99 as u64,
                            errs,
                            errs * 1_000_000 / n as u64,
                        ] {
                            text.push(b' ');
                            push_num(&mut text, value);
                        }
                        text.push(b'\n');
                    }
                    parts.lock().unwrap().push((first, text));
                }
            });
        }
    });
    let mut parts = parts.into_inner().unwrap();
    parts.sort_unstable_by_key(|p| p.0);
    for (_, text) in parts {
        out.write_all(&text).expect("cannot write output");
    }
}
