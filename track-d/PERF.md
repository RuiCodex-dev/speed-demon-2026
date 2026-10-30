# Track D: performance log

Every step from the reference to the final version, each with the live-board run ID that measured it.
Local numbers come from a laptop (i9-13900H, WSL, turbo on) and are only compared with each other.

| Step | Change | Run ID | T (s) | M (MB) | Lines | Points |
|---|---|---|---|---|---|---|
| 0 | The reference, unchanged | | 72.69 | 893 | 43 | 0 |
| 1 | Rust: mmap, parallel parse, atomic-count CSR, per-thread Dijkstra with early stop | | | | | |

## Step notes

### Step 1
Local: 2.31 s wall, output sha256 matches the reference (`a23dbdf0…`).
