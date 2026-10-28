#!/usr/bin/env python3
"""Prepare an independent tracked-file replica, then benchmark dirty scenarios.

prepare SOURCE OUTPUT
run SOURCE OUTPUT BASELINE_EXE CANDIDATE_EXE
All writes target OUTPUT; objects are read through an alternate. No hardlinks,
junctions, checkout, reset, clean, or source repository writes are used.
"""
import concurrent.futures
import csv
import hashlib
import json
import math
import os
from pathlib import Path
import shutil
import statistics
import subprocess
import sys
import time

ENV = {k: v for k, v in os.environ.items() if not k.upper().startswith("GIT_")}
ENV["GIT_OPTIONAL_LOCKS"] = "0"


def git(root, *args, env=ENV):
    return subprocess.check_output(["git", "--no-optional-locks", "-C", str(root), *args], env=env, timeout=1200)


def sha(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def write_json(path, data):
    path.write_text(json.dumps(data, ensure_ascii=False, indent=2)+"\n", encoding="utf8")


def tracked(root):
    return [p.decode("utf8") for p in git(root, "ls-files", "-z").split(b"\0") if p]


def snapshot(root, files, selected):
    manifest = [(name, (root/name).stat().st_size, (root/name).stat().st_mtime_ns) for name in files]
    return dict(head=git(root, "rev-parse", "HEAD").decode().strip(),
                index_sha256=sha(root/".git/index"), index_mtime_ns=(root/".git/index").stat().st_mtime_ns,
                config_sha256=sha(root/".git/config"), status_hex=git(root, "status", "--porcelain=v1", "-z", "-uall").hex(),
                tracked_metadata_sha256=hashlib.sha256(json.dumps(manifest).encode()).hexdigest(),
                selected_sha256={name: sha(root/name) for name in selected})


def prepare(source, out):
    out.mkdir(parents=True, exist_ok=False)
    fixture=out/"fixture"
    fixture.mkdir()
    files=tracked(source)
    selected=[]
    for name in files:
        if name.startswith(".") or Path(name).suffix not in (".cpp", ".h", ".md", ".py", ".txt"): continue
        b=(source/name).read_bytes()
        if len(b)>100000 or not b.endswith(b"\n"): continue
        if b"\r" in b: continue
        try: b.decode("utf8")
        except UnicodeError: continue
        selected.append(name)
        if len(selected)==20: break
    assert len(selected)==20
    before=snapshot(source, files, selected)
    assert not before["status_hex"], "Source must be clean for a controlled replica"
    manifest=dict(source=str(source), files=files, selected=selected, before=before,
                  tracked_bytes=sum((source/name).stat().st_size for name in files),
                  fixture_method="Independent copy2 of every tracked regular file; ignored generated files omitted; separate gitdir with read-only object alternate")
    write_json(out/"manifest.json", manifest)
    def copy(name):
        src=source/name; dst=fixture/name
        assert not src.is_symlink(), f"Symlink needs explicit treatment: {name}"
        dst.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(src,dst)
    print(f"Copying {len(files)} tracked files ({manifest['tracked_bytes']/1e9:.2f} GB) independently",flush=True)
    with concurrent.futures.ThreadPoolExecutor(max_workers=8) as pool:
        list(pool.map(copy,files))
    git(fixture,"init","--quiet")
    # Only plain config, refs and index; never source hooks or redirects.
    shutil.copy2(source/".git/config",fixture/".git/config")
    for name in ("HEAD","packed-refs","index"):
        p=source/".git"/name
        if p.exists(): shutil.copy2(p,fixture/".git"/name)
    shutil.copytree(source/".git/refs",fixture/".git/refs",dirs_exist_ok=True)
    exclude=source/".git/info/exclude"
    if exclude.exists(): shutil.copy2(exclude,fixture/".git/info/exclude")
    (fixture/".git/objects/info/alternates").write_bytes(((source/".git/objects").as_posix()+"\n").encode())
    assert git(fixture,"rev-parse","--show-toplevel").decode().strip().replace("\\","/")==fixture.as_posix()
    # Refresh stat metadata only in the independent replica, before timing.
    git(fixture,"update-index","--refresh")
    assert not git(fixture,"status","--porcelain=v1","-z","-uall")
    shutil.copy2(fixture/".git/index",out/"clean.index")
    originals=out/"originals"; originals.mkdir()
    for i,name in enumerate(selected): shutil.copy2(fixture/name,originals/str(i))
    after=snapshot(source,files,selected)
    manifest["after_prepare"]=after
    manifest["unchanged_after_prepare"]=before==after
    write_json(out/"manifest.json",manifest)
    assert before==after
    print("Replica ready; source metadata and selected file hashes unchanged",flush=True)


def run(source,out,binaries):
    m=json.loads((out/"manifest.json").read_text(encoding="utf8"));fixture=out/"fixture"
    assert snapshot(source,m["files"],m["selected"])==m["before"]
    resultdir=out/"results"; resultdir.mkdir(exist_ok=False)
    records=[]; scenarios={}; expected_keys=["STAGE_COUNT","WORKDIR_COUNT","STAGE_ADDED","STAGE_DELETED","WORKDIR_ADDED","WORKDIR_DELETED"]
    try:
        for scenario in ("unstaged","staged","untracked"):
            for i,name in enumerate(m["selected"]): shutil.copy2(out/"originals"/str(i),fixture/name)
            shutil.copy2(out/"clean.index",fixture/".git/index")
            if scenario in ("unstaged","staged"):
                for name in m["selected"]:
                    original=(fixture/name).read_bytes()
                    newline=b"\r\n" if original.count(b"\r\n")==original.count(b"\n") else b"\n"
                    with (fixture/name).open("ab") as f:
                        f.write(newline.join([b"// GitUI isolated performance probe 1",b"// GitUI isolated performance probe 2",b"// GitUI isolated performance probe 3",b""]))
                if scenario=="staged": git(fixture,"add","--",*m["selected"])
            else:
                directory=fixture/"gitui_performance_untracked_fixture";directory.mkdir()
                for i in range(20): (directory/f"new-{i:02}.txt").write_text("new benchmark line\n"*3,encoding="utf8")
            expected={"unstaged":[0,20,0,0,60,0],"staged":[20,0,60,0,0,0],"untracked":[0,20,0,0,0,0]}[scenario]
            status=git(fixture,"status","--porcelain=v1","-z","-uall")
            assert len(status.split(b"\0"))-1==20
            expected_prefix={"unstaged":b" M ","staged":b"M  ","untracked":b"?? "}[scenario]
            assert all(entry.startswith(expected_prefix) for entry in status.split(b"\0") if entry)
            # numstat validates exactly 60 additions and zero deletions for tracked scenarios.
            stats=git(fixture,"diff",*( ["--cached"] if scenario=="staged" else []),"--numstat").decode()
            totals=[0,0]
            for line in stats.splitlines():
                a,d,_=line.split("\t",2);totals[0]+=int(a);totals[1]+=int(d)
            assert totals==([60,0] if scenario!="untracked" else [0,0])
            scenarios[scenario]=dict(expected=dict(zip(expected_keys,expected)),status_sha256=hashlib.sha256(status).hexdigest(),numstat_totals=totals)
            for block in range(3):
                order=("baseline","candidate") if block%2==0 else ("candidate","baseline")
                for variant in order:
                    private=resultdir/"private.index";shutil.copy2(fixture/".git/index",private)
                    child=dict(ENV,GIT_INDEX_FILE=str(private),GITUI_LARGE_REPO=str(fixture))
                    child.update({"GITUI_EXPECT_"+key:str(value) for key,value in zip(expected_keys,expected)})
                    r=subprocess.run([str(binaries[variant]),"--exact","tabs::status::perf::real_repository_refresh","--ignored","--nocapture","--test-threads=1"],env=child,capture_output=True,text=True,timeout=300)
                    (resultdir/f"{scenario}-{block}-{variant}.txt").write_text((r.stdout+r.stderr).rstrip()+"\n",encoding="utf8");r.check_returncode()
                    lines=[line.split("UI_CSV,",1)[1] for line in r.stdout.splitlines() if "UI_CSV," in line]
                    assert len(lines)==24
                    for row in csv.DictReader(lines):records.append(dict(scenario=scenario,block=block,variant=variant,**{k:int(v) for k,v in row.items()}))
                    print(f"{scenario} block {block} {variant} complete",flush=True)
    finally:
        after=snapshot(source,m["files"],m["selected"])
        write_json(resultdir/"environment.json",dict(before=m["before"],after=after,source_unchanged=after==m["before"],tracked_count=len(m["files"]),tracked_bytes=m["tracked_bytes"],fixture_method=m["fixture_method"],scenarios=scenarios,binaries={k:dict(path=str(v),sha256=sha(v)) for k,v in binaries.items()}))
    assert after==m["before"]
    with (resultdir/"samples.csv").open("w",newline="",encoding="utf8") as f:
        writer=csv.DictWriter(f,fieldnames=list(records[0]));writer.writeheader();writer.writerows(records)
    summary=[]
    for scenario in scenarios:
        for variant in binaries:
            for metric in ("status_drawn_ns","all_jobs_ns","draw_ns"):
                values=sorted(r[metric]/1e6 for r in records if r["scenario"]==scenario and r["variant"]==variant and r["round"]>=2)
                summary.append(dict(scenario=scenario,variant=variant,metric=metric,n=len(values),median_ms=statistics.median(values),p95_ms=values[math.ceil(len(values)*.95)-1]))
    write_json(resultdir/"summary.json",summary); print(json.dumps(summary,indent=2))


if __name__=="__main__":
    command,source_arg,out_arg,*binary_args=sys.argv[1:]
    source,out=Path(source_arg).resolve(),Path(out_arg).resolve()
    assert not out.is_relative_to(source)
    if command=="prepare": prepare(source,out)
    elif command=="run": run(source,out,dict(zip(("baseline","candidate"),[Path(p).resolve() for p in binary_args])))
    else: raise ValueError(command)
