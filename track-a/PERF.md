# Track A: performance log

Every step from the reference to the final version, each with the live-board run ID that measured it.
Local numbers come from a laptop (i9-13900H, WSL, turbo on) and are only compared with each other.

| Step | Change | Run ID | T (s) | M (MB) | Lines | Points |
|---|---|---|---|---|---|---|
| 0 | The reference, unchanged | | 32.05 | 228 | 56 | 0 |
| 1 | Rust: mmap, 12 threads, packed records, counting sort, `select_nth_unstable` | | | | 323 | |

## Step notes

### Step 1
Local: 0.10 s wall, output sha256 matches the reference (`495ca096…`).
