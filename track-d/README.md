# Track D: Traverse

The task, the input files and the exact output format are in the [spec](https://github.com/hackathon-raptors/speed-demon-2026/blob/main/specs/d-traverse.md).

## Approach

Rust, standard library only. `graph.txt` is memory-mapped and its edge lines are parsed in parallel chunks
that end on a newline; out-degrees are counted with atomic counters while parsing. A prefix sum gives a
compressed adjacency array (CSR), and every thread scatters its edges into their tail node's slice through an
atomic cursor. Parallel edges and duplicates are kept: Dijkstra relaxes each one and only the lightest can
improve a distance.

Queries run in parallel, one Dijkstra per query with a binary heap and lazy deletion, stopping as soon as the
target is settled. Each thread owns one `u64` distance array and resets only the nodes the last search
touched, so a short search costs what it visits. Distances are `u64` because answers exceed 2^32.

## Dependencies

None.

## With another 24 hours

See PERF.md.
