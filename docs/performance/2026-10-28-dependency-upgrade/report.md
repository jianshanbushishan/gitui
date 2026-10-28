> Archived experiment: the gix 0.90.0 / ratatui-textarea 0.9.3 migration was reverted after these measurements. Current production dependencies remain at gix 0.86.0 and ratatui-textarea 0.8.0, retaining the preceding dependency updates and pull error fix. The measurements below describe the experiment, not the current application.

# GitUI dependency performance comparison

The candidate is a mixed result: standalone gix status gets faster, while textarea editing, cursor movement and rendering regress in these fixtures. GitUI's active split-status path uses libgit2 and does not receive the standalone gix status speedup. The large textarea operations remain approximately 1–1.3 milliseconds each; the large ratios start from sub-microsecond or sub-millisecond baselines, and should not be described as seconds of UI freezing.

| Selected operation | Baseline median | Intermediate median | Candidate median | Candidate / intermediate | Independent confirmation |
|---|---:|---:|---:|---:|---|
| CJK middle insertion, small | 0.296 µs | 0.258 µs | 17.384 µs | 67.38× | 17.548 µs vs 0.252 µs; 69.63× |
| CJK middle insertion, large | 0.910 µs | 0.766 µs | 1,096.004 µs | 1,430.81× | 1,102.675 µs vs 0.579 µs; 1,904.45× |
| 100 cursor moves, small | 0.378 µs | 0.382 µs | 22.406 µs | 58.65× | 22.943 µs vs 0.386 µs; 59.44× |
| 100 cursor moves, large | 0.590 µs | 0.596 µs | 35.786 µs | 60.04× | 35.290 µs vs 0.587 µs; 60.12× |
| Render 120×40, small | 115.820 µs | 124.530 µs | 145.628 µs | 1.17× | 143.444 µs vs 128.387 µs; 1.12× |
| Render 120×40, large | 164.414 µs | 177.822 µs | 1,325.076 µs | 7.45× | 1,314.273 µs vs 177.663 µs; 7.40× |
| gix history, 300 commits | 1,272.090 µs | 1,271.640 µs | 1,371.310 µs | 1.078× | 1,450.023 µs vs 1,299.730 µs; 1.116× |

Primary standalone gix status medians improved 6.5–13.6% versus intermediate across all six cases, and 6.2–13.1% versus baseline. Split status medians ranged from 1.4% faster to 4.3% slower versus intermediate; these small changes, like the small git2 diff/metadata changes, should be treated cautiously on one machine. The 300-commit gix walk was 7.8% slower versus intermediate in the main run and 11.6% slower in confirmation, an absolute difference of about 0.10–0.15 ms. The direction repeated, but its exact size is uncertain.

The six additional confirmation triplets use each ordering once, the same immutable binaries, iterations, fixtures and in-process warmups, after 10 seconds idle. Raw stdout, samples and summaries live in `confirmation/`; none of the primary 21-round data was discarded or overwritten. Textarea regressions repeat by large margins, beyond the observed run variation. The exact sub-microsecond baseline ratios vary more than the candidate's absolute insertion time: in the primary run large insertion ranges were 0.704–1.514 µs (baseline), 0.688–3.210 µs (intermediate), and 1,075.636–1,261.822 µs (candidate). The six-round confirmation p95 is only the maximum and should not be treated as a precise tail estimate.

Static source evidence is consistent with the edit-size scaling: ratatui-textarea 0.9.3 `textarea.rs::push_history()` calls `refresh_screen_map()` and then `screen_map_load()`. `screen_map.rs` rebuilds row/data-pointer mappings across all lines and computes display widths even with `WrapMode::None`. This correlates with the timing and provides a plausible cause; no sampling profiler was run. The benchmark includes the complete public edit and mapping work but excludes construction and initial cursor placement. All timed and warmup operations retain an opaque reference to the resulting textarea state under LTO.

The small textarea contains 64 lines × 120 Unicode scalar values (240 UTF-8 bytes) per line: 7,680 values / 15,360 bytes total. The larger textarea contains 2,048 lines × 240 values (480 UTF-8 bytes): 491,520 values / 983,040 bytes, or 960 KiB total, excluding line separators and allocation overhead. Both mix ASCII, CJK and emoji. Insertion adds exactly 16 CJK characters at the middle row/column; cursor timing covers 50 forward/back pairs, rather than a single key event. Render timing includes a fresh 120×40 buffer but excludes terminal output.

Source provenance uses explicit `git -c core.autocrlf=true` only for source status/diff, matching the checkout's EOL policy. Fixtures retain isolated Git config and `core.autocrlf=false`. The initial sanitized source metadata made physical CRLF appear as changes in the intermediate checkout; original diagnostics are retained as `environment.initial-eol-diagnostics.json` and `*.initial-eol-diagnostics.patch`. Corrected source patches are empty for all three measured groups. Baseline/intermediate have only identical added harness files; the measured candidate was clean at 78a8d869. The post-measurement runner metadata fix does not change any measured example source or binary.

## Complete primary results

Times are microseconds per operation; negative change means faster.

| Operation | Baseline median | Intermediate median | Candidate median | Baseline→intermediate | Intermediate→candidate | Baseline→candidate | Baseline p95 | Intermediate p95 | Candidate p95 |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| status_split/clean | 3078.99 | 3049.42 | 3082.16 | -1.0% | +1.1% | +0.1% | 3401.39 | 3254.59 | 3426.61 |
| status_gix/clean | 6417.86 | 6330.05 | 5809.85 | -1.4% | -8.2% | -9.5% | 7071.60 | 6682.92 | 6306.01 |
| status_split/dirty | 3007.84 | 3011.80 | 3108.33 | +0.1% | +3.2% | +3.3% | 3241.84 | 3184.86 | 3407.75 |
| status_gix/dirty | 6432.67 | 6333.29 | 5873.15 | -1.5% | -7.3% | -8.7% | 7057.77 | 7000.33 | 6328.88 |
| status_split/untracked | 3832.08 | 3748.41 | 3845.34 | -2.2% | +2.6% | +0.3% | 4029.52 | 3970.75 | 4541.72 |
| status_gix/untracked | 6949.60 | 6993.99 | 6042.07 | +0.6% | -13.6% | -13.1% | 7393.87 | 7310.02 | 6743.83 |
| status_split/staged | 2817.52 | 2889.95 | 2850.85 | +2.6% | -1.4% | +1.2% | 3039.01 | 2977.80 | 3098.15 |
| status_gix/staged | 6097.78 | 6118.58 | 5722.12 | +0.3% | -6.5% | -6.2% | 6872.64 | 6666.63 | 6113.96 |
| status_split/large_clean | 11020.68 | 10879.45 | 11348.14 | -1.3% | +4.3% | +3.0% | 12005.24 | 12386.19 | 12277.69 |
| status_gix/large_clean | 14455.31 | 14419.92 | 13439.51 | -0.2% | -6.8% | -7.0% | 15063.68 | 15355.14 | 15603.05 |
| status_split/large_dirty | 10953.69 | 10821.03 | 11107.07 | -1.2% | +2.6% | +1.4% | 11508.07 | 11702.60 | 12099.13 |
| status_gix/large_dirty | 13932.81 | 14022.40 | 12593.32 | +0.6% | -10.2% | -9.6% | 14994.40 | 15236.36 | 14323.85 |
| diff/workdir | 2795.92 | 2770.08 | 2764.76 | -0.9% | -0.2% | -1.1% | 3009.05 | 3202.39 | 2952.95 |
| diff/staged | 1773.15 | 1837.55 | 1815.60 | +3.6% | -1.2% | +2.4% | 1877.37 | 1946.49 | 1879.20 |
| history/gix_walk_300 | 1272.09 | 1271.64 | 1371.31 | -0.0% | +7.8% | +7.8% | 1413.18 | 1385.35 | 1548.96 |
| history/git2_metadata_300 | 1511.63 | 1523.59 | 1498.33 | +0.8% | -1.7% | -0.9% | 1591.78 | 1622.67 | 1731.55 |
| textarea/small/middle_cjk_insert_16 | 0.30 | 0.26 | 17.38 | -12.8% | +6638.0% | +5773.0% | 0.31 | 0.28 | 20.38 |
| textarea/small/cursor_100 | 0.38 | 0.38 | 22.41 | +1.1% | +5765.4% | +5827.5% | 0.39 | 0.40 | 26.00 |
| textarea/small/render_120x40 | 115.82 | 124.53 | 145.63 | +7.5% | +16.9% | +25.7% | 124.59 | 131.79 | 160.90 |
| textarea/large/middle_cjk_insert_16 | 0.91 | 0.77 | 1096.00 | -15.8% | +142981.5% | +120340.0% | 1.20 | 1.04 | 1255.68 |
| textarea/large/cursor_100 | 0.59 | 0.60 | 35.79 | +1.0% | +5904.4% | +5965.4% | 0.62 | 0.66 | 40.61 |
| textarea/large/render_120x40 | 164.41 | 177.82 | 1325.08 | +8.2% | +645.2% | +705.9% | 175.02 | 207.66 | 1364.37 |

Method: two discarded process triplets, three in-process warmups per operation; rotated and reversed three-version process order (six permutations); 21 measured rounds. p95 is nearest-rank over round means, not individual-call tail latency. Fixture setup, process startup, textarea construction/cursor placement, and compilation are excluded. Rendering includes a fresh 120x40 ratatui buffer but no terminal I/O. All runs use the same disposable fixtures and warm filesystem cache. Checksums agree across variants. No network or user-project operations. See environment.json for SHAs, binary hashes, compiler, profile and command.

Limitations: one Windows machine; filesystem/cache/antivirus/background activity can add noise; no cold-cache or interactive end-to-end UI latency; dependency cohort plus required compatibility edits are compared together; this does not isolate a single crate's effect. Small synthetic repositories and linear history do not represent every workload.

Fixtures: 2,001 tracked files for clean/dirty/staged/untracked cases; 10,001 for large clean/dirty cases. Dirty/staged cases contain 80 changed files; untracked contains 200 files across 20 directories. Each repository contains 300 deterministic commits. Textareas use 64 lines × 8 repeats and 2,048 lines × 16 repeats of mixed ASCII, CJK and emoji, with cursor placement in the middle. Cursor benchmark performs 50 forward/back pairs. History walking uses gix; commit metadata uses GitUI's git2-backed get_commits_info API. The exact source, binary hashes and fixture commit IDs are retained with the raw results.

## Build and reproduction

Compiler: `rustc 1.99.1 (58cf8f9c3 2026-10-13)`.
baseline HEAD: `061eef1bde8d2363600f5d0b7ab1add43ee8ad88`; intermediate HEAD: `1c254d91a5e03eff5e2997bd030ac98694d7857f`; candidate HEAD: `78a8d869bfc18cbc6cf3c6e75d9d42e6026a3383`. Working-tree patches are saved alongside environment.json.
Versions: baseline: gix 0.86.0, git2 0.21.0, ratatui 0.30.0, ratatui-textarea 0.8.0, image 0.25.10, resvg 0.48.1, ron 0.12.2; intermediate: gix 0.86.0, git2 0.21.0, ratatui 0.30.2, ratatui-textarea 0.8.0, image 0.25.10, resvg 0.48.1, ron 0.12.2; candidate: gix 0.90.0, git2 0.21.0, ratatui 0.30.2, ratatui-textarea 0.9.3, image 0.25.10, resvg 0.48.1, ron 0.12.2.

Build in each checkout, preserving the same environment:
```powershell
cargo build --release --locked -p asyncgit --example perf_compare --features vendor-openssl
cargo build --release --locked -p gitui --example perf_textarea
```

Run:
```powershell
C:\Users\jians\.local\bin\python.exe scripts/perf_compare.py run --output target/perf-20261028 --baseline-root C:/Users/jians/.codex/worktrees/dependency-perf-baseline/gitui --intermediate-root C:/Users/jians/.codex/worktrees/dependency-perf-intermediate/gitui --candidate-root D:/private/code2read/gitui --rounds 21
```

The original binaries are preserved under binaries/baseline, binaries/intermediate and binaries/candidate before execution. Installed GitUI is never replaced.

## Validation and retained artifacts

Default regex-fancy and alternate regex-onig workspace tests each passed 547 tests, with 3 pre-existing ignored tests. Clippy and the optimized application build passed. The GIT_DIR environment test now runs its assertion in a child process to prevent cross-test repository redirection.

The installed application remains gitui 0.30.0-1c254d91 because this comparison identified reproducible textarea regressions. The migrated application build is preserved as binaries/candidate/gitui.exe (version 0.30.0-38fdd115). This application build predates only benchmark-harness observability/provenance commits; production migration code is 67c9bb25. The measured helper binaries include the observability barrier and their exact hashes are in environment.json.

Identical Rust harness snapshots and the corrected runner are preserved in harness/. All measured binaries and disposable fixtures remain in this artifact directory. The baseline managed checkout was queued for archival after measurement; restore its attached archive, or create a checkout at 061eef1b and copy the same harness, before rebuilding/rerunning the documented three-root command. The intermediate checkout is retained by the application as the primary checkout and was not removed. No user project was modified.
