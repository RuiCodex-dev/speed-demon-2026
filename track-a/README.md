# Track A: Ingest

The task, the input files and the exact output format are in the [spec](https://github.com/hackathon-raptors/speed-demon-2026/blob/main/specs/a-ingest.md).

## Approach

Rust, standard library only.

1. **Read nothing twice.** `requests.log` is memory-mapped, never copied: the scoring harness pre-warms it
   into the page cache, so the mapping costs no memory of our own. It is cut into one chunk per thread at
   line boundaries.
2. **Parse by position.** A timestamp is always 27 bytes, so the hour is the first 13 bytes, compared
   as two 8-byte words and recomputed with integer calendar arithmetic (no date library) only when it
   changes. Spaces are found 32 bytes at a time (AVX2). Endpoints are interned in a small per-thread
   open-addressing table, hashed from their first and last 8 bytes and compared 32 bytes at a time with
   masked loads that never read past the line. The latency is read 8 bytes at a time (SWAR). Each line
   becomes one packed `u64`: hour, endpoint id, 5xx flag, latency.
3. **Little memory in any phase.** Each thread parses its chunk in 4 MB pieces; a piece's records are
   sorted by (hour, endpoint) and reduced to 4-byte latencies plus one run per group before the next
   piece is read. After the per-thread endpoint tables are merged into one byte-sorted list, every run
   gets its group id `(hour - first hour) * endpoints + endpoint rank`, which is already the output order.
4. **Percentiles without full sorts.** Ranges of groups are summarised in parallel: a group's runs are
   gathered from every piece and the three nearest-rank percentiles come from `select_nth_unstable` on
   shrinking prefixes. The latencies are freed before any output text exists, because the output's page
   cache counts as memory.

Every step and its measured effect, including what did not work, is in [PERF.md](PERF.md).

## Dependencies

None.

## With another 24 hours

- Parsing is now bound on reading the input: profile the per-line work again (the endpoint table is
  still about a third of it) and try a layout of the table that needs one cache line per lookup.
- The output (10 MB) is written by one thread at the end; overlapping it with the formatting would
  remove most of that time.
