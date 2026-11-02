# Track D: Traverse

The task, the input files and the exact output format are in the [spec](https://github.com/hackathon-raptors/speed-demon-2026/blob/main/specs/d-traverse.md).

## Approach

Rust, standard library only.

1. **One parse, cache-friendly.** `graph.txt` is memory-mapped and split among the threads, and numbers
   are read 8 bytes at a time (SWAR). Each thread files its edges into buckets by tail id (32k nodes per
   bucket) with sequential writes, and marks nodes that have an in-edge from another node in its own
   small bitset. Each bucket is counting-sorted into its slice of a compressed adjacency array (CSR) by
   one thread, inside the cache, and its memory is handed back as soon as it is placed. An edge is one
   `u32`, `head << 11 | weight`; the 0.07 % of weights that do not fit live in a small map.
2. **Locality.** The input ids are a random relabelling, so a search would miss the cache on every node.
   Nodes are renumbered in breadth-first order (twelve searches from different seeds grow regions in
   parallel and claim nodes with an atomic flag), and the CSR is copied into the new numbering in
   parallel, with the nodes' rows prefetched a few nodes ahead.
3. **Queries.** One Dijkstra per query, in parallel, in order of the source's new id so the threads
   share the cache, with an early stop when the target is settled. The priority queue is a ring of
   2,048 buckets (every packed weight is below 2,047), so a push is one 4-byte append and each entry
   moves once; after each pop, the next nodes in the bucket have their edges and distances prefetched. A
   target with no in-edge from another node is answered `-1` without a search. Distances are `u32`
   "distance + 1" in 1,024-node pages taken from a per-thread pool, so a thread holds only the pages its
   current search touched; a search whose distances do not fit in 32 bits (the spec has weights near
   2^32) is repeated exactly with `u64`.

Every step and its measured effect, including what did not work, is in [PERF.md](PERF.md).

## Dependencies

None.

## With another 24 hours

- A bidirectional search settles 42 % fewer nodes, but the reverse graph and a second distance pool would
  raise the memory peak by about half, which the formula prices higher than the 1.14x speed-up measured
  (see PERF.md). A compact reverse graph built while the forward one is renumbered might change that.
- Most of the query time is now misses on a neighbour's distance and on the next node's offsets.
