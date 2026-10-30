# Track A: Ingest

The task, the input files and the exact output format are in the [spec](https://github.com/hackathon-raptors/speed-demon-2026/blob/main/specs/a-ingest.md).

## Approach

Rust, standard library only. The input is memory-mapped once and cut into chunks that end on a newline;
twelve threads pull chunks from an atomic counter and parse lines by position (the timestamp is always 27
bytes, so the hour is the first 13 bytes, computed with integer calendar arithmetic and cached while the
prefix does not change). Each thread interns endpoints in its own FxHash table and appends one packed `u64`
per line: hour, endpoint id, 5xx flag and latency.

The per-thread endpoint tables are merged into one byte-sorted list, so the dense group id
`(hour - first hour) * endpoints + endpoint rank` is already the output order. A counting sort then gives
every thread its own slots, each thread scatters its latencies into one array where each group is
contiguous, and the three nearest-rank percentiles come from `select_nth_unstable` on shrinking prefixes
instead of a full sort. Groups are formatted in parallel batches and written in order.

## Dependencies

None.

## With another 24 hours

See PERF.md.
