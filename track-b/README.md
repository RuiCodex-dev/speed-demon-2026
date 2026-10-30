# Track B: Index

The task, the input files and the exact output format are in the [spec](https://github.com/hackathon-raptors/speed-demon-2026/blob/main/specs/b-index.md).

## Approach

Rust, standard library only. The queries are readable before the corpus, so only their terms are indexed.
`queries.txt` is parsed first and every distinct term is interned. `docs.txt` is memory-mapped and scanned
in parallel chunks of whole lines: each token is lowercased (ASCII only, every byte >= 0x80 is a separator,
exactly as the spec defines), looked up among the query terms, and recorded once per document as a
(term, document) posting. Each document's text range is kept as well.

A counting sort over the chunks, in chunk order, turns the postings into one array where each term's
documents are contiguous and ascending. BOOL queries apply AND (galloping intersection), OR (merge) and NOT
(difference) strictly left to right. A PHRASE intersects its terms' postings, rarest first, then re-tokenises
only the surviving candidate documents to check that the terms are adjacent and in order.

## Dependencies

None.

## With another 24 hours

See PERF.md.
