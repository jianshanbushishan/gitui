#!/usr/bin/env python3
"""Compare two release Status component probes using a private index copy."""
import argparse
import csv
import hashlib
import json
import math
import os
from pathlib import Path
import shutil
import statistics
import subprocess


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("repo", type=Path)
    p.add_argument("output", type=Path)
    p.add_argument("baseline", type=Path)
    p.add_argument("candidate", type=Path)
    args = p.parse_args()
    repo, out = args.repo.resolve(), args.output.resolve()
    assert not out.is_relative_to(repo)
    out.mkdir(parents=True, exist_ok=False)
    env = {k: v for k, v in os.environ.items() if not k.upper().startswith("GIT_")}
    env["GIT_OPTIONAL_LOCKS"] = "0"

    def git(*a):
        return subprocess.check_output(["git", "--no-optional-locks", "-C", str(repo), *a], env=env, timeout=120)

    def sha(path):
        return hashlib.sha256(path.read_bytes()).hexdigest()

    gitdir = Path(git("rev-parse", "--absolute-git-dir").decode().strip())
    index = gitdir / "index"

    def snapshot():
        return dict(head=git("rev-parse", "HEAD").decode().strip(), index_sha256=sha(index),
                    index_mtime_ns=index.stat().st_mtime_ns, config_sha256=sha(gitdir / "config"),
                    status_hex=git("status", "--porcelain=v1", "-z", "--untracked-files=all").hex())

    before = snapshot()
    assert not before["status_hex"], "Only clean repositories are supported by this probe"
    bins = {name: getattr(args, name).resolve() for name in ("baseline", "candidate")}
    metadata = dict(before=before, binaries={name: dict(path=str(path), sha256=sha(path)) for name, path in bins.items()},
                    tracked_files=len(git("ls-files", "-z").split(b"\0"))-1,
                    blocks=3, rounds_per_process=23, warmups_per_process=2,
                    backend="Ratatui TestBackend 160x50; excludes terminal host and polling wait")
    rows = []
    try:
        for block in range(3):
            order = ("baseline", "candidate") if block % 2 == 0 else ("candidate", "baseline")
            for variant in order:
                copy = out / "private.index"
                shutil.copy2(index, copy)
                child_env = dict(env, GIT_INDEX_FILE=str(copy), GITUI_LARGE_REPO=str(repo))
                r = subprocess.run([str(bins[variant]), "--exact", "tabs::status::perf::real_repository_refresh", "--ignored", "--nocapture", "--test-threads=1"], env=child_env, capture_output=True, text=True, timeout=180)
                (out / f"{block}-{variant}.txt").write_text(r.stdout+r.stderr, encoding="utf8")
                r.check_returncode()
                lines = [line.split("UI_CSV,", 1)[1] for line in r.stdout.splitlines() if "UI_CSV," in line]
                assert len(lines) == 24, lines
                for row in csv.DictReader(lines):
                    rows.append(dict(block=block, variant=variant, **{k: int(v) for k, v in row.items()}))
                print(f"block {block}: {variant} complete", flush=True)
    finally:
        metadata["after"] = snapshot()
        metadata["unchanged"] = before == metadata["after"]
        (out / "environment.json").write_text(json.dumps(metadata, indent=2), encoding="utf8")
    assert metadata["unchanged"]
    with (out / "samples.csv").open("w", newline="", encoding="utf8") as f:
        writer = csv.DictWriter(f, fieldnames=list(rows[0]));writer.writeheader();writer.writerows(rows)
    summary = []
    for variant in bins:
        for metric in ("status_drawn_ns", "all_jobs_ns", "update_call_ns", "notification_handler_ns", "draw_ns"):
            values = sorted(row[metric]/1e6 for row in rows if row["variant"] == variant and row["round"] >= 2)
            summary.append(dict(variant=variant, metric=metric, n=len(values), median_ms=statistics.median(values), p95_ms=values[math.ceil(len(values)*.95)-1], min_ms=min(values), max_ms=max(values)))
    (out / "summary.json").write_text(json.dumps(summary, indent=2), encoding="utf8")
    print(json.dumps(summary, indent=2))


if __name__ == "__main__":
    main()
