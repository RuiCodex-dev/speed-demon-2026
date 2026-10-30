//! Speed Demon 2026, Track B (Index).
//!
//! The queries are known before the corpus is read, so only their terms are indexed:
//! 1. Parse `queries.txt` and intern every distinct term.
//! 2. Scan `docs.txt` in parallel chunks of whole lines. Each token is lowercased, looked up among the
//!    query terms, and a (term, doc) posting is recorded once per document for terms BOOL queries use.
//!    Phrases are matched in the same pass: an Aho-Corasick automaton over term ids finds every phrase
//!    query in a document's token stream, so a phrase answer is a ready-made list.
//! 3. A counting sort over the chunks, in chunk order, makes every list sorted by doc id.
//!    Terms that a sample shows to be frequent get a bitmap over all documents instead of a list.
//! 4. BOOL queries are sorted-list merges applied left to right, or bitmap operations when a term
//!    is stored as a bitmap.
use std::collections::{HashMap, VecDeque};
use std::hash::{BuildHasherDefault, Hasher};
use std::io::Write;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

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

/// Maps each byte to its lowercased form if it can be part of a token ([A-Za-z0-9]), else to 0.
static TOKEN_BYTE: [u8; 256] = {
    let mut table = [0u8; 256];
    let mut i = 0;
    while i < 256 {
        if (i as u8).is_ascii_alphanumeric() {
            table[i] = (i as u8).to_ascii_lowercase();
        }
        i += 1;
    }
    table
};

fn step_hash(h: u64, c: u8) -> u64 {
    (h.rotate_left(5) ^ c as u64).wrapping_mul(0x517cc1b727220a95)
}

/// Calls `f(token, hash)` for every token of `text`: maximal runs of [a-z0-9] after ASCII
/// lowercasing (every byte >= 0x80 is a separator). The hash is computed while scanning. A token
/// longer than 32 bytes cannot be a query term and is passed as empty.
fn tokens(text: &[u8], mut f: impl FnMut(&[u8], u64)) {
    let (mut buf, mut len, mut h) = ([0u8; 40], 0usize, 0u64); // 8 spare bytes for `word`
    for &b in text.iter().chain(std::iter::once(&b' ')) {
        let c = TOKEN_BYTE[b as usize];
        if c != 0 {
            if len < 32 {
                buf[len] = c;
            }
            len += 1;
            h = step_hash(h, c);
        } else if len > 0 {
            f(if len <= 32 { &buf[..len] } else { &[] }, h);
            (len, h) = (0, 0);
        }
    }
}

/// The 8 bytes of `s` starting at `i`, bytes past its end masked to zero. `s` is always followed by
/// at least 8 readable bytes: the tokenizer's 40-byte buffer holds tokens of at most 32 bytes, and
/// every word in the vocabulary arena is followed by 8 bytes of padding.
fn word(s: &[u8], i: usize) -> u64 {
    // SAFETY: see above.
    let w = unsafe { std::ptr::read_unaligned(s.as_ptr().add(i) as *const u64) };
    if s.len() - i >= 8 {
        w
    } else {
        w & ((1 << (8 * (s.len() - i))) - 1)
    }
}

/// The query terms in an open-addressing table keyed by the tokenizer's own hash. A slot packs
/// `tag:24 | length:8 | id + 1:32` (0 = empty), so most mismatches are rejected without leaving the
/// table; a candidate is then compared 8 bytes at a time with its copy in a padded arena.
struct Vocab {
    /// One bit per value of the hash's low 21 bits, set for every term: a 256 KB filter that stays
    /// in cache and turns away most tokens that are not query terms before they touch `slots`.
    filter: Vec<u64>,
    slots: Vec<u64>,
    arena: Vec<u8>,
    starts: Vec<u32>,
    shift: u32,
}

impl Vocab {
    /// `words[id]` is the term with that id.
    fn new(words: &[&[u8]]) -> Vocab {
        let bits = (words.len() * 2)
            .next_power_of_two()
            .trailing_zeros()
            .max(1);
        let mut vocab = Vocab {
            filter: vec![0; 1 << 15],
            slots: vec![0; 1 << bits],
            arena: Vec::new(),
            starts: Vec::new(),
            shift: 64 - bits,
        };
        for (id, word) in words.iter().enumerate() {
            let h = word.iter().fold(0, |h, &c| step_hash(h, c));
            vocab.filter[(h as usize >> 6) & ((1 << 15) - 1)] |= 1 << (h % 64);
            let mut i = (h >> vocab.shift) as usize;
            while vocab.slots[i] != 0 {
                i = (i + 1) & (vocab.slots.len() - 1);
            }
            vocab.slots[i] = (h & 0xFF_FFFF) << 40 | (word.len() as u64) << 32 | (id as u64 + 1);
            vocab.starts.push(vocab.arena.len() as u32);
            vocab.arena.extend_from_slice(word);
            vocab.arena.extend_from_slice(&[0; 8]);
        }
        vocab
    }

    fn get(&self, token: &[u8], h: u64) -> Option<u32> {
        if self.filter[(h as usize >> 6) & ((1 << 15) - 1)] >> (h % 64) & 1 == 0 {
            return None;
        }
        let key = (h & 0xFF_FFFF) << 40 | (token.len() as u64) << 32;
        let mut i = (h >> self.shift) as usize;
        loop {
            let slot = self.slots[i];
            if slot == 0 {
                return None;
            }
            if slot >> 32 == key >> 32 {
                let id = (slot as u32 - 1) as usize;
                let start = self.starts[id] as usize;
                let known = &self.arena[start..start + token.len()];
                if (0..token.len())
                    .step_by(8)
                    .all(|k| word(known, k) == word(token, k))
                {
                    return Some(id as u32);
                }
            }
            i = (i + 1) & (self.slots.len() - 1);
        }
    }
}

enum Query {
    /// The first term, then (operator, term) pairs. Operators: b'A'nd, b'O'r, b'N'ot.
    Bool(u32, Vec<(u8, u32)>),
    /// Index of the phrase, whose matching documents come from the automaton.
    Phrase(u32),
}

/// Splits `data` into about `parts` pieces that each end with a newline.
fn split_lines(data: &[u8], parts: usize) -> Vec<&[u8]> {
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

fn intersect(a: &[u32], b: &[u32]) -> Vec<u32> {
    let (small, large) = if a.len() <= b.len() { (a, b) } else { (b, a) };
    let mut out = Vec::with_capacity(small.len());
    let mut lo = 0;
    for &x in small {
        // Galloping search: cheap when one list is much shorter than the other.
        let mut step = 1;
        while lo + step < large.len() && large[lo + step] < x {
            lo += step;
            step *= 2;
        }
        let hi = (lo + step + 1).min(large.len());
        lo += large[lo..hi].partition_point(|&y| y < x);
        if lo < large.len() && large[lo] == x {
            out.push(x);
        }
    }
    out
}

fn union(a: &[u32], b: &[u32]) -> Vec<u32> {
    let mut out = Vec::with_capacity(a.len() + b.len());
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        let (x, y) = (a[i], b[j]);
        out.push(x.min(y));
        i += (x <= y) as usize;
        j += (y <= x) as usize;
    }
    out.extend_from_slice(&a[i..]);
    out.extend_from_slice(&b[j..]);
    out
}

fn difference(a: &[u32], b: &[u32]) -> Vec<u32> {
    let mut out = Vec::with_capacity(a.len());
    let mut j = 0;
    for &x in a {
        j += b[j..].partition_point(|&y| y < x);
        if j >= b.len() || b[j] != x {
            out.push(x);
        }
    }
    out
}

/// Aho-Corasick automaton over term ids. State 0 is the root.
struct Phrases {
    /// Transitions from the root, indexed by term id (0 means stay at the root).
    root: Vec<u32>,
    /// Transitions from every other state, keyed by `state << 32 | term`.
    goto: FxMap<u64, u32>,
    fail: Vec<u32>,
    /// The nearest state on the failure chain, itself included, that ends a phrase (or u32::MAX).
    out: Vec<u32>,
    /// Bit `t % 64` is set if the state has a transition on a term `t`: most lookups that would
    /// fail are skipped without touching `goto`.
    child_mask: Vec<u64>,
}

impl Phrases {
    /// Builds the automaton and returns, for every phrase, the state that ends it.
    fn build(phrases: &[Vec<u32>], term_count: usize) -> (Phrases, Vec<u32>) {
        let mut a = Phrases {
            root: vec![0; term_count],
            goto: FxMap::default(),
            fail: vec![0],
            out: vec![u32::MAX],
            child_mask: vec![0],
        };
        let mut children: Vec<Vec<(u32, u32)>> = vec![Vec::new()];
        let mut terminal = vec![false];
        let ends = phrases
            .iter()
            .map(|phrase| {
                let mut s = 0u32;
                for &t in phrase {
                    let key = (s as u64) << 32 | t as u64;
                    let found = if s == 0 {
                        a.root[t as usize]
                    } else {
                        a.goto.get(&key).copied().unwrap_or(0)
                    };
                    s = if found != 0 {
                        found
                    } else {
                        let new = children.len() as u32;
                        children.push(Vec::new());
                        terminal.push(false);
                        children[s as usize].push((t, new));
                        a.child_mask.push(0);
                        a.child_mask[s as usize] |= 1 << (t % 64);
                        if s == 0 {
                            a.root[t as usize] = new;
                        } else {
                            a.goto.insert(key, new);
                        }
                        new
                    };
                }
                terminal[s as usize] = true;
                s
            })
            .collect();
        // Failure links in breadth-first order, so a state's failure target is always done first.
        a.fail = vec![0; children.len()];
        a.out = vec![u32::MAX; children.len()];
        let mut queue: VecDeque<u32> = children[0].iter().map(|&(_, c)| c).collect();
        while let Some(s) = queue.pop_front() {
            let s = s as usize;
            a.out[s] = if terminal[s] {
                s as u32
            } else {
                a.out[a.fail[s] as usize]
            };
            for &(t, c) in &children[s] {
                a.fail[c as usize] = if s == 0 { 0 } else { a.step(a.fail[s], t) };
                queue.push_back(c);
            }
        }
        (a, ends)
    }

    fn step(&self, mut s: u32, t: u32) -> u32 {
        loop {
            if s == 0 {
                return self.root[t as usize];
            }
            if self.child_mask[s as usize] >> (t % 64) & 1 == 1 {
                if let Some(&next) = self.goto.get(&((s as u64) << 32 | t as u64)) {
                    return next;
                }
            }
            s = self.fail[s as usize];
        }
    }
}

/// Counting sort of packed postings, `key << shift | (doc - base)` with keys in `lo..lo + keys`,
/// from chunks given in document order with their base document: each key's docs end up contiguous
/// and ascending. Returns every key's start offset and the docs.
fn group_by_key(
    chunks: &[(&[u32], u32)],
    lo: usize,
    keys: usize,
    shift: u32,
) -> (Vec<usize>, Vec<u32>) {
    let mut starts = vec![0usize; keys + 1];
    for (chunk, _) in chunks {
        for &p in chunk.iter() {
            starts[(p >> shift) as usize - lo + 1] += 1;
        }
    }
    for k in 0..keys {
        starts[k + 1] += starts[k];
    }
    let mut fill = starts.clone();
    let mut docs = vec![0u32; starts[keys]];
    for &(chunk, base) in chunks {
        for &p in chunk.iter() {
            let k = (p >> shift) as usize - lo;
            docs[fill[k]] = base + (p & ((1 << shift) - 1));
            fill[k] += 1;
        }
    }
    (starts, docs)
}

/// The id of the document on the first line of `lines`.
fn first_id(lines: &[u8]) -> u32 {
    lines
        .iter()
        .take_while(|b| b.is_ascii_digit())
        .fold(0, |a, &b| a * 10 + (b - b'0') as u32)
}

/// Splits chunks of whole lines further until each spans fewer than `limit` document ids.
fn limit_span<'a>(chunks: Vec<&'a [u8]>, limit: u32) -> Vec<&'a [u8]> {
    let mut out = Vec::new();
    for chunk in chunks {
        let last = chunk[..chunk.len() - 1]
            .iter()
            .rposition(|&b| b == NEWLINE)
            .map_or(0, |i| i + 1);
        if first_id(&chunk[last..]) - first_id(chunk) < limit || last == 0 {
            out.push(chunk);
        } else {
            out.extend(limit_span(split_lines(chunk, 2), limit));
        }
    }
    out
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

/// Calls `f(doc_id, text)` for every line of a piece of `docs.txt` that holds whole lines.
fn for_each_doc(lines: &[u8], mut f: impl FnMut(u32, &[u8])) {
    let mut p = 0;
    while p < lines.len() {
        let tab = p + lines[p..].iter().position(|&b| b == b'\t').unwrap();
        let end = tab + lines[tab..].iter().position(|&b| b == b'\n').unwrap();
        f(
            lines[p..tab]
                .iter()
                .fold(0u32, |acc, &b| acc * 10 + (b - b'0') as u32),
            &lines[tab + 1..end],
        );
        p = end + 1;
    }
}

/// Moves `pos` forward to the start of the next line (or the end of `data`).
fn line_start(data: &[u8], mut pos: usize) -> usize {
    while pos > 0 && pos < data.len() && data[pos - 1] != b'\n' {
        pos += 1;
    }
    pos.min(data.len())
}

const NEWLINE: u8 = 10;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let data_dir = args.get(1).map_or("/data", |s| s.as_str());
    let out_dir = args.get(2).map_or("/out", |s| s.as_str());
    let threads = std::thread::available_parallelism().map_or(1, |n| n.get());

    // 1. Queries and their vocabulary. Only terms used by a BOOL query need document sets.
    let query_text = map_file(&format!("{data_dir}/queries.txt"));
    let mut terms: FxMap<&'static [u8], u32> = FxMap::default();
    let mut in_bool: Vec<bool> = Vec::new();
    let mut intern = |word: &'static [u8], bool_term: bool| {
        let next = terms.len() as u32;
        let t = *terms.entry(word).or_insert(next);
        if t as usize == in_bool.len() {
            in_bool.push(false);
        }
        in_bool[t as usize] |= bool_term;
        t
    };
    let mut queries = Vec::new();
    let mut phrases: Vec<Vec<u32>> = Vec::new();
    for line in query_text.split(|&b| b == b'\n').filter(|l| !l.is_empty()) {
        if let Some(rest) = line.strip_prefix(b"PHRASE \"") {
            let words = &rest[..rest.len() - 1];
            queries.push(Query::Phrase(phrases.len() as u32));
            phrases.push(
                words
                    .split(|&b| b == b' ')
                    .map(|w| intern(w, false))
                    .collect(),
            );
        } else {
            let mut words = line[5..].split(|&b| b == b' ');
            let first = intern(words.next().unwrap(), true);
            let mut ops = Vec::new();
            while let (Some(op), Some(term)) = (words.next(), words.next()) {
                ops.push((op[0], intern(term, true)));
            }
            queries.push(Query::Bool(first, ops));
        }
    }
    let term_count = terms.len();
    let mut words = vec![&b""[..]; term_count];
    for (&word, &id) in &terms {
        words[id as usize] = word;
    }
    let vocab = Vocab::new(&words);
    let (automaton, phrase_end) = Phrases::build(&phrases, term_count);
    let states = automaton.fail.len();

    // 2. Frequent BOOL terms get a bitmap over all documents instead of a posting list. A sample of
    //    about 1/64 of the corpus, spread over the file, estimates each term's document frequency;
    //    a wrong guess changes only the representation, never an answer.
    let docs = map_file(&format!("{data_dir}/docs.txt"));
    let last_line = docs[..docs.len().saturating_sub(1)]
        .iter()
        .rposition(|&b| b == b'\n')
        .map_or(0, |i| i + 1);
    let doc_count = if docs.is_empty() {
        0
    } else {
        docs[last_line..]
            .iter()
            .take_while(|b| b.is_ascii_digit())
            .fold(0, |acc, &b| acc * 10 + (b - b'0') as usize)
            + 1
    };
    let words = doc_count.div_ceil(64);
    let samples: Vec<(u32, Vec<u32>)> = std::thread::scope(|scope| {
        let workers: Vec<_> = (0..threads)
            .map(|k| {
                let (vocab, in_bool) = (&vocab, &in_bool);
                scope.spawn(move || {
                    let start = line_start(docs, docs.len() * k / threads);
                    let end = line_start(docs, start + docs.len() / threads / 64);
                    let (mut seen_docs, mut df, mut last) =
                        (0u32, vec![0u32; term_count], vec![u32::MAX; term_count]);
                    for_each_doc(&docs[start..end], |doc, text| {
                        seen_docs += 1;
                        tokens(text, |token, h| {
                            if let Some(t) = vocab
                                .get(token, h)
                                .filter(|&t| in_bool[t as usize] && last[t as usize] != doc)
                            {
                                last[t as usize] = doc;
                                df[t as usize] += 1;
                            }
                        });
                    });
                    (seen_docs, df)
                })
            })
            .collect();
        workers.into_iter().map(|w| w.join().unwrap()).collect()
    });
    let sampled_docs: u32 = samples.iter().map(|s| s.0).sum();
    let mut dense = vec![u32::MAX; term_count];
    let mut dense_count = 0;
    for t in 0..term_count {
        // A bitmap costs doc_count / 8 bytes; a posting costs 4 bytes, and 8 while it is collected.
        if samples.iter().map(|s| s.1[t]).sum::<u32>() * 64 >= sampled_docs.max(1) {
            dense[t] = dense_count;
            dense_count += 1;
        }
    }
    let bitmaps: Vec<AtomicU64> = (0..dense_count as usize * words)
        .map(|_| AtomicU64::new(0))
        .collect();

    // 3. Parallel corpus scan: bitmap bits or postings for BOOL terms, and phrase matches, each
    //    once per document.
    // Postings and phrase matches are packed as `key << shift | (doc - first doc of the chunk)` in a
    // u32, so every chunk must span fewer document ids than the bits left over allow.
    let bits = |keys: usize| usize::BITS - keys.leading_zeros();
    let (term_shift, state_shift) = (32 - bits(term_count), 32 - bits(states));
    assert!(
        term_shift.min(state_shift) >= 8,
        "too many query terms to pack postings"
    );
    let chunks = limit_span(
        split_lines(docs, threads * 16),
        1 << term_shift.min(state_shift),
    );
    let next = AtomicUsize::new(0);
    let mut results: Vec<(usize, u32, Vec<Vec<u32>>, Vec<u32>)> = std::thread::scope(|scope| {
        let workers: Vec<_> = (0..threads)
            .map(|_| {
                scope.spawn(|| {
                    let mut done = Vec::new();
                    let mut last_doc = vec![u32::MAX; term_count];
                    let mut last_match = vec![u32::MAX; states];
                    loop {
                        let c = next.fetch_add(1, Ordering::Relaxed);
                        let Some(chunk) = chunks.get(c) else { break };
                        // Postings are filed by term range, one range per thread for the sort.
                        let (mut postings, mut matches) = (vec![Vec::new(); threads], Vec::new());
                        // Bits for frequent terms go to a bitmap local to this chunk's documents
                        // and are merged with one atomic OR per word at the end.
                        let last = chunk[..chunk.len() - 1]
                            .iter()
                            .rposition(|&b| b == NEWLINE)
                            .map_or(0, |i| i + 1);
                        let first_doc = first_id(chunk);
                        let (base, span) = (
                            first_doc as usize / 64,
                            first_id(&chunk[last..]) as usize / 64 + 1 - first_doc as usize / 64,
                        );
                        let mut local = vec![0u64; dense_count as usize * span];
                        for_each_doc(chunk, |doc, text| {
                            let mut state = 0;
                            tokens(text, |token, h| {
                                let Some(t) = vocab.get(token, h) else {
                                    state = 0; // no phrase contains a word that is not a query term
                                    return;
                                };
                                if in_bool[t as usize] && last_doc[t as usize] != doc {
                                    last_doc[t as usize] = doc;
                                    match dense[t as usize] {
                                        u32::MAX => postings[t as usize * threads / term_count]
                                            .push(t << term_shift | (doc - first_doc)),
                                        d => {
                                            local[d as usize * span + doc as usize / 64 - base] |=
                                                1 << (doc % 64)
                                        }
                                    }
                                }
                                state = automaton.step(state, t);
                                let mut hit = automaton.out[state as usize];
                                while hit != u32::MAX {
                                    if last_match[hit as usize] != doc {
                                        last_match[hit as usize] = doc;
                                        matches.push(hit << state_shift | (doc - first_doc));
                                    }
                                    hit = automaton.out[automaton.fail[hit as usize] as usize];
                                }
                            });
                        });
                        for (i, &bits) in local.iter().enumerate().filter(|(_, b)| **b != 0) {
                            bitmaps[i / span * words + base + i % span]
                                .fetch_or(bits, Ordering::Relaxed);
                        }
                        done.push((c, first_doc, postings, matches));
                    }
                    done
                })
            })
            .collect();
        workers
            .into_iter()
            .flat_map(|w| w.join().unwrap())
            .collect()
    });
    results.sort_unstable_by_key(|r| r.0);
    let bitmaps: Vec<u64> = bitmaps.into_iter().map(AtomicU64::into_inner).collect();

    // 4. Counting sorts, in chunk order, so every list is ascending. Thread k sorts the terms
    //    `first(k)..first(k + 1)`, which is exactly what the scan filed under k.
    let bases: Vec<u32> = results.iter().map(|r| r.1).collect();
    let (postings, matches): (Vec<_>, Vec<_>) =
        results.into_iter().map(|(_, _, p, m)| (p, m)).unzip();
    let first = |k: usize| (k * term_count).div_ceil(threads);
    let groups: Vec<(Vec<usize>, Vec<u32>)> = std::thread::scope(|scope| {
        let workers: Vec<_> = (0..threads)
            .map(|k| {
                let (postings, first, bases) = (&postings, &first, &bases);
                scope.spawn(move || {
                    let lists: Vec<(&[u32], u32)> = postings
                        .iter()
                        .zip(bases)
                        .map(|(chunk, &b)| (&chunk[k][..], b))
                        .collect();
                    group_by_key(&lists, first(k), first(k + 1) - first(k), term_shift)
                })
            })
            .collect();
        workers.into_iter().map(|w| w.join().unwrap()).collect()
    });
    drop(postings);
    let matches: Vec<(&[u32], u32)> = matches
        .iter()
        .zip(&bases)
        .map(|(m, &b)| (&m[..], b))
        .collect();
    let (state_start, state_docs) = group_by_key(&matches, 0, states, state_shift);
    let list = |t: u32| {
        let k = t as usize * threads / term_count;
        let (starts, docs) = &groups[k];
        &docs[starts[t as usize - first(k)]..starts[t as usize - first(k) + 1]]
    };
    // Applies `op` with term `t` to a bitmap result, whichever way `t` is stored.
    let apply = |r: &mut Vec<u64>, op: u8, t: u32| match dense[t as usize] {
        u32::MAX => {
            let docs = list(t);
            match op {
                b'A' => {
                    let mut kept = vec![0u64; words];
                    for &d in docs
                        .iter()
                        .filter(|&&d| r[d as usize / 64] >> (d % 64) & 1 == 1)
                    {
                        kept[d as usize / 64] |= 1 << (d % 64);
                    }
                    *r = kept;
                }
                b'O' => docs
                    .iter()
                    .for_each(|&d| r[d as usize / 64] |= 1 << (d % 64)),
                _ => docs
                    .iter()
                    .for_each(|&d| r[d as usize / 64] &= !(1 << (d % 64))),
            }
        }
        d => {
            let bits = &bitmaps[d as usize * words..(d as usize + 1) * words];
            for (x, &b) in r.iter_mut().zip(bits) {
                *x = match op {
                    b'A' => *x & b,
                    b'O' => *x | b,
                    _ => *x & !b,
                };
            }
        }
    };

    // 5. Answer the queries in parallel, in contiguous ranges so the output stays in order. A BOOL
    //    query that touches a bitmap term is evaluated on bitmaps, any other one on sorted lists.
    let parts: Vec<Vec<u8>> = std::thread::scope(|scope| {
        let workers: Vec<_> = (0..threads)
            .map(|k| {
                let (queries, phrase_end, state_start, state_docs, list, apply, dense) = (
                    &queries,
                    &phrase_end,
                    &state_start,
                    &state_docs,
                    &list,
                    &apply,
                    &dense,
                );
                scope.spawn(move || {
                    let mut text = Vec::new();
                    for query in
                        &queries[queries.len() * k / threads..queries.len() * (k + 1) / threads]
                    {
                        let owned;
                        let (count, sum, smallest): (usize, u64, Vec<u32>) = match query {
                            Query::Bool(first, ops)
                                if std::iter::once(first)
                                    .chain(ops.iter().map(|o| &o.1))
                                    .any(|&t| dense[t as usize] != u32::MAX) =>
                            {
                                let mut r = vec![0u64; words];
                                apply(&mut r, b'O', *first);
                                for &(op, t) in ops {
                                    apply(&mut r, op, t);
                                }
                                let (mut count, mut sum, mut smallest) = (0, 0, Vec::new());
                                for (i, &w) in r.iter().enumerate() {
                                    let mut w = w;
                                    while w != 0 {
                                        let d = i as u32 * 64 + w.trailing_zeros();
                                        count += 1;
                                        sum += d as u64;
                                        if smallest.len() < 10 {
                                            smallest.push(d);
                                        }
                                        w &= w - 1;
                                    }
                                }
                                (count, sum, smallest)
                            }
                            _ => {
                                let result: &[u32] = match query {
                                    Query::Bool(first, ops) if ops.is_empty() => list(*first),
                                    Query::Bool(first, ops) => {
                                        let mut r = list(*first).to_vec();
                                        for &(op, t) in ops {
                                            r = match op {
                                                b'A' => intersect(&r, list(t)),
                                                b'O' => union(&r, list(t)),
                                                _ => difference(&r, list(t)),
                                            };
                                        }
                                        owned = r;
                                        &owned
                                    }
                                    Query::Phrase(i) => {
                                        let s = phrase_end[*i as usize] as usize;
                                        &state_docs[state_start[s]..state_start[s + 1]]
                                    }
                                };
                                (
                                    result.len(),
                                    result.iter().map(|&d| d as u64).sum(),
                                    result.iter().take(10).copied().collect(),
                                )
                            }
                        };
                        push_num(&mut text, count as u64);
                        text.push(b' ');
                        push_num(&mut text, sum);
                        for d in smallest {
                            text.push(b' ');
                            push_num(&mut text, d as u64);
                        }
                        text.push(b'\n');
                    }
                    text
                })
            })
            .collect();
        workers.into_iter().map(|w| w.join().unwrap()).collect()
    });
    let mut out =
        std::fs::File::create(format!("{out_dir}/results.txt")).expect("cannot create output");
    for text in parts {
        out.write_all(&text).expect("cannot write output");
    }
}
