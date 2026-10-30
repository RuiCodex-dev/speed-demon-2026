//! Speed Demon 2026, Track C (Join).
//!
//! 1. `regions.csv` becomes a dense array: region id -> country index (countries sorted bytewise).
//! 2. `records.csv` is memory-mapped and cut into chunks. Quotes only occur in quoted fields and
//!    `""` escapes come in pairs (spec 2.1), so a byte is inside quotes exactly when an odd number of
//!    `"` precede it: a parallel quote count plus a prefix parity tells every chunk where it starts,
//!    and the chunk then begins after its first record terminator outside quotes.
//! 3. Each thread parses its records with a byte state machine and keeps per-country totals.
//! 4. Distinct names per country, exactly and in parallel: every name belongs to the partition
//!    `hash % threads`. Threads intern names locally and file both their new names and their
//!    (country, name) pairs under that partition; then each partition's owner merges only its own
//!    names from all threads and counts them with one small bitset per country.
use std::collections::HashMap;
use std::hash::{BuildHasher, BuildHasherDefault, Hasher};
use std::io::Write;
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

/// FxHash: one rotate, xor and multiply per 8 bytes.
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
type FxMap<K, V> = HashMap<K, V, BuildHasherDefault<Fx>>;

const BOM: &[u8] = b"\xEF\xBB\xBF";

#[derive(Clone, Copy)]
struct Totals {
    count: u64,
    id_sum: u64,
    amount_sum: i64,
    amount_min: i64,
    amount_max: i64,
    name_bytes: u64,
    note_bytes: u64,
}
const EMPTY: Totals = Totals {
    count: 0,
    id_sum: 0,
    amount_sum: 0,
    amount_min: i64::MAX,
    amount_max: i64::MIN,
    name_bytes: 0,
    note_bytes: 0,
};

struct Shard {
    totals: Vec<Totals>,
    /// Name -> thread-local id.
    names: FxMap<&'static [u8], u32>,
    /// Partition of each local id.
    partition_of: Vec<u8>,
    /// Per partition: (local id, name) of the names this thread met first.
    new_names: Vec<Vec<(u32, &'static [u8])>>,
    /// Per partition: (country, local name id) for every record.
    pairs: Vec<Vec<(u16, u32)>>,
    /// Local id -> id inside its partition, written by that partition's owner only.
    partition_id: Vec<AtomicU32>,
}

/// Reads a text field starting at `*p` and leaves `*p` on the byte after it (`,` or a terminator).
/// Returns the raw bytes between the quotes (or the unquoted value) and the number of `""` pairs.
fn text_field(data: &'static [u8], p: &mut usize) -> (&'static [u8], usize) {
    if data[*p] != b'"' {
        let start = *p;
        while data[*p] != b',' {
            *p += 1;
        }
        return (&data[start..*p], 0);
    }
    let start = *p + 1;
    let mut i = start;
    let mut escapes = 0;
    loop {
        while data[i] != b'"' {
            i += 1;
        }
        if data.get(i + 1) == Some(&b'"') {
            escapes += 1;
            i += 2;
        } else {
            *p = i + 1;
            return (&data[start..i], escapes);
        }
    }
}

fn integer(data: &[u8], p: &mut usize) -> u64 {
    let mut value = 0;
    while data[*p].is_ascii_digit() {
        value = value * 10 + (data[*p] - b'0') as u64;
        *p += 1;
    }
    value
}

fn unescape(raw: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(raw.len());
    let mut i = 0;
    while i < raw.len() {
        out.push(raw[i]);
        i += if raw[i] == b'"' { 2 } else { 1 };
    }
    out
}

/// Parses whole records in `data[start..end]` (both on record boundaries) into `shard`.
fn parse(
    data: &'static [u8],
    start: usize,
    end: usize,
    country_of: &[u16],
    unmatched: u16,
    shard: &mut Shard,
) {
    let parts = shard.pairs.len() as u64;
    let mut p = start;
    while p < end {
        if data[p..].starts_with(BOM) {
            p += BOM.len();
        }
        let id = integer(data, &mut p);
        p += 1;
        let (name, name_escapes) = text_field(data, &mut p);
        p += 1;
        let (note, note_escapes) = text_field(data, &mut p);
        p += 1;
        let region = integer(data, &mut p) as usize;
        p += 1;
        let negative = data[p] == b'-';
        p += negative as usize;
        let whole = integer(data, &mut p) as i64;
        let cents = whole * 100 + ((data[p + 1] - b'0') * 10 + (data[p + 2] - b'0')) as i64;
        let amount = if negative { -cents } else { cents };
        p += 3;
        p += if data.get(p) == Some(&b'\r') { 2 } else { 1 };

        let country = country_of.get(region).copied().unwrap_or(unmatched);
        let t = &mut shard.totals[country as usize];
        t.count += 1;
        t.id_sum += id;
        t.amount_sum += amount;
        t.amount_min = t.amount_min.min(amount);
        t.amount_max = t.amount_max.max(amount);
        t.name_bytes += (name.len() - name_escapes) as u64;
        t.note_bytes += (note.len() - note_escapes) as u64;
        // A name with `""` is stored unescaped; that copy is rare and lives until exit.
        let key: &'static [u8] = if name_escapes == 0 {
            name
        } else {
            Box::leak(unescape(name).into_boxed_slice())
        };
        let next = shard.partition_of.len() as u32;
        let local = *shard.names.entry(key).or_insert_with(|| {
            let part = (BuildHasherDefault::<Fx>::default().hash_one(key) % parts) as usize;
            shard.partition_of.push(part as u8);
            shard.new_names[part].push((next, key));
            next
        });
        shard.pairs[shard.partition_of[local as usize] as usize].push((country, local));
    }
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

fn push_money(out: &mut Vec<u8>, cents: i64) {
    if cents < 0 {
        out.push(b'-');
    }
    let abs = cents.unsigned_abs();
    push_num(out, abs / 100);
    out.extend_from_slice(&[b'.', b'0' + (abs % 100 / 10) as u8, b'0' + (abs % 10) as u8]);
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let data_dir = args.get(1).map_or("/data", |s| s.as_str());
    let out_dir = args.get(2).map_or("/out", |s| s.as_str());
    let threads = std::thread::available_parallelism()
        .map_or(1, |n| n.get())
        .min(255);

    // regions.csv: plain `region_id,country` lines after a header, in one pass. Country codes get
    // temporary ids in order of appearance, which are then renumbered in sorted order.
    let regions = map_file(&format!("{data_dir}/regions.csv"));
    let mut seen: FxMap<&[u8], u16> = FxMap::default();
    let mut region_code: Vec<u16> = Vec::new();
    for line in regions
        .split(|&b| b == b'\n')
        .skip(1)
        .filter(|line| !line.is_empty())
    {
        let comma = line
            .iter()
            .position(|&b| b == b',')
            .expect("bad regions.csv line");
        let region = line[..comma]
            .iter()
            .fold(0, |acc, &b| acc * 10 + (b - b'0') as usize);
        let next = seen.len() as u16;
        let code = *seen.entry(&line[comma + 1..]).or_insert(next);
        if region >= region_code.len() {
            region_code.resize(region + 1, u16::MAX);
        }
        region_code[region] = code;
    }
    let mut keys: Vec<&[u8]> = seen.keys().copied().collect();
    keys.sort_unstable();
    let unmatched = keys.len() as u16;
    let rank: Vec<u16> = {
        let mut rank = vec![0u16; keys.len()];
        for (sorted, key) in keys.iter().enumerate() {
            rank[seen[key] as usize] = sorted as u16;
        }
        rank
    };
    let country_of: Vec<u16> = region_code
        .iter()
        .map(|&c| {
            if c == u16::MAX {
                unmatched
            } else {
                rank[c as usize]
            }
        })
        .collect();
    keys.push(b"_UNMATCHED"); // `_` sorts after every [A-Z] code, so it stays last

    // records.csv: skip the BOM and the header, then cut the body into chunks.
    let data = map_file(&format!("{data_dir}/records.csv"));
    let mut body = if data.starts_with(BOM) { BOM.len() } else { 0 };
    body += data[body..]
        .iter()
        .position(|&b| b == b'\n')
        .map_or(data.len() - body, |i| i + 1);
    let parts = threads * 8;
    let cuts: Vec<usize> = (0..=parts)
        .map(|i| body + (data.len() - body) * i / parts)
        .collect();
    let quotes: Vec<usize> = std::thread::scope(|scope| {
        let workers: Vec<_> = (0..threads)
            .map(|k| {
                let cuts = &cuts;
                scope.spawn(move || {
                    (parts * k / threads..parts * (k + 1) / threads)
                        .map(|i| {
                            data[cuts[i]..cuts[i + 1]]
                                .iter()
                                .filter(|&&b| b == b'"')
                                .count()
                        })
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        workers
            .into_iter()
            .flat_map(|w| w.join().unwrap())
            .collect()
    });
    // Each cut moves forward to just after the first terminator outside quotes.
    let mut starts = vec![body];
    let mut parity = 0;
    for i in 1..parts {
        parity += quotes[i - 1];
        let mut inside = parity % 2 == 1;
        let mut p = cuts[i];
        while p < data.len() && (inside || data[p] != b'\n') {
            inside ^= data[p] == b'"';
            p += 1;
        }
        starts.push((p + 1).min(data.len()).max(*starts.last().unwrap()));
    }
    starts.push(data.len());

    let next = AtomicUsize::new(0);
    let shards: Vec<Shard> = std::thread::scope(|scope| {
        let workers: Vec<_> = (0..threads)
            .map(|_| {
                scope.spawn(|| {
                    let mut shard = Shard {
                        totals: vec![EMPTY; keys.len()],
                        names: FxMap::default(),
                        partition_of: Vec::new(),
                        new_names: vec![Vec::new(); threads],
                        pairs: vec![Vec::new(); threads],
                        partition_id: Vec::new(),
                    };
                    loop {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        if i >= parts {
                            break;
                        }
                        parse(
                            data,
                            starts[i],
                            starts[i + 1],
                            &country_of,
                            unmatched,
                            &mut shard,
                        );
                    }
                    shard.partition_id = (0..shard.partition_of.len())
                        .map(|_| AtomicU32::new(0))
                        .collect();
                    shard
                })
            })
            .collect();
        workers.into_iter().map(|w| w.join().unwrap()).collect()
    });

    // Distinct names, one partition per thread: merge the partition's names from every shard, then
    // set one bit per (country, name) and count. Returns the count per country.
    let distinct: Vec<Vec<u32>> = std::thread::scope(|scope| {
        let workers: Vec<_> = (0..threads)
            .map(|part| {
                let (shards, keys) = (&shards, &keys);
                scope.spawn(move || {
                    let mut ids: FxMap<&[u8], u32> = FxMap::default();
                    for shard in shards {
                        for &(local, name) in &shard.new_names[part] {
                            let next = ids.len() as u32;
                            shard.partition_id[local as usize]
                                .store(*ids.entry(name).or_insert(next), Ordering::Relaxed);
                        }
                    }
                    let words = ids.len().div_ceil(64);
                    let mut bits = vec![0u64; words * keys.len()];
                    for shard in shards {
                        for &(country, local) in &shard.pairs[part] {
                            let id =
                                shard.partition_id[local as usize].load(Ordering::Relaxed) as usize;
                            bits[country as usize * words + id / 64] |= 1 << (id % 64);
                        }
                    }
                    bits.chunks(words.max(1))
                        .map(|c| c.iter().map(|w| w.count_ones()).sum())
                        .collect::<Vec<u32>>()
                })
            })
            .collect();
        workers.into_iter().map(|w| w.join().unwrap()).collect()
    });

    let mut out = Vec::new();
    for (c, key) in keys.iter().enumerate() {
        let mut t = EMPTY;
        for shard in &shards {
            let s = &shard.totals[c];
            t.count += s.count;
            t.id_sum += s.id_sum;
            t.amount_sum += s.amount_sum;
            t.amount_min = t.amount_min.min(s.amount_min);
            t.amount_max = t.amount_max.max(s.amount_max);
            t.name_bytes += s.name_bytes;
            t.note_bytes += s.note_bytes;
        }
        if t.count == 0 {
            continue;
        }
        let names: u32 = distinct
            .iter()
            .map(|d| d.get(c).copied().unwrap_or(0))
            .sum();
        out.extend_from_slice(key);
        for value in [t.count, t.id_sum] {
            out.push(b' ');
            push_num(&mut out, value);
        }
        for cents in [t.amount_sum, t.amount_min, t.amount_max] {
            out.push(b' ');
            push_money(&mut out, cents);
        }
        for value in [names as u64, t.name_bytes, t.note_bytes] {
            out.push(b' ');
            push_num(&mut out, value);
        }
        out.push(b'\n');
    }
    std::fs::File::create(format!("{out_dir}/aggregates.txt"))
        .and_then(|mut f| f.write_all(&out))
        .expect("cannot write output");
}
