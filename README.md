# Speed Demon 2026 - Rui Codex

Our entry for [Speed Demon 2026](https://speeddemon.dev): all four tracks, in Rust, using the standard library only
(no crates, so D = 1 on every track). Each `track-*` folder is built and scored on its own.

| Track | Task | Approach in one line | Details |
|---|---|---|---|
| A Ingest | per (hour, endpoint) counts, p50/p95/p99, 5xx rate over 3.1M log lines | mmap, positional parse, packed records sorted per thread, `select_nth_unstable` | [README](track-a/README.md) · [PERF](track-a/PERF.md) |
| B Index | 50,000 boolean and phrase queries over a 254 MB corpus | index only the query terms; phrases matched during the one scan by an Aho-Corasick automaton; bitmaps for frequent terms | [README](track-b/README.md) · [PERF](track-b/PERF.md) |
| C Join | hostile CSV joined to a lookup table, sorted aggregates | quote-parity chunking, byte state machine, one lock-free name table, atomic (country, name) bits | [README](track-c/README.md) · [PERF](track-c/PERF.md) |
| D Traverse | 2,000 shortest-path queries on a 5M-edge graph | bucketed CSR build, BFS renumbering for locality, radix-heap Dijkstra with early stop | [README](track-d/README.md) · [PERF](track-d/PERF.md) |

Every optimisation step in the PERF files cites the live-board run that measured it, and the experiments
that did not pay off are there too, with their numbers.

## Build and check

Each track builds offline from its own folder with an allowlisted image (see the track's `Dockerfile`).
`check.py` from the organisers' starter builds a track the way the board does and compares the output hash:

```
python3 check.py a --data <folder with requests.log> --expected <sha256 of the reference's summary.txt>
```

## How it was made

Built during the event window with AI coding assistance (Claude Code). Every change was measured on the
live board and with local A/B runs, and reviewed before it was kept.

## Licence

MIT, see [LICENSE](LICENSE). The organisers' starter template is also MIT.
