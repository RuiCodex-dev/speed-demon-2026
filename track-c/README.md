# Track C: Join

The task, the input files and the exact output format are in the [spec](https://github.com/hackathon-raptors/speed-demon-2026/blob/main/specs/c-join.md).

## Approach

Rust, standard library only.

1. **Regions.** `regions.csv` is read in one pass. Country codes are three capital letters, so a code read
   as a base-26 number indexes a 17,576-entry table that lists the codes already in sorted order; region
   ids map to a country index, one byte per region when there are fewer than 255 countries (1 MB, which
   stays in the scoring CPU's L2 cache).
2. **Splitting a hostile CSV safely.** A cut can land inside a quoted field that contains line breaks. The
   generator writes a CR only as the first byte of a CRLF terminator and never inside quotes (spec 2.2,
   which also holds for the holdout data), so every chunk starts after the first CR past its cut, with no
   pass over the file before parsing.
3. **Parsing.** Each thread parses whole records with a small byte state machine: an optional BOM at a
   record start, quoted or unquoted text (quotes found 32 bytes at a time), `""` escapes, CRLF or LF. Numbers are
   read 8 bytes at a time (SWAR).
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

- About half the time is the atomic OR into the (country, name) bitsets and the name comparison, both
  cache misses into multi-megabyte tables. Partitioning names between threads after parsing would make
  the bitsets private, at the price of storing the (country, name) pairs once.
