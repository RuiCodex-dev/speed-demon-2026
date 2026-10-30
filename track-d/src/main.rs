//! Speed Demon 2026, Track D (Traverse).
//!
//! 1. Map `graph.txt` and parse the edge lines in parallel chunks.
//! 2. Build a compressed adjacency array (CSR): out-degrees with atomic counters, a prefix sum,
//!    then every thread scatters its edges into their tail's slice.
//! 3. Answer the queries in parallel: one Dijkstra per query with a binary heap, stopping when the
//!    target is settled. Each thread keeps one distance array and resets only the nodes it touched.
use std::cmp::Reverse;
use std::collections::BinaryHeap;
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

#[derive(Clone, Copy)]
struct SharedPtr(*mut (u32, u32));
// SAFETY: each edge slot is claimed exactly once through an atomic cursor.
unsafe impl Send for SharedPtr {}
unsafe impl Sync for SharedPtr {}

/// Shortest distance from `s` to `t`, or None. `dist` is all-MAX on entry and on return.
fn shortest(
    offsets: &[u32],
    edges: &[(u32, u32)],
    s: u32,
    t: u32,
    dist: &mut [u64],
    touched: &mut Vec<u32>,
) -> Option<u64> {
    let mut heap = BinaryHeap::new();
    dist[s as usize] = 0;
    touched.push(s);
    heap.push(Reverse((0u64, s)));
    let mut answer = None;
    while let Some(Reverse((d, u))) = heap.pop() {
        if d > dist[u as usize] {
            continue; // stale entry: u was already settled closer
        }
        if u == t {
            answer = Some(d);
            break;
        }
        for &(v, w) in &edges[offsets[u as usize] as usize..offsets[u as usize + 1] as usize] {
            let nd = d + w as u64;
            if nd < dist[v as usize] {
                if dist[v as usize] == u64::MAX {
                    touched.push(v);
                }
                dist[v as usize] = nd;
                heap.push(Reverse((nd, v)));
            }
        }
    }
    for &v in touched.iter() {
        dist[v as usize] = u64::MAX;
    }
    touched.clear();
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
    let chunks = split_lines(&graph[pos..], threads * 4);

    // Pass 1: parse every chunk into (tail, head, weight) and count out-degrees.
    let degree: Vec<AtomicU32> = (0..n + 1).map(|_| AtomicU32::new(0)).collect();
    // has_in[v]: v has an in-edge from another node, so a path from some s != v can end at v.
    let has_in: Vec<std::sync::atomic::AtomicU64> =
        (0..n.div_ceil(64)).map(|_| Default::default()).collect();
    let parsed: Vec<Vec<(u32, u32, u32)>> = std::thread::scope(|scope| {
        let workers: Vec<_> = chunks
            .iter()
            .map(|&chunk| {
                let (degree, has_in) = (&degree, &has_in);
                scope.spawn(move || {
                    let mut out = Vec::with_capacity(chunk.len() / 16);
                    let mut p = 0;
                    while p < chunk.len() {
                        let (u, v, w) = (
                            number(chunk, &mut p),
                            number(chunk, &mut p),
                            number(chunk, &mut p),
                        );
                        degree[u as usize].fetch_add(1, Ordering::Relaxed);
                        if u != v {
                            has_in[v as usize / 64].fetch_or(1 << (v % 64), Ordering::Relaxed);
                        }
                        out.push((u as u32, v as u32, w as u32));
                    }
                    out
                })
            })
            .collect();
        workers.into_iter().map(|w| w.join().unwrap()).collect()
    });
    assert_eq!(
        parsed.iter().map(Vec::len).sum::<usize>(),
        m,
        "edge count differs from the header"
    );

    // CSR: offsets[u]..offsets[u+1] are u's out-edges. `cursor` hands out slots during the scatter.
    let mut offsets = vec![0u32; n + 1];
    let mut total = 0u32;
    for u in 0..n {
        offsets[u] = total;
        total += degree[u].load(Ordering::Relaxed);
    }
    offsets[n] = total;
    let cursor: Vec<AtomicU32> = offsets.iter().map(|&o| AtomicU32::new(o)).collect();
    let mut edges = vec![(0u32, 0u32); m];
    let edges_ptr = SharedPtr(edges.as_mut_ptr());
    std::thread::scope(|scope| {
        for part in &parsed {
            let cursor = &cursor;
            scope.spawn(move || {
                let ptr = edges_ptr;
                for &(u, v, w) in part {
                    let slot = cursor[u as usize].fetch_add(1, Ordering::Relaxed) as usize;
                    // SAFETY: fetch_add gives every edge a distinct slot inside u's range.
                    unsafe { *ptr.0.add(slot) = (v, w) };
                }
            });
        }
    });
    drop(parsed);

    // Queries, answered in parallel; answers land in their input order.
    let mut list = Vec::new();
    let mut p = 0;
    while p < queries.len() {
        list.push((
            number(queries, &mut p) as u32,
            number(queries, &mut p) as u32,
        ));
    }
    let answers: Vec<AtomicU64Opt> = (0..list.len()).map(|_| AtomicU64Opt::default()).collect();
    let next = AtomicUsize::new(0);
    std::thread::scope(|scope| {
        for _ in 0..threads.min(list.len()) {
            scope.spawn(|| {
                let mut dist = vec![u64::MAX; n];
                let mut touched = Vec::new();
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some(&(s, t)) = list.get(i) else { break };
                    let reachable = s == t
                        || has_in[t as usize / 64].load(Ordering::Relaxed) >> (t % 64) & 1 == 1;
                    let answer = if reachable {
                        shortest(&offsets, &edges, s, t, &mut dist, &mut touched)
                    } else {
                        None
                    };
                    answers[i]
                        .0
                        .store(answer.unwrap_or(u64::MAX), Ordering::Relaxed);
                }
            });
        }
    });
    let mut out = String::with_capacity(list.len() * 12);
    for answer in &answers {
        match answer.0.load(Ordering::Relaxed) {
            u64::MAX => out.push_str("-1\n"),
            d => out.push_str(&format!("{d}\n")),
        }
    }
    std::fs::write(format!("{out_dir}/distances.txt"), out).expect("cannot write output");
}

/// An answer slot; u64::MAX means unreachable (a real distance is below 2^63).
#[derive(Default)]
struct AtomicU64Opt(std::sync::atomic::AtomicU64);
