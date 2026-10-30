# Track C: Join

The task, the input files and the exact output format are in the [spec](https://github.com/hackathon-raptors/speed-demon-2026/blob/main/specs/c-join.md).

## Approach

Rust, standard library only.

1. **Regions.** `regions.csv` is read in one pass into a dense array from region id to a country index;
   country codes are renumbered in byte order, with `_UNMATCHED` last.
2. **Splitting a hostile CSV safely.** A cut can land inside a quoted field that contains line breaks. The
   spec makes quote parity exact (quotes only appear in quoted fields, and `""` escapes come in pairs), so
   twelve threads count the `"` bytes of each chunk, a prefix sum says whether each cut is inside quotes,
   and every chunk starts after its first line break outside quotes.
3. **Parsing.** Each thread parses whole records with a small byte state machine: an optional BOM at a
   record start, quoted or unquoted text (quotes found 8 bytes at a time), `""` escapes, CRLF or LF.
   Name and note lengths are the raw length minus one per `""`, so text is only unescaped for names that
   need it. Totals per country are kept per thread and summed at the end.
4. **Distinct names, with nothing stored per record.** One lock-free table shared by all threads maps a
   name to an id: a slot holds a hash tag and the id, and is claimed with a compare-and-swap. Each
   distinct name is copied once into a compact arena, so the exact byte comparison reads memory that
   stays in cache instead of a random place in the 200 MB input. Each record sets one atomic bit for
   (country, name id); the distinct count of a country is a popcount. The table is sized for the spec's
   pool of 100,000 names but does not rely on it: if it fills up, the run is repeated with a table four
   times larger. The big arrays come from `calloc`, so pages that are never written cost no memory.

Every step and its measured effect, including what did not work, is in [PERF.md](PERF.md).

## Dependencies

None.

## With another 24 hours

- Process records in small batches and prefetch the name-table slots of the whole batch, to overlap the
  cache misses that are now most of the time.
- Store country bitsets per thread for the most frequent countries, to avoid atomic traffic on their
  cache lines.
