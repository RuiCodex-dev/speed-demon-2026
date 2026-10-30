//! Speed Demon 2026, Track D (Traverse).
//!
//! 1. Map `graph.txt`. A first parallel pass counts out-degrees and marks which nodes have an
//!    in-edge from another node; a second pass scatters the edges into a compressed adjacency
//!    array (CSR). No intermediate edge list is kept.
//! 2. Renumber the nodes in breadth-first order: the input ids are a random relabelling, so without
//!    this every step of a search is a cache miss. A third pass scatters the edges straight into the
//!    renumbered CSR, after the first one has been freed.
//!    An edge is one u32, `head << wbits | weight`; the rare weight that does not fit is kept in a
//!    small map by (tail, head).
//! 3. Answer the queries in parallel, one Dijkstra per query with a radix heap, stopping when the
//!    target is settled. A target with no in-edge from another node is unreachable without a search.
//!    Distances live in 1024-node pages taken from a per-thread pool, so a thread holds only the
//!    pages its current search touched.
use std::arch::x86_64::{_MM_HINT_T0, _mm_prefetch};
use std::collections::HashMap;
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
/// i - 1, so a pop only rescans one bucket and every entry moves down at most 64 times. Keys are
/// stored as `K` (u32 for the u32 searches), so an entry is 8 bytes there instead of 16.
struct RadixHeap<K> {
    buckets: Vec<Vec<(K, u32)>>,
    last: u64,
}

impl<K: Copy + Into<u64>> RadixHeap<K> {
    fn new() -> Self {
        RadixHeap {
            buckets: (0..65).map(|_| Vec::new()).collect(),
            last: 0,
        }
    }
    fn bucket(&self, key: K) -> usize {
        64 - (key.into() ^ self.last).leading_zeros() as usize
    }
    fn push(&mut self, key: K, node: u32) {
        let b = self.bucket(key);
        self.buckets[b].push((key, node));
    }
    fn pop(&mut self) -> Option<(K, u32)> {
        if self.buckets[0].is_empty() {
            let i = self.buckets.iter().position(|b| !b.is_empty())?;
            let mut moved = std::mem::take(&mut self.buckets[i]);
            self.last = moved.iter().map(|e| e.0.into()).min().unwrap();
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

/// Entries per page of `Dist`.
const PAGE: usize = 1024;

/// Tentative distances (+ 1, so 0 means "not reached") in pages of `PAGE` nodes. A page is taken
/// from the pool when first written and returned, zeroed, when the search ends, so a thread holds
/// only the pages its current search touched. Pool page 0 stays all zero and stands for every page
/// not in use.
struct Dist<T> {
    page: Vec<u32>,
    pool: Vec<T>,
    free: Vec<u32>,
    used: Vec<u32>,
}

impl<T: Copy + Default> Dist<T> {
    fn new(n: usize) -> Self {
        Dist {
            page: vec![0; n.div_ceil(PAGE)],
            pool: vec![T::default(); PAGE],
            free: Vec::new(),
            used: Vec::new(),
        }
    }
    fn get(&self, v: u32) -> T {
        self.pool[self.page[v as usize / PAGE] as usize * PAGE + v as usize % PAGE]
    }
    fn set(&mut self, v: u32, d: T) {
        let p = v as usize / PAGE;
        if self.page[p] == 0 {
            self.page[p] = match self.free.pop() {
                Some(slot) => slot,
                None => {
                    self.pool.resize(self.pool.len() + PAGE, T::default());
                    (self.pool.len() / PAGE - 1) as u32
                }
            };
            self.used.push(p as u32);
        }
        self.pool[self.page[p] as usize * PAGE + v as usize % PAGE] = d;
    }
    fn reset(&mut self) {
        for p in self.used.drain(..) {
            let slot = std::mem::take(&mut self.page[p as usize]);
            self.pool[slot as usize * PAGE..][..PAGE].fill(T::default());
            self.free.push(slot);
        }
    }
}

/// The graph the searches use: a CSR of packed edges (see `main`) in breadth-first ids.
struct Graph {
    offsets: Vec<u32>,
    edges: Vec<u32>,
    wbits: u32,
    big: u32,
    /// The weights of edges stored with the marker `big`, by `tail << 32 | head`.
    heavy: HashMap<u64, u64>,
}

/// Shortest distance from `s` to `t`: Ok(None) if `t` is unreachable, Err(()) if a distance did not
/// fit in `T`.
fn shortest<T: Copy + Into<u64> + TryFrom<u64> + PartialEq + Default>(
    g: &Graph,
    (s, t): (u32, u32),
    dist: &mut Dist<T>,
    heap: &mut RadixHeap<T>,
) -> Result<Option<u64>, ()> {
    // Heap keys are distances + 1, like the entries of `dist`.
    let mut search = || {
        let one = T::try_from(1).map_err(|_| ())?;
        dist.set(s, one);
        heap.push(one, s);
        while let Some((key, u)) = heap.pop() {
            let d = key.into() - 1;
            if u == t {
                return Ok(Some(d));
            }
            if key.into() > dist.get(u).into() {
                continue; // stale entry: u was already settled closer
            }
            for &e in &g.edges[g.offsets[u as usize] as usize..g.offsets[u as usize + 1] as usize] {
                let (v, w) = (e >> g.wbits, e & g.big);
                let w = if w == g.big {
                    g.heavy[&((u as u64) << 32 | v as u64)]
                } else {
                    w as u64
                };
                let nd = d + w;
                let old = dist.get(v);
                if old == T::default() || nd + 1 < old.into() {
                    let key = T::try_from(nd + 1).map_err(|_| ())?;
                    dist.set(v, key);
                    heap.push(key, v);
                    // v will probably be settled later: start loading its edges now.
                    // SAFETY: a prefetch never faults, whatever the address.
                    let at = g
                        .edges
                        .as_ptr()
                        .wrapping_add(g.offsets[v as usize] as usize);
                    unsafe { _mm_prefetch::<_MM_HINT_T0>(at as *const i8) };
                }
            }
        }
        Ok(None)
    };
    let answer = search();
    dist.reset();
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
    // An edge is one u32, `head << wbits | weight`, the head taking the bits the node count needs.
    // A weight that does not fit (0.07 % of the spec's edges) is stored as `big`, the all-ones
    // marker, and kept in a map by (tail, head) instead: parallel edges matter only through their
    // smallest weight, so that is the one kept.
    let wbits = 32u32.saturating_sub((n.max(2) - 1).ilog2() + 1);
    let big = (1u32 << wbits) - 1;

    // 1. One parse. Every thread files its edges into buckets by tail (`u >> SHIFT`, about 8k nodes
    //    per bucket), in file order, and marks in-edges in its own bitset: only sequential writes
    //    and a cache-sized bitset, instead of random atomics into node-sized arrays.
    //    has_in[v]: v has an in-edge from another node, so a path from some s != v can end at v.
    const SHIFT: usize = 13;
    let buckets = (n >> SHIFT) + 1;
    let parsed: Vec<(Vec<Vec<(u32, u32)>>, Vec<u64>, Vec<(u64, u64)>)> =
        std::thread::scope(|scope| {
            let workers: Vec<_> = chunks
                .iter()
                .map(|&chunk| {
                    scope.spawn(move || {
                        // An edge line is about 19 bytes; sizing the buckets up front avoids regrowing them.
                        let per_bucket = chunk.len() / 18 / buckets + 64;
                        let mut out: Vec<Vec<(u32, u32)>> = (0..buckets)
                            .map(|_| Vec::with_capacity(per_bucket))
                            .collect();
                        let (mut has_in, mut heavy) = (vec![0u64; n.div_ceil(64)], Vec::new());
                        let mut p = 0;
                        while p < chunk.len() {
                            let (u, v) =
                                (number(chunk, &mut p) as u32, number(chunk, &mut p) as u32);
                            let w = number(chunk, &mut p);
                            if u != v {
                                has_in[v as usize / 64] |= 1 << (v % 64);
                            }
                            if w >= big as u64 {
                                heavy.push(((u as u64) << 32 | v as u64, w));
                            }
                            out[u as usize >> SHIFT]
                                .push((u, v << wbits | w.min(big as u64) as u32));
                        }
                        (out, has_in, heavy)
                    })
                })
                .collect();
            workers.into_iter().map(|w| w.join().unwrap()).collect()
        });
    let has_in: Vec<u64> = (0..n.div_ceil(64))
        .map(|i| parsed.iter().fold(0, |acc, p| acc | p.1[i]))
        .collect();
    let mut heavy: HashMap<u64, u64> = HashMap::new();
    for &(key, w) in parsed.iter().flat_map(|p| &p.2) {
        heavy
            .entry(key)
            .and_modify(|x| *x = (*x).min(w))
            .or_insert(w);
    }

    // 2. The CSR in input ids: each bucket is counting-sorted into its own slice by one thread.
    let bucket_start =
        prefix_sums((0..buckets).map(|b| parsed.iter().map(|p| p.0[b].len() as u32).sum()));
    assert_eq!(
        bucket_start[buckets] as usize, m,
        "edge count differs from the header"
    );
    let mut offsets = vec![m as u32; n + 1];
    let mut edges = vec![0u32; m];
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
                        for &(u, _) in list {
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
                        for &(u, e) in list {
                            let slot = &mut cursor[u as usize - base];
                            my_edges[edge_base + *slot as usize] = e;
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
        if claimed[seed as usize].load(Ordering::Relaxed)
            || claimed[seed as usize].swap(true, Ordering::Relaxed)
        {
            return;
        }
        let mut head = region.len();
        region.push(seed);
        while head < region.len() {
            let u = region[head] as usize;
            head += 1;
            for v in edges[offsets[u] as usize..offsets[u + 1] as usize]
                .iter()
                .map(|e| e >> wbits)
            {
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
    let mut new_edges = vec![0u32; m];
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
                    for &e in
                        &edges[offsets[old as usize] as usize..offsets[old as usize + 1] as usize]
                    {
                        *out.next().unwrap() = rank[(e >> wbits) as usize] << wbits | e & big;
                    }
                }
            });
        }
    });
    drop((order, edges));
    let rename =
        |key: u64| (rank[(key >> 32) as usize] as u64) << 32 | rank[key as u32 as usize] as u64;
    let graph = Graph {
        offsets: new_offsets,
        edges: new_edges,
        wbits,
        big,
        heavy: heavy.into_iter().map(|(key, w)| (rename(key), w)).collect(),
    };

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
                let (mut dist, mut heap, mut wide) = (Dist::<u32>::new(n), RadixHeap::new(), None);
                loop {
                    let Some(&i) = by_source.get(next.fetch_add(1, Ordering::Relaxed) as usize)
                    else {
                        break;
                    };
                    let (s, t) = list[i];
                    let reachable = s == t || has_in[t as usize / 64] >> (t % 64) & 1 == 1;
                    if !reachable {
                        continue;
                    }
                    let query = (rank[s as usize], rank[t as usize]);
                    let answer = shortest(&graph, query, &mut dist, &mut heap).or_else(|_| {
                        let (dist, heap) =
                            wide.get_or_insert_with(|| (Dist::new(n), RadixHeap::new()));
                        shortest::<u64>(&graph, query, dist, heap)
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
