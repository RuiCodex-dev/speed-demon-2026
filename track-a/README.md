# Track A: Ingest

The task, the input files and the exact output format are in the [spec](https://github.com/hackathon-raptors/speed-demon-2026/blob/main/specs/a-ingest.md).

## Approach

Rust, standard library only.

1. **Read nothing twice.** `requests.log` is memory-mapped, never copied: the scoring harness pre-warms it
   into the page cache, so the mapping costs no memory of our own. It is cut into one chunk per thread at
   line boundaries.
2. **Parse by position.** A timestamp is always 27 bytes, so the hour is the first 13 bytes; it is
   recomputed with integer calendar arithmetic (no date library) only when that prefix changes, which is
   rare because each shard writes in time order. Spaces are found 8 bytes at a time with the classic
   zero-byte test. Endpoints are interned in a small per-thread open-addressing table that hashes and
   compares them 8 bytes at a time with masked loads, so there are no `memcpy`/`memcmp` calls per line.
   Each line becomes one packed `u64`: hour, endpoint id, 5xx flag, latency.
3. **Group without copying.** The per-thread endpoint tables are merged into one byte-sorted list, so the
   group id `(hour - first hour) * endpoints + endpoint rank` is already the output order. Each thread
   rewrites its records in place as `group << 33 | 5xx << 32 | latency` and sorts them, so every group is
   one run per chunk and no second per-line array exists.
4. **Percentiles without full sorts.** Output ranges of groups are formatted in parallel: a group's runs are
   gathered from the 12 sorted chunks and the three nearest-rank percentiles come from
   `select_nth_unstable` on shrinking prefixes.

Every step and its measured effect, including what did not work, is in [PERF.md](PERF.md).

## Dependencies

None.

## With another 24 hours

- The sort is still about a fifth of the time; a radix partition on the hour while parsing (records are
  nearly in time order per shard) could replace most of it without a second buffer.
- Memory is now the per-line records (8 bytes each): storing latencies as 4 bytes in per-hour runs would
  halve it.
