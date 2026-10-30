//! Speed Demon 2026, Track C (Join).
//!
//! 1. `regions.csv` becomes a dense array: region id -> country index (countries sorted bytewise).
//! 2. `records.csv` is memory-mapped and cut into chunks. Quotes only occur in quoted fields and
//!    `""` escapes come in pairs (spec 2.1), so a byte is inside quotes exactly when an odd number of
//!    `"` precede it: a parallel quote count plus a prefix parity tells every chunk where it starts,
//!    and the chunk then begins after its first record terminator outside quotes.
//! 3. Each thread parses its records with a byte state machine and keeps per-country totals.
//! 4. Distinct names: one lock-free table shared by all threads maps each name to an id. It keeps a
//!    copy of every distinct name in a compact arena, so the exact byte comparison reads memory that
//!    stays in cache, never the 200 MB input. Each record then sets the bit (country, id).
//!    Nothing is stored per record.
use std::collections::HashMap;
use std::hash::{BuildHasherDefault, Hasher};
use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

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

const BOM: &[u8] = b"\xEF\xBB\xBF";

/// The 8 bytes of `name` starting at `i`, bytes past its end masked to zero. Every name passed here
/// is followed by at least 8 readable bytes: in the input by the rest of its record (`","...,0.00` and
/// a terminator), in the arena and in unescaped copies by explicit padding.
fn word(name: &[u8], i: usize) -> u64 {
    // SAFETY: see above.
    let w = unsafe { std::ptr::read_unaligned(name.as_ptr().add(i) as *const u64) };
    if name.len() - i >= 8 {
        w
    } else {
        w & ((1 << (8 * (name.len() - i))) - 1)
    }
}

/// FxHash-style hash of a name, 8 bytes per step, without copying.
fn hash(name: &[u8]) -> u64 {
    (0..name.len()).step_by(8).fold(name.len() as u64, |h, i| {
        (h.rotate_left(5) ^ word(name, i)).wrapping_mul(0x517cc1b727220a95)
    })
}

/// Byte equality of two names, 8 bytes at a time.
fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && (0..a.len()).step_by(8).all(|i| word(a, i) == word(b, i))
}

/// Index of the first `"` at or after `i`, testing 8 bytes at a time where 8 bytes remain.
fn find_quote(s: &[u8], mut i: usize) -> usize {
    while let Some(bytes) = s.get(i..i + 8) {
        let w = u64::from_le_bytes(bytes.try_into().unwrap()) ^ 0x2222_2222_2222_2222;
        let zero = w.wrapping_sub(0x0101_0101_0101_0101) & !w & 0x8080_8080_8080_8080;
        if zero != 0 {
            return i + zero.trailing_zeros() as usize / 8; // the lowest zero byte is exact
        }
        i += 8;
    }
    i + s[i..].iter().position(|&b| b == b'"').unwrap()
}

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

/// Name -> id, shared by all threads without locks. A slot holds `tag << 32 | (id + 1)` (0 = empty),
/// where `tag` is the high half of the name's hash; `spans[id]` says where the arena holds the
/// name's bytes. All three arrays are zero-allocated, so pages the run never touches cost no memory.
struct Names {
    slots: Vec<AtomicU64>,
    spans: Vec<AtomicU64>,
    /// Start of a zeroed buffer as long as the input, leaked: every name fits in it.
    arena: *mut u8,
    next_id: AtomicUsize,
    arena_used: AtomicUsize,
    /// Set when the table is too full to be trusted; the run is then repeated with a bigger one.
    full: AtomicBool,
}

// SAFETY: the arena is written only in ranges handed out by `arena_used.fetch_add`, each to one
// thread, and read only after the span that names the range was published with Release.
unsafe impl Sync for Names {}

/// A zeroed vector of atomics straight from `calloc`, so its pages cost memory only once written.
fn zeroed_atomics(n: usize) -> Vec<AtomicU64> {
    let mut v = std::mem::ManuallyDrop::new(vec![0u64; n]);
    // SAFETY: AtomicU64 has the same size and alignment as u64, and zero is a valid value.
    unsafe { Vec::from_raw_parts(v.as_mut_ptr() as *mut AtomicU64, v.len(), v.capacity()) }
}

impl Names {
    fn new(capacity: usize, arena_bytes: usize) -> Names {
        Names {
            slots: zeroed_atomics(capacity),
            spans: zeroed_atomics(capacity),
            arena: vec![0u8; arena_bytes.max(1)].leak().as_mut_ptr(),
            next_id: AtomicUsize::new(0),
            arena_used: AtomicUsize::new(0),
            full: AtomicBool::new(false),
        }
    }

    fn bytes(&self, id: usize) -> &[u8] {
        let span = self.spans[id].load(Ordering::Acquire);
        // SAFETY: the span was published after its bytes were written (see `id`).
        unsafe {
            std::slice::from_raw_parts(
                self.arena.add((span >> 16) as usize),
                (span & 0xFFFF) as usize,
            )
        }
    }

    fn id(&self, name: &[u8]) -> usize {
        let h = hash(name);
        let (tag, mask) = (h >> 32, self.slots.len() - 1);
        let mut i = h as usize & mask;
        loop {
            let mut slot = self.slots[i].load(Ordering::Acquire);
            if slot == 0 {
                // Claim an id and arena space, publish the bytes, then try to take the slot.
                let id = self.next_id.fetch_add(1, Ordering::Relaxed);
                if id >= self.slots.len() / 2 {
                    self.full.store(true, Ordering::Relaxed);
                    return 0;
                }
                let at = self.arena_used.fetch_add(name.len() + 8, Ordering::Relaxed); // + padding for `word`
                // SAFETY: `at..at + len` was handed to this thread alone by the fetch_add above, and
                // nobody reads it before the span below is published with Release.
                unsafe {
                    std::ptr::copy_nonoverlapping(name.as_ptr(), self.arena.add(at), name.len())
                };
                self.spans[id].store((at as u64) << 16 | name.len() as u64, Ordering::Release);
                match self.slots[i].compare_exchange(
                    0,
                    tag << 32 | (id as u64 + 1),
                    Ordering::AcqRel,
                    Ordering::Acquire,
                ) {
                    Ok(_) => return id,
                    Err(other) => slot = other, // another thread won this slot; its id stays unused
                }
            }
            if slot >> 32 == tag && same(self.bytes((slot as u32 - 1) as usize), name) {
                return (slot as u32 - 1) as usize;
            }
            i = (i + 1) & mask;
        }
    }
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
        i = find_quote(data, i);
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

/// The unescaped value of a quoted field (`""` -> `"`), followed by 8 bytes of padding for `word`;
/// the returned length excludes the padding.
fn unescape(raw: &[u8]) -> (Vec<u8>, usize) {
    let mut out = Vec::with_capacity(raw.len() + 8);
    let mut i = 0;
    while i < raw.len() {
        out.push(raw[i]);
        i += if raw[i] == b'"' { 2 } else { 1 };
    }
    let len = out.len();
    out.extend_from_slice(&[0; 8]);
    (out, len)
}

/// Parses whole records in `data[start..end]` (both on record boundaries): totals into `totals`,
/// one bit per (country, name id) into `seen`, which has `words` words per country.
fn parse(
    data: &'static [u8],
    start: usize,
    end: usize,
    country_of: &[u16],
    unmatched: u16,
    totals: &mut [Totals],
    names: &Names,
    seen: &[AtomicU64],
    words: usize,
) {
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
        let t = &mut totals[country as usize];
        t.count += 1;
        t.id_sum += id;
        t.amount_sum += amount;
        t.amount_min = t.amount_min.min(amount);
        t.amount_max = t.amount_max.max(amount);
        t.name_bytes += (name.len() - name_escapes) as u64;
        t.note_bytes += (note.len() - note_escapes) as u64;
        let name_id = if name_escapes == 0 {
            names.id(name)
        } else {
            {
                let (copy, len) = unescape(name);
                names.id(&copy[..len])
            }
        };
        seen[country as usize * words + name_id / 64]
            .fetch_or(1 << (name_id % 64), Ordering::Relaxed);
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
    let threads = std::thread::available_parallelism().map_or(1, |n| n.get());

    // regions.csv: plain `region_id,country` lines after a header, in one pass. Country codes get
    // temporary ids in order of appearance, which are then renumbered in sorted order.
    let regions = map_file(&format!("{data_dir}/regions.csv"));
    let mut seen_codes: HashMap<&[u8], u16, BuildHasherDefault<Fx>> = HashMap::default();
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
        let next = seen_codes.len() as u16;
        let code = *seen_codes.entry(&line[comma + 1..]).or_insert(next);
        if region >= region_code.len() {
            region_code.resize(region + 1, u16::MAX);
        }
        region_code[region] = code;
    }
    let mut keys: Vec<&[u8]> = seen_codes.keys().copied().collect();
    keys.sort_unstable();
    let unmatched = keys.len() as u16;
    let mut rank = vec![0u16; keys.len()];
    for (sorted, key) in keys.iter().enumerate() {
        rank[seen_codes[key] as usize] = sorted as u16;
    }
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

    // The name table starts at 2^18 slots (half of them usable), which the spec's pool of 100,000
    // names fits; if a dataset has more distinct names, the run is repeated with 4 times as many.
    let mut capacity = 1 << 18;
    let (totals, names, seen, words) = loop {
        let names = Names::new(capacity, data.len() * 2); // names + 8 bytes of padding each
        let words = capacity / 2 / 64;
        let seen = zeroed_atomics(words * keys.len());
        let next = AtomicUsize::new(0);
        let totals: Vec<Vec<Totals>> = std::thread::scope(|scope| {
            let workers: Vec<_> = (0..threads)
                .map(|_| {
                    scope.spawn(|| {
                        let mut totals = vec![EMPTY; keys.len()];
                        while let Some(w) =
                            starts.windows(2).nth(next.fetch_add(1, Ordering::Relaxed))
                        {
                            parse(
                                data,
                                w[0],
                                w[1],
                                &country_of,
                                unmatched,
                                &mut totals,
                                &names,
                                &seen,
                                words,
                            );
                        }
                        totals
                    })
                })
                .collect();
            workers.into_iter().map(|w| w.join().unwrap()).collect()
        });
        if !names.full.load(Ordering::Relaxed) {
            break (totals, names, seen, words);
        }
        capacity *= 4;
    };
    let used_words = names.next_id.load(Ordering::Relaxed).div_ceil(64);

    let mut out = Vec::new();
    for (c, key) in keys.iter().enumerate() {
        let mut t = EMPTY;
        for s in totals.iter().map(|thread| &thread[c]) {
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
        let distinct: u32 = seen[c * words..c * words + used_words]
            .iter()
            .map(|w| w.load(Ordering::Relaxed).count_ones())
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
        for value in [distinct as u64, t.name_bytes, t.note_bytes] {
            out.push(b' ');
            push_num(&mut out, value);
        }
        out.push(b'\n');
    }
    std::fs::File::create(format!("{out_dir}/aggregates.txt"))
        .and_then(|mut f| f.write_all(&out))
        .expect("cannot write output");
}
