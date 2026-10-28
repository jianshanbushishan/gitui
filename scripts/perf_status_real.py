#!/usr/bin/env python3
"""Measure real status paths without modifying the target repository's index.

Build: cargo build --release --locked -p asyncgit --example perf_status_real --features vendor-openssl
Run: python scripts/perf_status_real.py REPO OUTPUT [--blocks 3 --rounds 9]
The first two samples in each process are warmups. This is a clean-repository
diagnostic: it refuses a dirty initial state and validates all result counts.
"""
import argparse
import csv
import hashlib
import io
import json
import math
import os
from pathlib import Path
import platform
import shutil
import statistics
import subprocess
import time


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("repo", type=Path)
    parser.add_argument("output", type=Path)
    parser.add_argument("--blocks", type=int, default=3)
    parser.add_argument("--rounds", type=int, default=9)
    parser.add_argument("--binary", type=Path, default=Path("target/release/examples/perf_status_real.exe"))
    args = parser.parse_args()
    assert args.rounds > 2 and args.blocks > 0
    repo, output = args.repo.resolve(), args.output.resolve()
    assert not output.is_relative_to(repo), "Store measurements outside the target repository"
    output.mkdir(parents=True, exist_ok=False)
    env = {k: v for k, v in os.environ.items() if not k.upper().startswith("GIT_")}
    env["GIT_OPTIONAL_LOCKS"] = "0"

    def git(*arguments):
        return subprocess.check_output(["git", "--no-optional-locks", "-C", str(repo), *arguments], env=env, timeout=120)

    gitdir = Path(git("rev-parse", "--absolute-git-dir").decode().strip())
    index = gitdir / "index"
    assert not (gitdir / "index.lock").exists(), "Another process holds the index"

    def snapshot():
        return {"head": git("rev-parse", "HEAD").decode().strip(),
                "index_sha256": digest(index), "index_mtime_ns": index.stat().st_mtime_ns,
                "config_sha256": digest(gitdir / "config"),
                "status_hex": git("status", "--porcelain=v1", "-z", "--untracked-files=all").hex()}

    before = snapshot()
    assert not before["status_hex"], "This experiment requires a clean repository"
    config = subprocess.run(["git", "-C", str(repo), "config", "--get", "status.showUntrackedFiles"], env=env, capture_output=True, text=True, check=False)
    assert config.returncode in (0, 1), config.stderr
    assert config.stdout.strip() not in ("no", "normal"), "Raw comparisons assume GitUI's All setting"
    metadata = {"repo": str(repo), "before": before, "tracked_files": len(git("ls-files", "-z").split(b"\0")) - 1,
                "index_bytes": index.stat().st_size,
                "pack_bytes": sum(p.stat().st_size for p in (gitdir / "objects/pack").glob("*.pack")),
                "git": git("--version").decode().strip(), "platform": platform.platform(),
                "binary_sha256": digest(args.binary), "blocks": args.blocks, "rounds": args.rounds,
                "warmups_per_process": 2, "status_show_untracked_files": config.stdout.strip() or "unset (GitUI defaults to All)",
                "started_utc": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())}
    (output / "environment.json").write_text(json.dumps(metadata, indent=2), encoding="utf8")
    modes = ["production", "async", "refresh_pipeline", "raw", "reuse", "no_update", "normal", "tracked_only", "no_renames", "index_only", "workdir_only", "staged_stats", "unstaged_stats"]
    rows = []
    private_index = output / "private.index"
    for block in range(args.blocks):
        order = modes[block * 4 % len(modes):] + modes[:block * 4 % len(modes)]
        if block % 2:
            order.reverse()
        for mode in order:
            shutil.copy2(index, private_index)
            child_env = dict(env, GIT_INDEX_FILE=str(private_index))
            result = subprocess.run([str(args.binary.resolve()), str(repo), mode, str(args.rounds)], env=child_env, capture_output=True, text=True, timeout=180, check=True)
            (output / f"{block}-{mode}.txt").write_text(result.stderr + result.stdout, encoding="utf8")
            for row in csv.DictReader(io.StringIO(result.stdout)):
                row = {k: v if k == "mode" else int(v) for k, v in row.items()}
                assert row["count"] == 0, f"Repository changed during measurement: {row}"
                rows.append(dict(block=block, warmup=row["round"] < 2, **row))
            print(f"block={block} {mode}: last={rows[-1]['total_ns']/1e6:.2f} ms", flush=True)
        for round_number in range(args.rounds):
            start = time.perf_counter_ns()
            status = git("status", "--porcelain=v1", "-z", "--untracked-files=all")
            elapsed = time.perf_counter_ns() - start
            assert not status
            rows.append(dict(block=block, warmup=round_number < 2, mode="git_cli", round=round_number,
                             total_ns=elapsed, open_ns=0, scan_ns=0, collect_ns=0, count=0))
    after = snapshot()
    metadata["after"] = after
    metadata["unchanged"] = before == after
    metadata["finished_utc"] = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())
    (output / "environment.json").write_text(json.dumps(metadata, indent=2), encoding="utf8")
    with (output / "samples.csv").open("w", newline="", encoding="utf8") as f:
        writer = csv.DictWriter(f, fieldnames=list(rows[0]))
        writer.writeheader()
        writer.writerows(rows)
    summary = []
    for mode in modes + ["git_cli"]:
        samples = [r for r in rows if r["mode"] == mode and not r["warmup"]]
        values = sorted(r["total_ns"] / 1e6 for r in samples)
        summary.append(dict(mode=mode, n=len(values), median_ms=statistics.median(values), p95_ms=values[math.ceil(len(values)*.95)-1],
                            min_ms=min(values), max_ms=max(values),
                            open_median_ms=statistics.median(r["open_ns"]/1e6 for r in samples),
                            scan_median_ms=statistics.median(r["scan_ns"]/1e6 for r in samples),
                            collect_median_ms=statistics.median(r["collect_ns"]/1e6 for r in samples)))
    (output / "summary.json").write_text(json.dumps(summary, indent=2), encoding="utf8")
    print(json.dumps(summary, indent=2))
    assert before == after, "Target metadata/status changed during experiment; inspect results before using them"


if __name__ == "__main__":
    main()
