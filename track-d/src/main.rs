//! Speed Demon 2026, Track D (Traverse).
//!
//! 1. Map `graph.txt`. A first parallel pass counts out-degrees and marks which nodes have an
//!    in-edge from another node; a second pass scatters the edges into a compressed adjacency
//!    array (CSR). No intermediate edge list is kept.
//! 2. Renumber the nodes in breadth-first order: the input ids are a random relabelling, so without
//!    this every step of a search is a cache miss. A third pass scatters the edges straight into the
//!    renumbered CSR, after the first one has been freed.
//! 3. Answer the queries in parallel, one Dijkstra per query with a radix heap, stopping when the
//!    target is settled. A target with no in-edge from another node is unreachable without a search.
//!    Each thread keeps one distance array and resets only the nodes the last search touched.
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

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

/// Reads the unsigned decimal at `*pos` and skips the one separator byte after it.
fn number(data: &[u8], pos: &mut usize) -> u64 {
    let mut value = 0;
    while data[*pos] >= b'0' {
        value = value * 10 + (data[*pos] - b'0') as u64;
        *pos += 1;
    }
    *pos += 1;
    value
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

/// Calls `f(u, v, w)` for every edge line, one thread per chunk.
fn for_each_edge(chunks: &[&[u8]], f: &(dyn Fn(u32, u32, u32) + Sync)) {
    std::thread::scope(|scope| {
        for &chunk in chunks {
            scope.spawn(move || {
                let mut p = 0;
                while p < chunk.len() {
                    let (u, v, w) = (
                        number(chunk, &mut p),
                        number(chunk, &mut p),
                        number(chunk, &mut p),
                    );
                    f(u as u32, v as u32, w as u32);
                }
            });
        }
    });
}

#[derive(Clone, Copy)]
struct SharedPtr(*mut (u32, u32));
// SAFETY: each edge slot is claimed exactly once through an atomic cursor.
unsafe impl Send for SharedPtr {}
unsafe impl Sync for SharedPtr {}

/// Builds the CSR edge array for `offsets`, with every node id passed through `id`.
fn scatter(chunks: &[&[u8]], offsets: &[u32], id: &(dyn Fn(u32) -> u32 + Sync)) -> Vec<(u32, u32)> {
    let cursor: Vec<AtomicU32> = offsets.iter().map(|&o| AtomicU32::new(o)).collect();
    let mut edges = vec![(0u32, 0u32); *offsets.last().unwrap() as usize];
    let ptr = SharedPtr(edges.as_mut_ptr());
    for_each_edge(chunks, &|u, v, w| {
        let ptr = ptr;
        let slot = cursor[id(u) as usize].fetch_add(1, Ordering::Relaxed) as usize;
        // SAFETY: fetch_add gives every edge a distinct slot inside its tail's range.
        unsafe { *ptr.0.add(slot) = (id(v), w) };
    });
    edges
}

fn prefix_sums(counts: impl Iterator<Item = u32>) -> Vec<u32> {
    let mut offsets = vec![0u32];
    for c in counts {
        offsets.push(offsets.last().unwrap() + c);
    }
    offsets
}

/// Radix heap: a priority queue for integer keys that never go below the last key popped, which
/// Dijkstra guarantees. Bucket i holds the keys whose highest bit that differs from `last` is bit
/// i - 1, so a pop only rescans one bucket and every entry moves down at most 64 times.
struct RadixHeap {
    buckets: Vec<Vec<(u64, u32)>>,
    last: u64,
}

impl RadixHeap {
    fn bucket(&self, key: u64) -> usize {
        64 - (key ^ self.last).leading_zeros() as usize
    }
    fn push(&mut self, key: u64, node: u32) {
        let b = self.bucket(key);
        self.buckets[b].push((key, node));
    }
    fn pop(&mut self) -> Option<(u64, u32)> {
        if self.buckets[0].is_empty() {
            let i = self.buckets.iter().position(|b| !b.is_empty())?;
            let mut moved = std::mem::take(&mut self.buckets[i]);
            self.last = moved.iter().map(|e| e.0).min().unwrap();
            for &(key, node) in &moved {
                self.push(key, node); // always lands in a bucket below i
            }
            moved.clear();
            self.buckets[i] = moved; // keep the allocation
        }
        self.buckets[0].pop()
    }
    fn clear(&mut self) {
        self.buckets.iter_mut().for_each(Vec::clear);
        self.last = 0;
    }
}

/// Shortest distance from `s` to `t`, or None if `t` is not reachable. `dist` is all-MAX on entry
/// and on return; only the entries a search touched are reset.
fn shortest(
    offsets: &[u32],
    edges: &[(u32, u32)],
    s: u32,
    t: u32,
    dist: &mut [u64],
    touched: &mut Vec<u32>,
    heap: &mut RadixHeap,
) -> Option<u64> {
    dist[s as usize] = 0;
    touched.push(s);
    heap.push(0, s);
    let mut answer = None;
    while let Some((d, u)) = heap.pop() {
        if u == t {
            answer = Some(d);
            break;
        }
        if d > dist[u as usize] {
            continue; // stale entry: u was already settled closer
        }
        for &(v, w) in &edges[offsets[u as usize] as usize..offsets[u as usize + 1] as usize] {
            let nd = d + w as u64;
            if nd < dist[v as usize] {
                if dist[v as usize] == u64::MAX {
                    touched.push(v);
                }
                dist[v as usize] = nd;
                heap.push(nd, v);
            }
        }
    }
    for &v in touched.iter() {
        dist[v as usize] = u64::MAX;
    }
    touched.clear();
    heap.clear();
    answer
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let data_dir = args.get(1).map_or("/data", |s| s.as_str());
    let out_dir = args.get(2).map_or("/out", |s| s.as_str());
    let graph = map_file(&format!("{data_dir}/graph.txt"));
    let queries = map_file(&format!("{data_dir}/queries.txt"));
    let threads = std::thread::available_parallelism().map_or(1, |n| n.get());

    let mut pos = 0;
    let n = number(graph, &mut pos) as usize;
    let m = number(graph, &mut pos) as usize;
    let chunks = split_lines(&graph[pos..], threads);

    // Pass 1: out-degrees, and has_in[v] = v has an in-edge from another node, so a path from some
    // s != v can end at v.
    let degree: Vec<AtomicU32> = (0..n).map(|_| AtomicU32::new(0)).collect();
    let has_in: Vec<AtomicU64> = (0..n.div_ceil(64)).map(|_| AtomicU64::new(0)).collect();
    for_each_edge(&chunks, &|u, v, _| {
        degree[u as usize].fetch_add(1, Ordering::Relaxed);
        if u != v {
            has_in[v as usize / 64].fetch_or(1 << (v % 64), Ordering::Relaxed);
        }
    });
    let degree: Vec<u32> = degree.into_iter().map(AtomicU32::into_inner).collect();
    let offsets = prefix_sums(degree.iter().copied());
    assert_eq!(offsets[n] as usize, m, "edge count differs from the header");

    // Pass 2: the CSR in input ids, used only to find a breadth-first order.
    let edges = scatter(&chunks, &offsets, &|u| u);
    // Every thread grows a breadth-first region from its own seed and claims nodes with an atomic
    // flag, so each region gets contiguous new ids. Nodes no seed reaches (islands, isolated nodes)
    // are numbered afterwards by the same search, run serially from each of them.
    let claimed: Vec<AtomicBool> = (0..n).map(|_| AtomicBool::new(false)).collect();
    let grow = |seed: u32, region: &mut Vec<u32>| {
        if claimed[seed as usize].swap(true, Ordering::Relaxed) {
            return;
        }
        let mut head = region.len();
        region.push(seed);
        while head < region.len() {
            let u = region[head] as usize;
            head += 1;
            for &(v, _) in &edges[offsets[u] as usize..offsets[u + 1] as usize] {
                if !claimed[v as usize].load(Ordering::Relaxed)
                    && !claimed[v as usize].swap(true, Ordering::Relaxed)
                {
                    region.push(v);
                }
            }
        }
    };
    let regions: Vec<Vec<u32>> = std::thread::scope(|scope| {
        let workers: Vec<_> = (0..threads)
            .map(|k| {
                let grow = &grow;
                scope.spawn(move || {
                    let mut region = Vec::new();
                    grow((n * k / threads) as u32, &mut region);
                    region
                })
            })
            .collect();
        workers.into_iter().map(|w| w.join().unwrap()).collect()
    });
    let mut order: Vec<u32> = regions.concat();
    drop(regions);
    for root in 0..n as u32 {
        grow(root, &mut order);
    }
    let mut rank = vec![0u32; n];
    for (new, &old) in order.iter().enumerate() {
        rank[old as usize] = new as u32;
    }
    drop(edges);
    let offsets = prefix_sums(order.iter().map(|&old| degree[old as usize]));
    drop((order, degree));

    // Pass 3: the CSR in breadth-first ids, the one the searches use.
    let edges = scatter(&chunks, &offsets, &|u| rank[u as usize]);

    // Queries, answered in parallel; answers land in their input order.
    let mut list = Vec::new();
    let mut p = 0;
    while p < queries.len() {
        list.push((
            number(queries, &mut p) as u32,
            number(queries, &mut p) as u32,
        ));
    }
    let answers: Vec<AtomicU64> = (0..list.len()).map(|_| AtomicU64::new(u64::MAX)).collect();
    let next = AtomicU32::new(0);
    std::thread::scope(|scope| {
        for _ in 0..threads.min(list.len()) {
            scope.spawn(|| {
                let (mut dist, mut touched) = (vec![u64::MAX; n], Vec::new());
                let mut heap = RadixHeap {
                    buckets: vec![Vec::new(); 65],
                    last: 0,
                };
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed) as usize;
                    let Some(&(s, t)) = list.get(i) else { break };
                    let reachable = s == t
                        || has_in[t as usize / 64].load(Ordering::Relaxed) >> (t % 64) & 1 == 1;
                    if reachable {
                        if let Some(d) = shortest(
                            &offsets,
                            &edges,
                            rank[s as usize],
                            rank[t as usize],
                            &mut dist,
                            &mut touched,
                            &mut heap,
                        ) {
                            answers[i].store(d, Ordering::Relaxed);
                        }
                    }
                }
            });
        }
    });
    let mut out = String::with_capacity(list.len() * 12);
    for answer in &answers {
        match answer.load(Ordering::Relaxed) {
            u64::MAX => out.push_str("-1\n"), // a real distance is below 2^63
            d => out.push_str(&format!("{d}\n")),
        }
    }
    std::fs::write(format!("{out_dir}/distances.txt"), out).expect("cannot write output");
}
