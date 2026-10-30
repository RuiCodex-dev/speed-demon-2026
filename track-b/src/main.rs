//! Speed Demon 2026, Track B (Index).
//!
//! The queries are known before the corpus is read, so only their terms are indexed:
//! 1. Parse `queries.txt` and intern every distinct term.
//! 2. Scan `docs.txt` in parallel chunks of whole lines. Each token is lowercased, looked up among the
//!    query terms, and a (term, doc) posting is recorded once per document. Each document's text range
//!    is kept for phrase checks.
//! 3. A counting sort over the chunks, in chunk order, gives each term a posting list sorted by doc id.
//! 4. BOOL queries are sorted-list merges applied left to right. A PHRASE intersects its terms'
//!    postings, then re-tokenises only those candidate documents to check adjacency.
use std::collections::HashMap;
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

/// Calls `f` with every token of `text`: maximal runs of [a-z0-9] after ASCII-lowercasing.
/// Tokens are lowercased into `buf`, so the slice passed to `f` is only valid during the call.
fn tokens(text: &[u8], buf: &mut Vec<u8>, mut f: impl FnMut(&[u8])) {
    for &b in text {
        let c = b.to_ascii_lowercase();
        if c.is_ascii_lowercase() || c.is_ascii_digit() {
            buf.push(c);
        } else if !buf.is_empty() {
            f(buf);
            buf.clear();
        }
    }
    if !buf.is_empty() {
        f(buf);
        buf.clear();
    }
}

enum Query {
    /// The first term, then (operator, term) pairs. Operators: b'A'nd, b'O'r, b'N'ot.
    Bool(u32, Vec<(u8, u32)>),
    Phrase(Vec<u32>),
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

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let data_dir = args.get(1).map_or("/data", |s| s.as_str());
    let out_dir = args.get(2).map_or("/out", |s| s.as_str());
    let threads = std::thread::available_parallelism().map_or(1, |n| n.get());

    // 1. Queries and their vocabulary.
    let query_text = map_file(&format!("{data_dir}/queries.txt"));
    let mut terms: FxMap<&'static [u8], u32> = FxMap::default();
    let mut intern = |word: &'static [u8]| {
        let next = terms.len() as u32;
        *terms.entry(word).or_insert(next)
    };
    let mut queries = Vec::new();
    for line in query_text.split(|&b| b == b'\n').filter(|l| !l.is_empty()) {
        if let Some(rest) = line.strip_prefix(b"PHRASE \"") {
            let words = &rest[..rest.len() - 1];
            queries.push(Query::Phrase(
                words.split(|&b| b == b' ').map(&mut intern).collect(),
            ));
        } else {
            let mut words = line[5..].split(|&b| b == b' ');
            let first = intern(words.next().unwrap());
            let mut ops = Vec::new();
            while let (Some(op), Some(term)) = (words.next(), words.next()) {
                ops.push((op[0], intern(term)));
            }
            queries.push(Query::Bool(first, ops));
        }
    }
    let term_count = terms.len();

    // 2. Parallel corpus scan: postings for query terms only, plus each document's text range.
    let docs = map_file(&format!("{data_dir}/docs.txt"));
    let chunks = split_lines(docs, threads * 16);
    let next = AtomicUsize::new(0);
    let mut results: Vec<(usize, Vec<(u32, u32)>, Vec<(u32, u32, u32)>)> =
        std::thread::scope(|scope| {
            let workers: Vec<_> = (0..threads)
                .map(|_| {
                    scope.spawn(|| {
                        let mut done = Vec::new();
                        let mut last_doc = vec![u32::MAX; term_count];
                        let mut buf = Vec::new();
                        loop {
                            let c = next.fetch_add(1, Ordering::Relaxed);
                            let Some(chunk) = chunks.get(c) else { break };
                            let base = chunk.as_ptr() as usize - docs.as_ptr() as usize;
                            let (mut postings, mut ranges) = (Vec::new(), Vec::new());
                            let mut p = 0;
                            while p < chunk.len() {
                                let tab = p + chunk[p..].iter().position(|&b| b == b'\t').unwrap();
                                let end =
                                    tab + chunk[tab..].iter().position(|&b| b == b'\n').unwrap();
                                let doc = chunk[p..tab]
                                    .iter()
                                    .fold(0u32, |acc, &b| acc * 10 + (b - b'0') as u32);
                                ranges.push((doc, (base + tab + 1) as u32, (base + end) as u32));
                                tokens(&chunk[tab + 1..end], &mut buf, |token| {
                                    if let Some(&t) = terms.get(token) {
                                        if last_doc[t as usize] != doc {
                                            last_doc[t as usize] = doc;
                                            postings.push((t, doc));
                                        }
                                    }
                                });
                                p = end + 1;
                            }
                            done.push((c, postings, ranges));
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

    // 3. Counting sort by term, in chunk order, so every posting list is ascending.
    let mut starts = vec![0usize; term_count + 1];
    for (_, postings, _) in &results {
        for &(t, _) in postings {
            starts[t as usize + 1] += 1;
        }
    }
    for t in 0..term_count {
        starts[t + 1] += starts[t];
    }
    let mut fill = starts.clone();
    let mut all = vec![0u32; starts[term_count]];
    let mut ranges = Vec::new();
    for (_, postings, chunk_ranges) in results {
        for (t, doc) in postings {
            all[fill[t as usize]] = doc;
            fill[t as usize] += 1;
        }
        ranges.extend(chunk_ranges);
    }
    let postings = |t: u32| &all[starts[t as usize]..starts[t as usize + 1]];

    // 4. Answer the queries in parallel; each answer is formatted into its own slot.
    let answers: Vec<std::sync::Mutex<Vec<u8>>> =
        (0..queries.len()).map(|_| Default::default()).collect();
    let next = AtomicUsize::new(0);
    std::thread::scope(|scope| {
        for _ in 0..threads {
            scope.spawn(|| {
                let mut buf = Vec::new();
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some(query) = queries.get(i) else { break };
                    let result: Vec<u32> = match query {
                        Query::Bool(first, ops) => {
                            let mut r = postings(*first).to_vec();
                            for &(op, t) in ops {
                                r = match op {
                                    b'A' => intersect(&r, postings(t)),
                                    b'O' => union(&r, postings(t)),
                                    _ => difference(&r, postings(t)),
                                };
                            }
                            r
                        }
                        Query::Phrase(words) => {
                            let mut order: Vec<u32> = words.clone();
                            order.sort_unstable_by_key(|&t| postings(t).len());
                            let mut candidates = postings(order[0]).to_vec();
                            for &t in &order[1..] {
                                candidates = intersect(&candidates, postings(t));
                            }
                            candidates.retain(|&doc| {
                                let (start, end) = (
                                    ranges[doc as usize].1 as usize,
                                    ranges[doc as usize].2 as usize,
                                );
                                let mut ids = Vec::new();
                                tokens(&docs[start..end], &mut buf, |token| {
                                    ids.push(terms.get(token).copied().unwrap_or(u32::MAX))
                                });
                                ids.windows(words.len()).any(|w| w == &words[..])
                            });
                            candidates
                        }
                    };
                    let sum: u64 = result.iter().map(|&d| d as u64).sum();
                    let mut line = format!("{} {}", result.len(), sum);
                    for d in result.iter().take(10) {
                        line.push_str(&format!(" {d}"));
                    }
                    line.push('\n');
                    *answers[i].lock().unwrap() = line.into_bytes();
                }
            });
        }
    });
    let mut out = Vec::new();
    for answer in answers {
        out.extend_from_slice(&answer.into_inner().unwrap());
    }
    std::fs::File::create(format!("{out_dir}/results.txt"))
        .and_then(|mut f| f.write_all(&out))
        .expect("cannot write output");
}
