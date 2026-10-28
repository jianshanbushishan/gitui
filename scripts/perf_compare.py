"""Reproducible, local-only GitUI dependency comparison (Python standard library).

Prepare: python scripts/perf_compare.py prepare --output target/perf-20261028
Run: python scripts/perf_compare.py run --output target/perf-20261028 \
  --baseline-root ABSOLUTE_PATH --intermediate-root ABSOLUTE_PATH \
  --candidate-root ABSOLUTE_PATH --rounds 21

Build examples first, with the same toolchain/default features/release profile:
  cargo build --release --locked -p asyncgit --example perf_compare --features vendor-openssl
  cargo build --release --locked -p gitui --example perf_textarea
This script never builds, installs, deletes, or touches a user repository.
"""
import argparse
import csv
import hashlib
import json
import math
import os
from pathlib import Path
import platform
import re
import shutil
import statistics
import subprocess
import sys
import time


def environment():
    # Both GitUI's public APIs and git CLI honor repository-selection variables.
    # Remove every inherited Git setting before addressing disposable fixtures.
    env = {key: value for key, value in os.environ.items() if not key.upper().startswith("GIT_")}
    env.update(GIT_CONFIG_NOSYSTEM="1", GIT_CONFIG_GLOBAL=os.devnull,
               GIT_TERMINAL_PROMPT="0", GIT_CONFIG_COUNT="0")
    return env


def command(args, cwd=None, data=None):
    return subprocess.check_output([str(x) for x in args], cwd=cwd,
                                   env=environment(), input=data, stderr=subprocess.STDOUT)


def create_repo(path, files):
    path.mkdir()
    command(["git", "init", "--quiet", path])
    for key, value in {"core.autocrlf": "false", "core.fsmonitor": "false",
                       "core.untrackedCache": "false", "core.ignorestat": "false"}.items():
        command(["git", "config", key, value], path)
    stream = bytearray()
    def blob(text):
        value = text.encode()
        return f"data {len(value)}\n".encode() + value + b"\n"
    for commit in range(300):
        stream.extend(f"commit refs/heads/main\ncommitter Perf Fixture <perf@example.invalid> {1700000000 + commit} +0000\n".encode())
        stream.extend(blob(f"deterministic commit {commit:03}\n"))
        if commit == 0:
            for index in range(files):
                stream.extend(f"M 100644 inline files/{index:05}.txt\n".encode())
                stream.extend(blob((f"file {index:05} 内容 abcdefghijklmnopqrstuvwxyz\n") * 32))
        stream.extend(b"M 100644 inline history.txt\n")
        stream.extend(blob(f"history revision {commit:03}\n"))
        stream.extend(b"\n")
    stream.extend(b"done\n")
    command(["git", "fast-import", "--quiet"], path, bytes(stream))
    command(["git", "symbolic-ref", "HEAD", "refs/heads/main"], path)
    command(["git", "reset", "--hard", "--quiet", "HEAD"], path)


def prepare(output):
    fixture = output / "fixtures"
    fixture.mkdir(parents=True, exist_ok=False)
    create_repo(fixture / "clean", 2000)
    create_repo(fixture / "large_clean", 10000)
    for name, source in [("dirty", "clean"), ("staged", "clean"),
                         ("untracked", "clean"), ("large_dirty", "large_clean")]:
        path = fixture / name
        shutil.copytree(fixture / source, path)
        if name == "untracked":
            for index in range(200):
                p = path / "untracked" / f"dir{index % 20:02}" / f"{index:04}.txt"
                p.parent.mkdir(parents=True, exist_ok=True)
                p.write_text("untracked 测试\n" * 16, encoding="utf-8")
        else:
            for index in range(80):
                p = path / "files" / f"{index:05}.txt"
                with p.open("a", encoding="utf-8", newline="\n") as handle:
                    handle.write("changed 中间修改\n" * 16)
            if name == "staged":
                command(["git", "add", "files"], path)
    manifest = {}
    for path in sorted(fixture.iterdir()):
        status = command(["git", "status", "--porcelain=v1", "--untracked-files=all"], path).decode()
        manifest[path.name] = {"head": command(["git", "rev-parse", "HEAD"], path).decode().strip(),
                               "status_sha256": hashlib.sha256(status.encode()).hexdigest(),
                               "status_entries": len(status.splitlines()),
                               "tracked_files": int(command(["git", "ls-files"], path).count(b"\n"))}
    (output / "fixture_manifest.json").write_text(json.dumps(manifest, indent=2), encoding="utf-8")
    print(f"Prepared deterministic fixtures: {fixture}")


def percentile(values, p):
    return sorted(values)[max(0, math.ceil(len(values) * p) - 1)]


def run(args, output):
    fixture = output / "fixtures"
    assert fixture.is_dir(), "Run prepare first"
    roots = {"baseline": Path(args.baseline_root).resolve(), "intermediate": Path(args.intermediate_root).resolve(), "candidate": Path(args.candidate_root).resolve()}
    suffix = ".exe" if os.name == "nt" else ""
    binaries = {label: {kind: (output / "binaries" / label / (name + suffix) if (output / "binaries" / label / (name + suffix)).is_file()
                              else root / "target" / "release" / "examples" / (name + suffix))
                       for kind, name in [("git", "perf_compare"), ("textarea", "perf_textarea")]}
                for label, root in roots.items()}
    for group in binaries.values():
        for binary in group.values():
            assert binary.is_file(), f"Missing release example {binary}"
    source_hashes = {}
    for relative in ["asyncgit/examples/perf_compare.rs", "examples/perf_textarea.rs", "scripts/perf_compare.py"]:
        hashes = {label: hashlib.sha256((root / relative).read_bytes()).hexdigest() for label, root in roots.items()}
        assert len(set(hashes.values())) == 1, (relative, hashes)
        source_hashes[relative] = next(iter(hashes.values()))
        saved_source = output / "harness" / relative
        saved_source.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(roots["candidate"] / relative, saved_source)
    for label, group in binaries.items():
        directory = output / "binaries" / label
        directory.mkdir(parents=True, exist_ok=True)
        for kind, binary in list(group.items()):
            saved = directory / binary.name
            if binary.resolve() != saved.resolve():
                shutil.copy2(binary, saved)
            group[kind] = saved
    metadata = {"created_utc": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
                "platform": platform.platform(), "cpu": platform.processor(),
                "python": sys.version, "rustc": command(["rustc", "-Vv"]).decode(),
                "git": command(["git", "--version"]).decode().strip(),
                "rounds": args.rounds, "warmup_process_triplets": 2,
                "profile": "release; opt-level=3; lto=true; codegen-units=1; gitui default features; asyncgit default features plus vendor-openssl",
                "harness_sha256": source_hashes,
                "build_environment": {key: os.environ.get(key) for key in ["CARGO_TARGET_DIR", "OPENSSL_DIR", "OPENSSL_NO_VENDOR", "OPENSSL_STATIC", "RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS", "RUSTUP_TOOLCHAIN"]},
                "roots": {}, "command": sys.argv}
    for label, root in roots.items():
        lock = (root / "Cargo.lock").read_bytes()
        patch = command(["git", "-c", "core.autocrlf=true", "diff", "HEAD", "--binary"], root)
        (output / f"{label}.patch").write_bytes(patch)
        metadata["roots"][label] = {"path": str(root), "sha": command(["git", "rev-parse", "HEAD"], root).decode().strip(),
            "git_status": command(["git", "-c", "core.autocrlf=true", "status", "--short"], root).decode(),
            "lock_sha256": hashlib.sha256(lock).hexdigest(),
            "working_tree_patch_sha256": hashlib.sha256(patch).hexdigest(),
            "versions": {name: re.findall(r'name = "' + re.escape(name) + r'"\r?\nversion = "([^"]+)"', lock.decode())
                         for name in ["gix", "git2", "ratatui", "ratatui-textarea", "image", "resvg", "ron"]},
            "binaries": {kind: {"path": str(binary), "sha256": hashlib.sha256(binary.read_bytes()).hexdigest()}
                         for kind, binary in binaries[label].items()}}
    (output / "environment.json").write_text(json.dumps(metadata, indent=2), encoding="utf-8")
    samples = []
    def invoke(label, kind, round_index, warmup=False):
        argv = ([binaries[label][kind], fixture, "10"] if kind == "git" else [binaries[label][kind], "50"])
        result = command(argv).decode()
        with (output / "process_outputs.txt").open("a", encoding="utf-8") as handle:
            handle.write(f"round={round_index} label={label} kind={kind} warmup={warmup}\n{result}\n")
        if warmup:
            return
        for line in result.splitlines():
            name, count, nanos, checksum = line.split(",")
            if name == "history/metadata_300":
                name = "history/git2_metadata_300"
            samples.append({"round": round_index, "variant": label, "benchmark": name,
                            "iterations": int(count), "total_ns": int(nanos),
                            "ns_per_op": int(nanos) / int(count), "checksum": int(checksum)})
    for round_index in range(-2, args.rounds):
        # All six permutations, repeated; balances each variant's execution position.
        permutations = [("baseline", "intermediate", "candidate"), ("intermediate", "candidate", "baseline"),
                        ("candidate", "baseline", "intermediate"), ("candidate", "intermediate", "baseline"),
                        ("baseline", "candidate", "intermediate"), ("intermediate", "baseline", "candidate")]
        order = permutations[round_index % len(permutations)]
        for kind in ["git", "textarea"]:
            for label in order:
                invoke(label, kind, round_index, warmup=round_index < 0)
        print(f"Completed {'warmup' if round_index < 0 else 'measurement'} round {round_index + 1}", flush=True)
    with (output / "samples.csv").open("w", newline="", encoding="utf-8") as handle:
        writer = csv.DictWriter(handle, fieldnames=list(samples[0])); writer.writeheader(); writer.writerows(samples)
    rows = []
    for name in dict.fromkeys(sample["benchmark"] for sample in samples):
        row = {"benchmark": name}
        checksums = {}
        for label in roots:
            subset = [s for s in samples if s["benchmark"] == name and s["variant"] == label]
            values = [s["ns_per_op"] for s in subset]
            checksums[label] = sorted(set(s["checksum"] for s in subset))
            row[f"{label}_median_us"] = statistics.median(values) / 1000
            row[f"{label}_p95_us"] = percentile(values, .95) / 1000
        assert checksums["baseline"] == checksums["intermediate"] == checksums["candidate"], (name, checksums)
        row["median_change_pct"] = (row["candidate_median_us"] / row["baseline_median_us"] - 1) * 100
        row["baseline_to_intermediate_pct"] = (row["intermediate_median_us"] / row["baseline_median_us"] - 1) * 100
        row["intermediate_to_candidate_pct"] = (row["candidate_median_us"] / row["intermediate_median_us"] - 1) * 100
        rows.append(row)
    (output / "summary.json").write_text(json.dumps(rows, indent=2), encoding="utf-8")
    with (output / "summary.csv").open("w", newline="", encoding="utf-8") as handle:
        writer = csv.DictWriter(handle, fieldnames=list(rows[0])); writer.writeheader(); writer.writerows(rows)
    markdown = ["# GitUI dependency performance comparison", "", "Times are microseconds per operation; negative change means faster.", "",
                "| Operation | Baseline median | Intermediate median | Candidate median | Baseline→intermediate | Intermediate→candidate | Baseline→candidate | Baseline p95 | Intermediate p95 | Candidate p95 |", "|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|"]
    for row in rows:
        markdown.append(f"| {row['benchmark']} | {row['baseline_median_us']:.2f} | {row['intermediate_median_us']:.2f} | {row['candidate_median_us']:.2f} | {row['baseline_to_intermediate_pct']:+.1f}% | {row['intermediate_to_candidate_pct']:+.1f}% | {row['median_change_pct']:+.1f}% | {row['baseline_p95_us']:.2f} | {row['intermediate_p95_us']:.2f} | {row['candidate_p95_us']:.2f} |")
    markdown += ["", "Method: two discarded process triplets, three in-process warmups per operation; rotated and reversed three-version process order (six permutations); "
                 f"{args.rounds} measured rounds. p95 is nearest-rank over round means, not individual-call tail latency. "
                 "Fixture setup, process startup, textarea construction/cursor placement, and compilation are excluded. "
                 "Rendering includes a fresh 120x40 ratatui buffer but no terminal I/O. All runs use the same disposable fixtures and warm filesystem cache. "
                 "Checksums agree across variants. No network or user-project operations. See environment.json for SHAs, binary hashes, compiler, profile and command.", "",
                 "Limitations: one Windows machine; filesystem/cache/antivirus/background activity can add noise; no cold-cache or interactive end-to-end UI latency; "
                 "dependency cohort plus required compatibility edits are compared together; this does not isolate a single crate's effect. "
                 "Small synthetic repositories and linear history do not represent every workload."]
    markdown += ["", "Fixtures: 2,001 tracked files for clean/dirty/staged/untracked cases; 10,001 for large clean/dirty cases. "
                 "Dirty/staged cases contain 80 changed files; untracked contains 200 files across 20 directories. "
                 "Each repository contains 300 deterministic commits. Textareas use 64 lines × 8 repeats and 2,048 lines × 16 repeats "
                 "of mixed ASCII, CJK and emoji, with cursor placement in the middle. Cursor benchmark performs 50 forward/back pairs. "
                 "History walking uses gix; commit metadata uses GitUI's git2-backed get_commits_info API. "
                 "The exact source, binary hashes and fixture commit IDs are retained with the raw results."]
    markdown += ["", "## Build and reproduction", "", f"Compiler: `{metadata['rustc'].splitlines()[0]}`.",
                 "; ".join(f"{label} HEAD: `{metadata['roots'][label]['sha']}`" for label in roots) + ". Working-tree patches are saved alongside environment.json.",
                 "Versions: " + "; ".join(f"{label}: " + ", ".join(f"{name} {'/'.join(versions)}" for name, versions in metadata['roots'][label]['versions'].items()) for label in roots) + ".",
                 "", "Build in each checkout, preserving the same environment:", "```powershell",
                 "cargo build --release --locked -p asyncgit --example perf_compare --features vendor-openssl",
                 "cargo build --release --locked -p gitui --example perf_textarea", "```", "", "Run:", "```powershell",
                 subprocess.list2cmdline([sys.executable] + sys.argv), "```",
                 "", "The original binaries are preserved under binaries/baseline, binaries/intermediate and binaries/candidate before execution. Installed GitUI is never replaced."]
    (output / "report.md").write_text("\n".join(markdown) + "\n", encoding="utf-8")
    print(f"Saved {len(samples)} samples and comparison report: {output / 'report.md'}")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("action", choices=["prepare", "run"])
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--baseline-root")
    parser.add_argument("--intermediate-root")
    parser.add_argument("--candidate-root")
    parser.add_argument("--rounds", type=int, default=21)
    args = parser.parse_args()
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=True)
    if args.action == "prepare":
        prepare(output)
    else:
        assert args.baseline_root and args.intermediate_root and args.candidate_root
        run(args, output)
