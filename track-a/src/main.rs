//! Speed Demon 2026, Track A (Ingest).
//!
//! 1. Map `requests.log` and cut it into one chunk per thread. Each thread interns endpoints into
//!    its own table and parses its chunk in 4 MB pieces: one packed `u64` per line (hour, endpoint,
//!    5xx flag, latency), sorted by (hour, endpoint) and reduced to the latencies (u32) and one run
//!    `[key, start, end, 5xx]` per key before the next piece is read.
//! 2. Merge the endpoint tables into one sorted list, so that a group id
//!    `(hour - first_hour) * endpoints + endpoint_rank` is already the output order; every run's
//!    key becomes its group.
//! 3. Groups are summarised in parallel ranges: a group's runs are gathered from every piece into a
//!    small scratch buffer and the nearest-rank percentiles come from `select_nth_unstable`.
//! 4. The latencies are freed, then the text is formatted and written. The output's page cache
//!    counts as memory, so no phase holds the records, the latencies and the text together.
use std::arch::x86_64::*;
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
/// in the input is always followed by at least 7 more bytes of its line (" 200 0\n" is the
/// shortest tail), and a stored copy by 8 bytes of padding, so an 8-byte read from any position
/// inside it stays inside its buffer.
fn word(name: &[u8], i: usize) -> u64 {
    // SAFETY: see above; `name` is a slice of the mapped file with that tail after it.
    let w = unsafe { std::ptr::read_unaligned(name.as_ptr().add(i) as *const u64) };
    if name.len() - i >= 8 {
        w
    } else {
        w & ((1 << (8 * (name.len() - i))) - 1)
    }
}

/// Hash of an endpoint from its first and last 8 bytes and its length: constant work per line, and
/// endpoints sharing a long prefix still differ at the end. Equality is always checked in full.
fn hash(name: &[u8]) -> u64 {
    let last = word(name, name.len().saturating_sub(8));
    (word(name, 0) ^ last.rotate_left(29) ^ name.len() as u64).wrapping_mul(0x517cc1b727220a95)
}

/// Byte equality of two endpoints, 32 bytes at a time. `maskload` reads only the 4-byte lanes that
/// start inside the name and never faults on the others; a lane can end up to 3 bytes past the
/// name, which the padding of a copy and the rest of an input line (>= 7 bytes) cover.
fn same(a: &[u8], b: &[u8]) -> bool {
    let starts = unsafe { _mm256_setr_epi32(0, 4, 8, 12, 16, 20, 24, 28) };
    a.len() == b.len()
        && (0..a.len()).step_by(32).all(|i| {
            let rest = a.len() - i;
            // SAFETY: AVX2 is enabled at build time (x86-64-v3); the loads are bounded as above.
            let equal = unsafe {
                let lanes = _mm256_cmpgt_epi32(_mm256_set1_epi32(rest.min(32) as i32), starts);
                let x = _mm256_maskload_epi32(a.as_ptr().add(i) as *const i32, lanes);
                let y = _mm256_maskload_epi32(b.as_ptr().add(i) as *const i32, lanes);
                _mm256_movemask_epi8(_mm256_cmpeq_epi8(x, y)) as u32
            };
            let want = u32::MAX >> (32 - rest.min(32)); // rest >= 1
            equal & want == want
        })
}

/// Index of the first space at or after `i`, testing 32 bytes at a time where 32 bytes remain.
fn find_space(s: &[u8], mut i: usize) -> usize {
    while i + 32 <= s.len() {
        // SAFETY: AVX2 is enabled at build time (x86-64-v3), and the 32 bytes are inside `s`.
        let spaces = unsafe {
            let x = _mm256_loadu_si256(s.as_ptr().add(i) as *const __m256i);
            _mm256_movemask_epi8(_mm256_cmpeq_epi8(x, _mm256_set1_epi8(b' ' as i8))) as u32
        };
        if spaces != 0 {
            return i + spaces.trailing_zeros() as usize;
        }
        i += 32;
    }
    i + s[i..].iter().position(|&b| b == b' ').unwrap() // the last 31 bytes of a chunk
}

/// The decimal at `s[i..]`, which ends at the first byte below '0', and its length. Up to 7 digits
/// come from one 8-byte load: find the end with one SWAR test, combine digits pairwise.
fn number(s: &[u8], i: usize) -> (u64, usize) {
    if let Some(bytes) = s.get(i..i + 8) {
        let x = u64::from_le_bytes(bytes.try_into().unwrap());
        let stop = x.wrapping_sub(0x3030_3030_3030_3030) & !x & 0x8080_8080_8080_8080;
        let len = stop.trailing_zeros() as usize / 8; // 8 when no end in sight
        if (1..8).contains(&len) {
            let d = (x & 0x0F0F_0F0F_0F0F_0F0F) << (64 - 8 * len);
            let d = (d * 10 + (d >> 8)) & 0x00FF_00FF_00FF_00FF;
            let d = (d * 100 + (d >> 16)) & 0x0000_FFFF_0000_FFFF;
            return ((d * 10000 + (d >> 32)) & 0xFFFF_FFFF, len);
        }
    }
    let len = s[i..].iter().position(|&b| b < b'0').unwrap();
    let value = s[i..i + len]
        .iter()
        .fold(0, |v, &b| v * 10 + (b - b'0') as u64);
    (value, len)
}

/// A thread's endpoint -> local id table: open addressing, comparing the stored hash before bytes.
/// The names are copies, each followed by 8 bytes of padding for `word`.
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
        // Keep a padded copy: comparing against the name's first line in the 239 MB input was a
        // cache miss on every lookup, while ~2,000 copies stay in cache.
        let copy = Vec::leak([name, &[0; 8]].concat());
        self.names.push((&copy[..name.len()], h));
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

// Parse layout: latency in bits 0-31, the 5xx flag in bit 32, and above it the local key
// `hour << 16 | thread-local endpoint id`, the hour relative to the first line's hour and biased
// by HOUR_BIAS, so the key fits in 31 bits.
const HOUR_BIAS: i64 = 1 << 14;

/// Bytes of a chunk parsed before its records are sorted and reduced (about 54,000 lines).
const PIECE: usize = 1 << 22;

/// One thread's share of the input: its endpoint table, its latencies, and the runs
/// `[key, start, end, 5xx count]` into `lats`, one per key in each piece (by key; later by global
/// group). `pieces` holds where each piece's runs end. One vector each, so that freeing them hands
/// the memory back.
struct Shard {
    endpoints: Endpoints,
    lats: Vec<u32>,
    runs: Vec<[u32; 4]>,
    pieces: Vec<usize>,
    /// Smallest and largest biased hour seen.
    hours: (u64, u64),
}

/// Parses `chunk` a piece at a time: a piece's packed records are sorted by key and reduced to
/// latencies and runs before the next piece is read, so only one piece of 8-byte records exists.
fn parse(chunk: &'static [u8], base_hour: i64) -> Shard {
    // Lines are 40 to 160 bytes and about 77 on average; grow only if this guess is low.
    let mut shard = Shard {
        endpoints: Endpoints {
            slots: vec![0; 4096],
            names: Vec::new(),
        },
        lats: Vec::with_capacity(chunk.len() / 70),
        runs: Vec::with_capacity(chunk.len() / 512),
        pieces: Vec::new(),
        hours: (u64::MAX, 0),
    };
    let mut records = Vec::with_capacity(PIECE / 40);
    // The hour is the first 13 bytes, compared as two overlapping 8-byte words.
    let prefix = |line: &[u8]| (word(line, 0), word(&line[5..], 0));
    let (mut p, mut last_prefix, mut hour_bits) = (0, (0, 0), 0u64);
    while p < chunk.len() {
        let end = p + PIECE; // the last line may cross it
        while p < chunk.len() && p < end {
            let line = &chunk[p..];
            if prefix(line) != last_prefix {
                last_prefix = prefix(line);
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
            let id = shard.endpoints.id(&line[i + 1..j]);
            let error = (line[j + 1] == b'5') as u64;
            let (latency, len) = number(line, j + 5);
            records.push(hour_bits | (id as u64) << 33 | error << 32 | latency);
            p += j + 5 + len + 1;
        }
        // By key only: percentiles are selected later, so the order inside a key does not
        // matter, and equal keys are cheap for pdqsort.
        records.sort_unstable_by_key(|r| r >> 33);
        let (runs, first_run) = (&mut shard.runs, shard.pieces.last().copied().unwrap_or(0));
        for r in records.drain(..) {
            let (key, at) = ((r >> 33) as u32, shard.lats.len() as u32);
            let in_piece = runs.len() > first_run;
            match runs.last_mut() {
                Some(last) if in_piece && last[0] == key => last[2] += 1,
                _ => runs.push([key, at, at + 1, 0]),
            }
            runs.last_mut().unwrap()[3] += (r >> 32 & 1) as u32;
            shard.lats.push(r as u32);
        }
        shard.pieces.push(shard.runs.len());
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
    let mut names: Vec<&'static [u8]> = shards
        .iter()
        .flat_map(|s| s.endpoints.names.iter().map(|e| e.0))
        .collect();
    names.sort_unstable();
    names.dedup();
    let lo = shards.iter().map(|s| s.hours.0).min().unwrap();
    let hi = shards.iter().map(|s| s.hours.1).max().unwrap();
    let groups = (hi - lo + 1) as usize * names.len();
    assert!(groups < 1 << 31, "too many (hour, endpoint) groups");

    // Phase 3: turn every run's local key into its global group and re-sort each piece's runs by
    // group (a piece has a few thousand runs).
    std::thread::scope(|scope| {
        for shard in shards.iter_mut() {
            let names = &names;
            scope.spawn(move || {
                let ranks: Vec<u32> = (shard.endpoints.names.iter())
                    .map(|e| names.binary_search(&e.0).unwrap() as u32)
                    .collect();
                let mut start = 0;
                for &end in &shard.pieces {
                    let runs = &mut shard.runs[start..end];
                    start = end;
                    for run in runs.iter_mut() {
                        let hour = (run[0] >> 16) as usize - lo as usize;
                        run[0] = (hour * names.len()) as u32 + ranks[run[0] as u16 as usize];
                    }
                    runs.sort_unstable_by_key(|run| run[0]);
                }
            });
        }
    });
    let pieces: Vec<(&[[u32; 4]], &[u32])> = (shards.iter())
        .flat_map(|s| {
            let starts = std::iter::once(0).chain(s.pieces.iter().copied());
            (starts.zip(&s.pieces)).map(|(a, &b)| (&s.runs[a..b], &s.lats[..]))
        })
        .collect();

    // Phase 4: per-group statistics for contiguous ranges of groups in parallel. Each range finds
    // where its first group starts in every piece, then walks all pieces forward in step. The
    // latencies are freed before any text exists: the output's page cache counts as memory.
    let ranges = threads;
    let stats: Vec<Vec<[u32; 6]>> = std::thread::scope(|scope| {
        let workers: Vec<_> = (0..ranges)
            .map(|k| {
                let pieces = &pieces;
                scope.spawn(move || {
                    let (first, last) = (
                        (groups * k / ranges) as u32,
                        (groups * (k + 1) / ranges) as u32,
                    );
                    let mut cursors: Vec<usize> = (pieces.iter())
                        .map(|(runs, _)| runs.partition_point(|run| run[0] < first))
                        .collect();
                    let (mut stats, mut values) = (Vec::new(), Vec::new());
                    for g in first..last {
                        values.clear();
                        let mut errors = 0;
                        for ((runs, lats), cursor) in pieces.iter().zip(cursors.iter_mut()) {
                            if let Some(run) = runs.get(*cursor).filter(|run| run[0] == g) {
                                values.extend_from_slice(&lats[run[1] as usize..run[2] as usize]);
                                errors += run[3];
                                *cursor += 1;
                            }
                        }
                        if values.is_empty() {
                            continue;
                        }
                        // Nearest rank: the ceil(p * n / 100)-th smallest. After selecting rank r, the
                        // values before it are the r smallest, so each lower rank is selected there.
                        let n = values.len();
                        let rank = |p: usize| (p * n).div_ceil(100) - 1;
                        let p99 = *values.select_nth_unstable(rank(99)).1;
                        let p95 = *values[..=rank(99)].select_nth_unstable(rank(95)).1;
                        let p50 = *values[..=rank(95)].select_nth_unstable(rank(50)).1;
                        stats.push([g, n as u32, p50, p95, p99, errors]);
                    }
                    stats
                })
            })
            .collect();
        workers.into_iter().map(|w| w.join().unwrap()).collect()
    });
    drop(pieces);
    drop(shards);

    // Phase 5: the text, formatted in parallel, then written in order, each part freed once written.
    let hour_text: Vec<String> = (lo..=hi)
        .map(|h| {
            let hour = h as i64 - HOUR_BIAS + base_hour;
            let (y, m, d) = civil_from_days(hour.div_euclid(24));
            format!("{y:04}-{m:02}-{d:02}T{:02}:00:00Z ", hour.rem_euclid(24))
        })
        .collect();
    let parts: Vec<Vec<u8>> = std::thread::scope(|scope| {
        let workers: Vec<_> = (stats.iter())
            .map(|stats| {
                let (names, hour_text) = (&names, &hour_text);
                scope.spawn(move || {
                    let mut text = Vec::new();
                    for &[g, n, p50, p95, p99, errors] in stats {
                        let (g, n, errors) = (g as usize, n as u64, errors as u64);
                        text.extend_from_slice(hour_text[g / names.len()].as_bytes());
                        text.extend_from_slice(names[g % names.len()]);
                        let rate = errors * 1_000_000 / n;
                        for value in [n, p50 as u64, p95 as u64, p99 as u64, errors, rate] {
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
    drop(stats);
    for text in parts {
        out.write_all(&text).expect("cannot write output");
    }
}
