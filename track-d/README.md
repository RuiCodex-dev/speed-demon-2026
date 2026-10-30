# Track D: Traverse

The task, the input files and the exact output format are in the [spec](https://github.com/hackathon-raptors/speed-demon-2026/blob/main/specs/d-traverse.md).

## Approach

Rust, standard library only.

1. **One parse, cache-friendly.** `graph.txt` is memory-mapped and split among the threads. Each thread
   files its edges into buckets by tail id (about 8k nodes per bucket) with sequential writes, and marks
   nodes that have an in-edge from another node in its own small bitset. Each bucket is then
   counting-sorted into its slice of a compressed adjacency array (CSR) by one thread, inside the cache.
2. **Locality.** The input ids are a random relabelling, so a search would miss the cache on every node.
   Nodes are renumbered in breadth-first order (twelve searches from different seeds grow regions in
   parallel and claim nodes with an atomic flag), and the CSR is copied into the new numbering.
3. **Queries.** One Dijkstra per query, in parallel, with a radix heap (keys never decrease) and an early
   stop when the target is settled. A target with no in-edge from another node is answered `-1` without a
   search: otherwise those queries would explore the whole graph. Each thread keeps one distance array of
   `u32` "distance + 1" (0 = not reached) straight from `calloc`; a search whose distances do not fit in
   32 bits (the spec has weights near 2^32) is repeated exactly with a `u64` array.

Every step and its measured effect, including what did not work, is in [PERF.md](PERF.md).

## Dependencies

None.

## With another 24 hours

- A bidirectional search settles 42 % fewer nodes and was 1.14x faster, but the reverse graph and the
  second distance array cost 112 MB more, which the formula prices higher (see PERF.md). With the
  memory brought down (shared distance arrays, a compact reverse graph) it would win.
- Prefetch the next settled node's edges: most of the query time is cache misses on edges and distances.
