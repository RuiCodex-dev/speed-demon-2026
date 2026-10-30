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

/// Shortest distance from `s` to `t`: Ok(None) if `t` is unreachable, Err(()) if a distance did not
/// fit in `T`. `dist[v]` holds v's tentative distance + 1, so 0 means "not reached": the arrays can
/// come zeroed from calloc, and only the entries a search touched are reset afterwards.
fn shortest<T: Copy + Into<u64> + TryFrom<u64> + PartialEq + Default>(
    offsets: &[u32],
    edges: &[(u32, u32)],
    (s, t): (u32, u32),
    dist: &mut [T],
    touched: &mut Vec<u32>,
    heap: &mut RadixHeap,
) -> Result<Option<u64>, ()> {
    let mut search = || {
        dist[s as usize] = T::try_from(1).map_err(|_| ())?;
        touched.push(s);
        heap.push(0, s);
        while let Some((d, u)) = heap.pop() {
            if u == t {
                return Ok(Some(d));
            }
            if d + 1 > dist[u as usize].into() {
                continue; // stale entry: u was already settled closer
            }
            for &(v, w) in &edges[offsets[u as usize] as usize..offsets[u as usize + 1] as usize] {
                let nd = d + w as u64;
                let old = dist[v as usize];
                if old == T::default() || nd + 1 < old.into() {
                    if old == T::default() {
                        touched.push(v);
                    }
                    dist[v as usize] = T::try_from(nd + 1).map_err(|_| ())?;
                    heap.push(nd, v);
                }
            }
        }
        Ok(None)
    };
    let answer = search();
    for &v in touched.iter() {
        dist[v as usize] = T::default();
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

    // 1. One parse. Every thread files its edges into buckets by tail (`u >> SHIFT`, about 8k nodes
    //    per bucket), in file order, and marks in-edges in its own bitset: only sequential writes
    //    and a cache-sized bitset, instead of random atomics into node-sized arrays.
    //    has_in[v]: v has an in-edge from another node, so a path from some s != v can end at v.
    const SHIFT: usize = 13;
    let buckets = (n >> SHIFT) + 1;
    let parsed: Vec<(Vec<Vec<(u32, u32, u32)>>, Vec<u64>)> = std::thread::scope(|scope| {
        let workers: Vec<_> = chunks
            .iter()
            .map(|&chunk| {
                scope.spawn(move || {
                    // An edge line is about 19 bytes; sizing the buckets up front avoids regrowing them.
                    let per_bucket = chunk.len() / 18 / buckets + 64;
                    let mut out: Vec<Vec<(u32, u32, u32)>> = (0..buckets)
                        .map(|_| Vec::with_capacity(per_bucket))
                        .collect();
                    let mut has_in = vec![0u64; n.div_ceil(64)];
                    let mut p = 0;
                    while p < chunk.len() {
                        let (u, v, w) = (
                            number(chunk, &mut p) as u32,
                            number(chunk, &mut p) as u32,
                            number(chunk, &mut p) as u32,
                        );
                        if u != v {
                            has_in[v as usize / 64] |= 1 << (v % 64);
                        }
                        out[u as usize >> SHIFT].push((u, v, w));
                    }
                    (out, has_in)
                })
            })
            .collect();
        workers.into_iter().map(|w| w.join().unwrap()).collect()
    });
    let has_in: Vec<u64> = (0..n.div_ceil(64))
        .map(|i| parsed.iter().fold(0, |acc, p| acc | p.1[i]))
        .collect();

    // 2. The CSR in input ids: each bucket is counting-sorted into its own slice by one thread.
    let bucket_start =
        prefix_sums((0..buckets).map(|b| parsed.iter().map(|p| p.0[b].len() as u32).sum()));
    assert_eq!(
        bucket_start[buckets] as usize, m,
        "edge count differs from the header"
    );
    let mut offsets = vec![m as u32; n + 1];
    let mut edges = vec![(0u32, 0u32); m];
    std::thread::scope(|scope| {
        let (mut offsets_rest, mut edges_rest) = (&mut offsets[..n], &mut edges[..]);
        for k in 0..threads {
            let (first, last) = (buckets * k / threads, buckets * (k + 1) / threads);
            let nodes = (last << SHIFT).min(n) - (first << SHIFT).min(n);
            let (my_offsets, rest) = offsets_rest.split_at_mut(nodes);
            offsets_rest = rest;
            let (my_edges, rest) =
                edges_rest.split_at_mut((bucket_start[last] - bucket_start[first]) as usize);
            edges_rest = rest;
            let (parsed, bucket_start) = (&parsed, &bucket_start);
            scope.spawn(move || {
                for b in first..last {
                    let base = b << SHIFT;
                    let span = ((b + 1) << SHIFT).min(n) - base.min(n);
                    let mut cursor = vec![0u32; span + 1];
                    for list in parsed.iter().map(|p| &p.0[b]) {
                        for &(u, _, _) in list {
                            cursor[u as usize - base + 1] += 1;
                        }
                    }
                    for i in 0..span {
                        cursor[i + 1] += cursor[i];
                    }
                    let (start, local) = (
                        bucket_start[b],
                        (base - (first << SHIFT)).min(my_offsets.len()),
                    );
                    for i in 0..span {
                        my_offsets[local + i] = start + cursor[i];
                    }
                    let edge_base = (start - bucket_start[first]) as usize;
                    for list in parsed.iter().map(|p| &p.0[b]) {
                        for &(u, v, w) in list {
                            let slot = &mut cursor[u as usize - base];
                            my_edges[edge_base + *slot as usize] = (v, w);
                            *slot += 1;
                        }
                    }
                }
            });
        }
    });
    drop(parsed);

    // Every thread grows a breadth-first region from its own seed and claims nodes with an atomic
    // flag, so each region gets contiguous new ids. Nodes no seed reaches (islands, isolated nodes)
    // are numbered afterwards by the same search, run serially from each of them.
    let claimed: Vec<AtomicBool> = (0..n).map(|_| AtomicBool::new(false)).collect();
    let grow = |seed: u32, region: &mut Vec<u32>| {
        if claimed[seed as usize].load(Ordering::Relaxed) || claimed[seed as usize].swap(true, Ordering::Relaxed) {
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
    // 4. The CSR in breadth-first ids, the one the searches use: each thread copies a contiguous
    //    range of new ids from the old CSR, renaming heads on the way.
    let new_offsets = prefix_sums(
        order
            .iter()
            .map(|&old| offsets[old as usize + 1] - offsets[old as usize]),
    );
    let mut new_edges = vec![(0u32, 0u32); m];
    std::thread::scope(|scope| {
        let mut rest = &mut new_edges[..];
        for k in 0..threads {
            let (first, last) = (n * k / threads, n * (k + 1) / threads);
            let (mine, tail) = rest.split_at_mut((new_offsets[last] - new_offsets[first]) as usize);
            rest = tail;
            let (order, offsets, edges, rank) = (&order[first..last], &offsets, &edges, &rank);
            scope.spawn(move || {
                let mut out = mine.iter_mut();
                for &old in order {
                    for &(v, w) in
                        &edges[offsets[old as usize] as usize..offsets[old as usize + 1] as usize]
                    {
                        *out.next().unwrap() = (rank[v as usize], w);
                    }
                }
            });
        }
    });
    drop((order, edges));
    let (offsets, edges) = (new_offsets, new_edges);

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
    // Queries are taken in order of their source's new id, so the threads search neighbouring
    // regions at the same time and share the graph in the last-level cache.
    let mut by_source: Vec<usize> = (0..list.len()).collect();
    by_source.sort_unstable_by_key(|&i| rank[list[i].0 as usize]);
    let next = AtomicU32::new(0);
    std::thread::scope(|scope| {
        for _ in 0..threads.min(list.len()) {
            scope.spawn(|| {
                // u32 distances halve the per-thread array; a search that overflows them (only the
                // spec's near-2^32 weights do) is repeated with a u64 array made on first need.
                let (mut dist, mut wide, mut touched) = (vec![0u32; n], Vec::new(), Vec::new());
                let mut heap = RadixHeap {
                    buckets: vec![Vec::new(); 65],
                    last: 0,
                };
                loop {
                    let Some(&i) = by_source.get(next.fetch_add(1, Ordering::Relaxed) as usize) else { break };
                    let (s, t) = list[i];
                    let reachable = s == t || has_in[t as usize / 64] >> (t % 64) & 1 == 1;
                    if !reachable {
                        continue;
                    }
                    let query = (rank[s as usize], rank[t as usize]);
                    let answer =
                        shortest(&offsets, &edges, query, &mut dist, &mut touched, &mut heap)
                            .or_else(|_| {
                                if wide.is_empty() {
                                    wide = vec![0u64; n];
                                }
                                shortest(
                                    &offsets,
                                    &edges,
                                    query,
                                    &mut wide,
                                    &mut touched,
                                    &mut heap,
                                )
                            });
                    if let Ok(Some(d)) = answer {
                        answers[i].store(d, Ordering::Relaxed);
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
