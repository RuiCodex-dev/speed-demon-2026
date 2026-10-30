# Track B: performance log

Every step from the reference to the final version, each with the live-board run ID that measured it.
Local numbers come from a laptop (i9-13900H, WSL, turbo on) and are only compared with each other.

| Step | Change | Run ID | T (s) | M (MB) | Lines | Points |
|---|---|---|---|---|---|---|
| 0 | The reference, unchanged | | 56.35 | 2200 | 91 | 0 |
| 1 | Rust: query-term-only index, parallel scan, counting sort, merges; phrases verified on candidates | | | | | |

## Step notes

### Step 1
Local: 2.01 s wall, output sha256 matches the reference (`f86973c2…`).
