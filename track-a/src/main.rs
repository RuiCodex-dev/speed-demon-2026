//! Speed Demon 2026, Track A (Ingest).
//!
//! 1. Map `requests.log` and cut it into one chunk per thread. Each thread interns endpoints into
//!    its own table and appends one packed `u64` per line: hour, endpoint, 5xx flag, latency.
//! 2. Merge the endpoint tables into one sorted list, so that a group id
//!    `(hour - first_hour) * endpoints + endpoint_rank` is already the output order.
//! 3. Each thread rewrites its records in place as `group << 33 | 5xx << 32 | latency` and sorts
//!    them, so every group is one contiguous run per chunk. No other per-line array is allocated.
//! 4. Groups are formatted in parallel ranges: a group's runs are gathered from every chunk into a
//!    small scratch buffer and the nearest-rank percentiles come from `select_nth_unstable`.
use std::io::Write;

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

/// The 8 bytes of an endpoint starting at `i`, with bytes past its end masked to zero. An endpoint
/// is always followed by at least 7 more bytes of its line (" 200 0\n" is the shortest tail), so an
/// 8-byte read from any position inside it stays inside the mapping.
fn word(name: &[u8], i: usize) -> u64 {
    // SAFETY: see above; `name` is a slice of the mapped file with that tail after it.
    let w = unsafe { std::ptr::read_unaligned(name.as_ptr().add(i) as *const u64) };
    if name.len() - i >= 8 { w } else { w & ((1 << (8 * (name.len() - i))) - 1) }
}

/// FxHash-style hash of an endpoint: one rotate, xor and multiply per 8 bytes, no copying.
fn hash(name: &[u8]) -> u64 {
    (0..name.len()).step_by(8).fold(name.len() as u64, |h, i| (h.rotate_left(5) ^ word(name, i)).wrapping_mul(0x517cc1b727220a95))
}

/// Byte equality of two endpoints, 8 bytes at a time.
fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && (0..a.len()).step_by(8).all(|i| word(a, i) == word(b, i))
}

/// Index of the first space at or after `i`, testing 8 bytes at a time where 8 bytes remain.
fn find_space(s: &[u8], mut i: usize) -> usize {
    while let Some(word) = s.get(i..i + 8) {
        let w = u64::from_le_bytes(word.try_into().unwrap()) ^ 0x2020_2020_2020_2020;
        let zero = w.wrapping_sub(0x0101_0101_0101_0101) & !w & 0x8080_8080_8080_8080;
        if zero != 0 {
            return i + zero.trailing_zeros() as usize / 8; // the lowest zero byte is exact
        }
        i += 8;
    }
    i + s[i..].iter().position(|&b| b == b' ').unwrap()
}

/// A thread's endpoint -> local id table: open addressing, comparing the stored hash before bytes.
struct Endpoints {
    slots: Vec<u32>, // 0 = empty, otherwise local id + 1
    names: Vec<(&'static [u8], u64)>,
}

impl Endpoints {
    fn id(&mut self, name: &'static [u8]) -> u16 {
        let h = hash(name);
        let mask = self.slots.len() - 1;
        let mut i = (h >> 40) as usize & mask;
        while self.slots[i] != 0 {
            let (known, known_hash) = self.names[self.slots[i] as usize - 1];
            if known_hash == h && same(known, name) {
                return self.slots[i] as u16 - 1;
            }
            i = (i + 1) & mask;
        }
        self.names.push((name, h));
        self.slots[i] = self.names.len() as u32;
        if self.names.len() * 2 > self.slots.len() {
            let mut bigger = Endpoints {
                slots: vec![0; self.slots.len() * 2],
                names: Vec::new(),
            };
            for &(n, _) in &self.names {
                bigger.id(n);
            }
            *self = bigger;
        }
        u16::try_from(self.names.len() - 1).expect("more than 65536 endpoints in one chunk")
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

// Parse layout: latency in bits 0-31, thread-local endpoint id in 32-47, the 5xx flag in bit 48,
// and the hour relative to the first line's hour, biased by HOUR_BIAS, in bits 49-63.
const HOUR_BIAS: i64 = 1 << 14;

struct Shard {
    endpoints: Endpoints,
    records: Vec<u64>,
    /// Smallest and largest biased hour seen.
    hours: (u64, u64),
}

fn parse(chunk: &'static [u8], base_hour: i64) -> Shard {
    // Lines are 40 to 160 bytes and about 77 on average; grow only if this guess is low.
    let mut shard = Shard {
        endpoints: Endpoints {
            slots: vec![0; 4096],
            names: Vec::new(),
        },
        records: Vec::with_capacity(chunk.len() / 70),
        hours: (u64::MAX, 0),
    };
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
            shard.hours = (shard.hours.0.min(rel as u64), shard.hours.1.max(rel as u64));
        }
        // The timestamp is 27 bytes, then a space and a method of 3 to 7 bytes.
        let i = find_space(line, 31);
        let j = find_space(line, i + 1);
        let endpoint = &line[i + 1..j];
        let id = shard.endpoints.id(endpoint);
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
    shard
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

    // Phase 1: parse, one chunk per thread (lines are uniform, so equal byte ranges balance).
    let chunks = split_lines(data, threads);
    let mut shards: Vec<Shard> = std::thread::scope(|scope| {
        let workers: Vec<_> = chunks
            .iter()
            .map(|&c| scope.spawn(move || parse(c, base_hour)))
            .collect();
        workers.into_iter().map(|w| w.join().unwrap()).collect()
    });

    // Phase 2: one sorted endpoint list (byte order), the hour range, and dense group ids.
    let mut names: Vec<&[u8]> = shards
        .iter()
        .flat_map(|s| s.endpoints.names.iter().map(|e| e.0))
        .collect();
    names.sort_unstable();
    names.dedup();
    let lo = shards.iter().map(|s| s.hours.0).min().unwrap();
    let hi = shards.iter().map(|s| s.hours.1).max().unwrap();
    let groups = (hi - lo + 1) as usize * names.len();
    assert!(groups < 1 << 31, "too many (hour, endpoint) groups");

    // Phase 3: rewrite every record as (group, 5xx, latency) and sort each chunk by group.
    std::thread::scope(|scope| {
        for shard in shards.iter_mut() {
            let names = &names;
            scope.spawn(move || {
                let ranks: Vec<u64> = shard
                    .endpoints
                    .names
                    .iter()
                    .map(|e| names.binary_search(&e.0).unwrap() as u64)
                    .collect();
                for r in shard.records.iter_mut() {
                    let group =
                        ((*r >> 49) - lo) * names.len() as u64 + ranks[(*r >> 32) as u16 as usize];
                    *r = group << 33 | (*r >> 48 & 1) << 32 | (*r as u32 as u64);
                }
                shard.records.sort_unstable();
            });
        }
    });

    // Phase 4: format contiguous ranges of groups in parallel. Each range finds where its first
    // group starts in every chunk, then walks all chunks forward in step.
    let hour_text: Vec<String> = (lo..=hi)
        .map(|h| {
            let hour = h as i64 - HOUR_BIAS + base_hour;
            let (y, m, d) = civil_from_days(hour.div_euclid(24));
            format!("{y:04}-{m:02}-{d:02}T{:02}:00:00Z ", hour.rem_euclid(24))
        })
        .collect();
    let ranges = threads;
    let parts: Vec<Vec<u8>> = std::thread::scope(|scope| {
        let workers: Vec<_> = (0..ranges)
            .map(|k| {
                let (shards, names, hour_text) = (&shards, &names, &hour_text);
                scope.spawn(move || {
                    let (first, last) = (
                        (groups * k / ranges) as u64,
                        (groups * (k + 1) / ranges) as u64,
                    );
                    let mut cursors: Vec<usize> = shards
                        .iter()
                        .map(|s| s.records.partition_point(|&r| r >> 33 < first))
                        .collect();
                    let (mut text, mut values) = (Vec::new(), Vec::new());
                    for g in first..last {
                        values.clear();
                        let mut errors = 0u64;
                        for (shard, cursor) in shards.iter().zip(cursors.iter_mut()) {
                            while let Some(&r) =
                                shard.records.get(*cursor).filter(|&&r| r >> 33 == g)
                            {
                                values.push(r as u32);
                                errors += r >> 32 & 1;
                                *cursor += 1;
                            }
                        }
                        let n = values.len();
                        if n == 0 {
                            continue;
                        }
                        let rank = |p: usize| (p * n + 99) / 100 - 1;
                        let (r50, r95, r99) = (rank(50), rank(95), rank(99));
                        // After selecting rank r, the r smallest values sit in values[..r], so each
                        // lower rank is selected inside the previous prefix.
                        let p99 = *values.select_nth_unstable(r99).1;
                        let p95 = if r95 < r99 {
                            *values[..r99].select_nth_unstable(r95).1
                        } else {
                            p99
                        };
                        let p50 = if r50 < r95 {
                            *values[..r95].select_nth_unstable(r50).1
                        } else {
                            p95
                        };
                        let g = g as usize;
                        text.extend_from_slice(hour_text[g / names.len()].as_bytes());
                        text.extend_from_slice(names[g % names.len()]);
                        for value in [
                            n as u64,
                            p50 as u64,
                            p95 as u64,
                            p99 as u64,
                            errors,
                            errors * 1_000_000 / n as u64,
                        ] {
                            text.push(b' ');
                            push_num(&mut text, value);
                        }
                        text.push(b'\n');
                    }
                    text
                })
            })
            .collect();
        workers.into_iter().map(|w| w.join().unwrap()).collect()
    });
    for text in parts {
        out.write_all(&text).expect("cannot write output");
    }
}
