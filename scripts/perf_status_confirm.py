#!/usr/bin/env python3
"""Confirm repository reuse, measure the Status component, and compare Git preload.

Run after building the release example and the release gitui unit-test binary:
python scripts/perf_status_confirm.py REPO OUTPUT --ui-binary TEST_EXE
Only the named ignored probe is executed; it draws into a 160x50 TestBackend.
"""
import argparse
import csv
import hashlib
import io
import json
import math
import os
from pathlib import Path
import shutil
import statistics
import subprocess
import time


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("repo", type=Path)
    parser.add_argument("output", type=Path)
    parser.add_argument("--binary", type=Path, default=Path("target/release/examples/perf_status_real.exe"))
    parser.add_argument("--ui-binary", type=Path, required=True)
    args = parser.parse_args()
    repo, output = args.repo.resolve(), args.output.resolve()
    assert not output.is_relative_to(repo)
    output.mkdir(parents=True, exist_ok=False)
    env = {k: v for k, v in os.environ.items() if not k.upper().startswith("GIT_")}
    env["GIT_OPTIONAL_LOCKS"] = "0"

    def git(*arguments, child_env=env):
        return subprocess.check_output(["git", "--no-optional-locks", "-C", str(repo), *arguments], env=child_env, timeout=120)

    def sha(path):
        return hashlib.sha256(path.read_bytes()).hexdigest()

    gitdir = Path(git("rev-parse", "--absolute-git-dir").decode().strip())
    index = gitdir / "index"

    def snapshot():
        return dict(head=git("rev-parse", "HEAD").decode().strip(), index_sha256=sha(index),
                    index_mtime_ns=index.stat().st_mtime_ns, config_sha256=sha(gitdir / "config"),
                    status_hex=git("status", "--porcelain=v1", "-z", "--untracked-files=all").hex())

    before = snapshot()
    assert not before["status_hex"]
    metadata = dict(before=before, binary_sha256=sha(args.binary), ui_binary_sha256=sha(args.ui_binary))
    private_index = output / "private.index"
    child_env = dict(env, GIT_INDEX_FILE=str(private_index), GITUI_LARGE_REPO=str(repo))
    samples = []
    try:
        shutil.copy2(index, private_index)
        result = subprocess.run([str(args.binary.resolve()), str(repo), "paired", "46"], env=child_env, capture_output=True, text=True, check=True, timeout=180)
        (output / "paired.txt").write_text(result.stderr + result.stdout, encoding="utf8")
        for row in csv.DictReader(io.StringIO(result.stdout)):
            assert int(row["count"]) == 0
            if int(row["round"]) >= 4:
                samples.append(dict(mode=row["mode"], round=int(row["round"]), ms=int(row["total_ns"])/1e6))
        print("Paired raw/reuse measurements complete", flush=True)
        shutil.copy2(index, private_index)
        result = subprocess.run([str(args.ui_binary.resolve()), "--exact", "tabs::status::perf::real_repository_refresh", "--ignored", "--nocapture", "--test-threads=1"], env=child_env, capture_output=True, text=True, check=True, timeout=180)
        (output / "ui.txt").write_text(result.stdout + result.stderr, encoding="utf8")
        lines = [line.split("UI_CSV,", 1)[1] for line in result.stdout.splitlines() if "UI_CSV," in line]
        assert len(lines) == 24, "Expected header plus 23 UI samples"
        ui_rows = list(csv.DictReader(lines))
        for row in ui_rows[2:]:
            for key in ("status_drawn_ns", "all_jobs_ns", "update_call_ns", "notification_handler_ns", "draw_ns"):
                samples.append(dict(mode="ui_"+key.removesuffix("_ns"), round=int(row["round"]), ms=int(row[key])/1e6))
        print("Status component measurements complete", flush=True)
        shutil.copy2(index, private_index)
        for round_number in range(46):
            # ABBA ordering, excluding the first four samples (two per variant).
            preload = round_number % 4 in (0, 3)
            start = time.perf_counter_ns()
            status = git("-c", f"core.preloadIndex={'true' if preload else 'false'}", "status", "--porcelain=v1", "-z", "--untracked-files=all", child_env=child_env)
            elapsed = time.perf_counter_ns() - start
            assert not status
            if round_number >= 4:
                samples.append(dict(mode="git_preload" if preload else "git_no_preload", round=round_number, ms=elapsed/1e6))
        print("Git preload measurements complete", flush=True)
    finally:
        metadata["after"] = snapshot()
        metadata["unchanged"] = before == metadata["after"]
        (output / "environment.json").write_text(json.dumps(metadata, indent=2), encoding="utf8")
    assert metadata["unchanged"]
    with (output / "samples.csv").open("w", newline="", encoding="utf8") as f:
        writer = csv.DictWriter(f, fieldnames=list(samples[0]))
        writer.writeheader()
        writer.writerows(samples)
    summary = []
    for mode in dict.fromkeys(r["mode"] for r in samples):
        values = sorted(r["ms"] for r in samples if r["mode"] == mode)
        summary.append(dict(mode=mode, n=len(values), median_ms=statistics.median(values), p95_ms=values[math.ceil(len(values)*.95)-1], min_ms=min(values), max_ms=max(values)))
    (output / "summary.json").write_text(json.dumps(summary, indent=2), encoding="utf8")
    print(json.dumps(summary, indent=2))


if __name__ == "__main__":
    main()
