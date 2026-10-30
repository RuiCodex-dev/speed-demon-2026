//! Speed Demon 2026, Track B (Index).
//!
//! The queries are known before the corpus is read, so only their terms are indexed:
//! 1. Parse `queries.txt` and intern every distinct term.
//! 2. Scan `docs.txt` in parallel chunks of whole lines. Each token is lowercased, looked up among the
//!    query terms, and a (term, doc) posting is recorded once per document for terms BOOL queries use.
//!    Phrases are matched in the same pass: an Aho-Corasick automaton over term ids finds every phrase
//!    query in a document's token stream, so a phrase answer is a ready-made list.
//! 3. A counting sort over the chunks, in chunk order, makes every list sorted by doc id.
//! 4. BOOL queries are sorted-list merges applied left to right.
use std::collections::{HashMap, VecDeque};
use std::hash::{BuildHasherDefault, Hasher};
use std::io::Write;
use std::sync::atomic::{AtomicUsize, Ordering};

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
    let (mut buf, mut len, mut h) = ([0u8; 32], 0usize, 0u64);
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

/// The query terms in an open-addressing table keyed by the tokenizer's own hash, so a lookup costs
/// one probe sequence and one byte comparison.
struct Vocab {
    slots: Vec<u32>,
    words: Vec<&'static [u8]>,
    shift: u32,
}

impl Vocab {
    /// `words[id]` is the term with that id.
    fn new(words: Vec<&'static [u8]>) -> Vocab {
        let bits = (words.len() * 2)
            .next_power_of_two()
            .trailing_zeros()
            .max(1);
        let mut vocab = Vocab {
            slots: vec![u32::MAX; 1 << bits],
            words,
            shift: 64 - bits,
        };
        for id in 0..vocab.words.len() {
            let h = vocab.words[id].iter().fold(0, |h, &c| step_hash(h, c));
            let mut i = (h >> vocab.shift) as usize;
            while vocab.slots[i] != u32::MAX {
                i = (i + 1) & (vocab.slots.len() - 1);
            }
            vocab.slots[i] = id as u32;
        }
        vocab
    }

    fn get(&self, token: &[u8], h: u64) -> Option<u32> {
        let mut i = (h >> self.shift) as usize;
        loop {
            let id = self.slots[i];
            if id == u32::MAX {
                return None;
            }
            if self.words[id as usize] == token {
                return Some(id);
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
}

impl Phrases {
    /// Builds the automaton and returns, for every phrase, the state that ends it.
    fn build(phrases: &[Vec<u32>], term_count: usize) -> (Phrases, Vec<u32>) {
        let mut a = Phrases {
            root: vec![0; term_count],
            goto: FxMap::default(),
            fail: vec![0],
            out: vec![u32::MAX],
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
            if let Some(&next) = self.goto.get(&((s as u64) << 32 | t as u64)) {
                return next;
            }
            s = self.fail[s as usize];
        }
    }
}

/// Counting sort of (key, doc) pairs, keys in `lo..lo + keys`, from chunks in document order:
/// each key's docs end up contiguous and ascending. Returns every key's start offset and the docs.
fn group_by_key(chunks: &[&[(u32, u32)]], lo: usize, keys: usize) -> (Vec<usize>, Vec<u32>) {
    let mut starts = vec![0usize; keys + 1];
    for chunk in chunks {
        for &(k, _) in chunk.iter() {
            starts[k as usize - lo + 1] += 1;
        }
    }
    for k in 0..keys {
        starts[k + 1] += starts[k];
    }
    let mut fill = starts.clone();
    let mut docs = vec![0u32; starts[keys]];
    for chunk in chunks {
        for &(k, d) in chunk.iter() {
            docs[fill[k as usize - lo]] = d;
            fill[k as usize - lo] += 1;
        }
    }
    (starts, docs)
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
    let threads = std::thread::available_parallelism().map_or(1, |n| n.get());

    // 1. Queries and their vocabulary. Only terms used by a BOOL query need posting lists.
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
    let vocab = Vocab::new(words);
    let (automaton, phrase_end) = Phrases::build(&phrases, term_count);
    let states = automaton.fail.len();

    // 2. Parallel corpus scan: BOOL postings and phrase matches, each once per document.
    let docs = map_file(&format!("{data_dir}/docs.txt"));
    let chunks = split_lines(docs, threads * 16);
    let next = AtomicUsize::new(0);
    let mut results: Vec<(usize, Vec<Vec<(u32, u32)>>, Vec<(u32, u32)>)> =
        std::thread::scope(|scope| {
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
                            let (mut postings, mut matches) =
                                (vec![Vec::new(); threads], Vec::new());
                            let mut p = 0;
                            while p < chunk.len() {
                                let tab = p + chunk[p..].iter().position(|&b| b == b'\t').unwrap();
                                let end =
                                    tab + chunk[tab..].iter().position(|&b| b == b'\n').unwrap();
                                let doc = chunk[p..tab]
                                    .iter()
                                    .fold(0u32, |acc, &b| acc * 10 + (b - b'0') as u32);
                                let mut state = 0;
                                tokens(&chunk[tab + 1..end], |token, h| {
                                    let Some(t) = vocab.get(token, h) else {
                                        state = 0; // no phrase contains a word that is not a query term
                                        return;
                                    };
                                    if in_bool[t as usize] && last_doc[t as usize] != doc {
                                        last_doc[t as usize] = doc;
                                        postings[t as usize * threads / term_count].push((t, doc));
                                    }
                                    state = automaton.step(state, t);
                                    let mut hit = automaton.out[state as usize];
                                    while hit != u32::MAX {
                                        if last_match[hit as usize] != doc {
                                            last_match[hit as usize] = doc;
                                            matches.push((hit, doc));
                                        }
                                        hit = automaton.out[automaton.fail[hit as usize] as usize];
                                    }
                                });
                                p = end + 1;
                            }
                            done.push((c, postings, matches));
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

    // 3. Counting sorts, in chunk order, so every list is ascending. Thread k sorts the terms
    //    `first(k)..first(k + 1)`, which is exactly what the scan filed under k.
    let (postings, matches): (Vec<_>, Vec<_>) = results.into_iter().map(|(_, p, m)| (p, m)).unzip();
    let first = |k: usize| (k * term_count).div_ceil(threads);
    let groups: Vec<(Vec<usize>, Vec<u32>)> = std::thread::scope(|scope| {
        let workers: Vec<_> = (0..threads)
            .map(|k| {
                let (postings, first) = (&postings, &first);
                scope.spawn(move || {
                    let lists: Vec<&[(u32, u32)]> =
                        postings.iter().map(|chunk| &chunk[k][..]).collect();
                    group_by_key(&lists, first(k), first(k + 1) - first(k))
                })
            })
            .collect();
        workers.into_iter().map(|w| w.join().unwrap()).collect()
    });
    drop(postings);
    let matches: Vec<&[(u32, u32)]> = matches.iter().map(|m| &m[..]).collect();
    let (state_start, state_docs) = group_by_key(&matches, 0, states);
    let list = |t: u32| {
        let k = t as usize * threads / term_count;
        let (starts, docs) = &groups[k];
        &docs[starts[t as usize - first(k)]..starts[t as usize - first(k) + 1]]
    };

    // 4. Answer the queries in parallel, in contiguous ranges so the output stays in order.
    let parts: Vec<Vec<u8>> = std::thread::scope(|scope| {
        let workers: Vec<_> = (0..threads)
            .map(|k| {
                let (queries, phrase_end, state_start, state_docs, list) =
                    (&queries, &phrase_end, &state_start, &state_docs, &list);
                scope.spawn(move || {
                    let mut text = Vec::new();
                    for query in
                        &queries[queries.len() * k / threads..queries.len() * (k + 1) / threads]
                    {
                        let owned;
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
                        push_num(&mut text, result.len() as u64);
                        text.push(b' ');
                        push_num(&mut text, result.iter().map(|&d| d as u64).sum());
                        for &d in result.iter().take(10) {
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
