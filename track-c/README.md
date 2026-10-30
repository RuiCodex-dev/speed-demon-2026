# Track C: Join

The task, the input files and the exact output format are in the [spec](https://github.com/hackathon-raptors/speed-demon-2026/blob/main/specs/c-join.md).

## Approach

Rust, standard library only. `regions.csv` becomes a dense array from region id to a country index, with the
countries sorted bytewise and `_UNMATCHED` last.

`records.csv` is memory-mapped and split for twelve threads. Splitting a hostile CSV is the risky part:
a cut can land inside a quoted field that contains newlines. The spec makes quote parity exact (quotes only
appear in quoted fields, and `""` escapes come in pairs), so a parallel count of `"` per chunk and a prefix
sum tell every cut whether it sits inside quotes; the chunk then starts after its first line break outside
quotes. Each thread parses whole records with a small byte state machine (BOM at a record start, quoted or
unquoted text, `""` escapes, CRLF or LF) and keeps per-country totals. Byte counts are the raw length minus
one per `""` pair, so the unescaped text is only materialised for the rare name that needs it.

For `names_distinct`, every thread interns names into its own table; the tables are merged into global ids
and one bitset per country is OR-ed, then counted with `popcount`.

## Dependencies

None.

## With another 24 hours

See PERF.md.
