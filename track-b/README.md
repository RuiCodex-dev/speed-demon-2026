# Track B: Index

The task, the input files and the exact output format are in the [spec](https://github.com/hackathon-raptors/speed-demon-2026/blob/main/specs/b-index.md).

## Approach

Rust, standard library only. The queries are readable before the corpus, so only their terms are indexed,
and the whole corpus is read exactly once.

1. **Queries first.** `queries.txt` is parsed and every distinct term interned (about 76k). The phrase
   queries become an Aho-Corasick automaton over term ids: transitions from the root are a dense array,
   the rest a hash map guarded by a 64-bit mask of each state's child terms, so a lookup that would fail
   is usually skipped.
2. **Which terms are frequent.** A sample of about 1/64 of the corpus, spread over the file, estimates each
   BOOL term's document frequency. Terms in more than 1/32 of the documents get a bitmap over all
   documents instead of a posting list. A wrong guess only changes the representation, never an answer.
3. **One parallel scan.** `docs.txt` is memory-mapped and split into chunks of whole lines. A table-driven
   tokenizer lowercases ASCII and treats every byte >= 0x80 as a separator, exactly as the spec says, and
   hashes each token while it scans it. The token is looked up in an open-addressing vocabulary whose slots
   carry a hash tag and a length, so most non-terms are rejected inside the table. For each document, a
   BOOL term records a posting (or sets its bit in a bitmap local to the chunk), and the automaton reports
   every phrase that occurs. Both are deduplicated per document.
4. **Lists.** Postings and phrase matches are counting-sorted in chunk order, one term range per thread,
   so every list comes out sorted by document id. Chunk bitmaps are merged with one atomic OR per word.
5. **Answers.** A BOOL query that touches a bitmap term is evaluated on bitmaps; any other one is a chain of
   sorted-list merges (galloping intersection, union, difference), strictly left to right. A phrase answer
   is the automaton's list for that phrase. Count, sum and the ten smallest ids are written in order.

Every step and its measured effect, including what did not work, is in [PERF.md](PERF.md).

## Dependencies

None.

## With another 24 hours

- Make the posting lists smaller: delta + varint encoding would cut the memory of the medium-frequency
  terms, which are now most of the peak.
- Prefetch vocabulary slots a few tokens ahead: the remaining scan time is mostly cache misses in the
  vocabulary and the automaton.
- Trim the source: several helpers exist in two shapes (bitmap and list paths) and could share code.
