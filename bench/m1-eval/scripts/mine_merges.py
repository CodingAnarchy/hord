#!/usr/bin/env python3
"""Mine git 3-way conflict cases from cargo/tokio into corpora/merges/.

Each case is a file that `git merge-file` conflicts on, with the merge
commit's blob as the human-labeled resolution (spec §12 M1).
"""

from __future__ import annotations

import json
import os
import subprocess
import sys
import tempfile
from collections import defaultdict
from pathlib import Path

# cargo (7532 merges) and tokio (183 merges) together contain 93
# `.rs`/`.toml` labels equal to `git merge-file --ours`. Markdown conflicts
# are blob-tier hard conflicts (spec §5.2 rule 5), so they are not mined.
OURS_TARGET = int(os.environ.get("HORD_MINE_OURS", "93"))


def next_case_id(out: Path) -> int:
    ids = [int(p.name) for p in out.iterdir() if p.is_dir() and p.name.isdigit()]
    return (max(ids) if ids else 0) + 1
MAX_BYTES = 500_000
MAX_PER_PATH = 100
LANG_SUFFIX = (".rs", ".toml", ".md")


def git(gitdir: Path, *args: str, check: bool = True) -> subprocess.CompletedProcess[bytes]:
    r = subprocess.run(
        ["git", "--git-dir", str(gitdir), *args],
        capture_output=True,
    )
    if check and r.returncode != 0:
        raise RuntimeError(f"git {args}: {r.stderr.decode('utf-8', 'replace')}")
    return r


def show(gitdir: Path, rev: str, path: str) -> bytes | None:
    r = git(gitdir, "show", f"{rev}:{path}", check=False)
    if r.returncode != 0:
        return None
    return r.stdout


def conflicted_paths(gitdir: Path, ours: str, theirs: str) -> list[str]:
    r = git(gitdir, "merge-tree", "--write-tree", "--name-only", ours, theirs, check=False)
    paths: list[str] = []
    for ln in r.stdout.decode("utf-8", "replace").splitlines():
        if not ln or " " in ln or ln.startswith("Auto-merging") or ln.startswith("CONFLICT"):
            continue
        if len(ln) == 40 and all(c in "0123456789abcdef" for c in ln):
            continue
        if ln.endswith(LANG_SUFFIX):
            paths.append(ln)
    return paths


def git_merge_ours(base: bytes, ours: bytes, theirs: bytes) -> bytes | None:
    """Stdout of `git merge-file --ours`. None when git fails outright."""
    with tempfile.TemporaryDirectory() as td:
        t = Path(td)
        (t / "base").write_bytes(base)
        (t / "ours").write_bytes(ours)
        (t / "theirs").write_bytes(theirs)
        r = subprocess.run(
            [
                "git",
                "merge-file",
                "-p",
                "--ours",
                str(t / "ours"),
                str(t / "base"),
                str(t / "theirs"),
            ],
            capture_output=True,
        )
        if r.stdout == b"" and r.returncode != 0:
            return None
        return r.stdout


def label_is_ours(case: Path) -> bool:
    meta = case / "meta.json"
    if meta.exists():
        marker = json.loads(meta.read_text()).get("label")
        if marker == "git-merge-ours":
            return True
        if marker == "manual":
            return False
    for ext in (".rs", ".toml", ".md"):
        base = case / f"base{ext}"
        if not base.exists():
            continue
        files = [base.read_bytes()]
        files += [(case / f"{name}{ext}").read_bytes() for name in ("ours", "theirs", "result")]
        got = git_merge_ours(files[0], files[1], files[2])
        return got is not None and got == files[3]
    return False


def mine_repo(
    name: str,
    gitdir: Path,
    out: Path,
    remaining: int,
    per_path: dict[str, int],
    seen_keys: set[tuple[str, str, str]],
) -> int:
    if remaining <= 0:
        return 0
    merges = git(gitdir, "rev-list", "--merges", "HEAD").stdout.decode().split()
    taken = 0
    print(f"  {len(merges)} merges", flush=True)
    for nth, merge in enumerate(merges, start=1):
        if taken >= remaining:
            break
        if nth % 200 == 0:
            print(f"  scanned {nth} merges, kept {taken}", flush=True)
        parents = git(gitdir, "log", "-1", "--format=%P", merge).stdout.decode().split()
        if len(parents) != 2:
            continue
        ours_rev, theirs_rev = parents[0], parents[1]
        base_r = git(gitdir, "merge-base", ours_rev, theirs_rev, check=False)
        if base_r.returncode != 0:
            continue
        base_rev = base_r.stdout.decode().strip()
        if not base_rev:
            continue
        both = conflicted_paths(gitdir, ours_rev, theirs_rev)
        for path in both:
            if taken >= remaining:
                break
            if not path.endswith((".rs", ".toml")):
                continue
            if (name, merge, path) in seen_keys:
                continue
            if per_path[path] >= MAX_PER_PATH:
                continue
            b = show(gitdir, base_rev, path)
            o = show(gitdir, ours_rev, path)
            t = show(gitdir, theirs_rev, path)
            res = show(gitdir, merge, path)
            if None in (b, o, t, res):
                continue
            assert b is not None and o is not None and t is not None and res is not None
            if max(len(b), len(o), len(t), len(res)) > MAX_BYTES:
                continue
            if b == o or b == t:
                continue
            # Only git's landing-order resolution. Hand edits are not scored.
            if git_merge_ours(b, o, t) != res:
                continue
            # merge-tree already listed this path as conflicted.
            n = next_case_id(out)
            case = out / f"{n:04d}"
            while case.exists():
                n += 1
                case = out / f"{n:04d}"
            case.mkdir(parents=True)
            ext = Path(path).suffix
            (case / f"base{ext}").write_bytes(b)
            (case / f"ours{ext}").write_bytes(o)
            (case / f"theirs{ext}").write_bytes(t)
            (case / f"result{ext}").write_bytes(res)
            (case / "meta.json").write_text(
                json.dumps(
                    {
                        "repo": name,
                        "merge": merge,
                        "ours": ours_rev,
                        "theirs": theirs_rev,
                        "base": base_rev,
                        "path": path,
                        "git_conflicted": True,
                        "label": "git-merge-ours",
                    },
                    indent=2,
                )
                + "\n"
            )
            per_path[path] += 1
            seen_keys.add((name, merge, path))
            taken += 1
            print(f"  {n:04d} {name} {path} @ {merge[:12]}", flush=True)
    return taken


def main() -> int:
    root = Path(__file__).resolve().parents[3]
    out = root / "corpora" / "merges"
    out.mkdir(parents=True, exist_ok=True)
    cache = Path(os.environ.get("HORD_CORPORA", Path.home() / ".cache/hord/corpora"))
    repos = [
        ("cargo", cache / "cargo.git"),
        ("tokio", cache / "tokio.git"),
    ]
    have_ours = sum(1 for p in out.iterdir() if p.is_dir() and label_is_ours(p))
    print(f"{have_ours} labels already equal git merge-file --ours (target {OURS_TARGET})")
    if have_ours >= OURS_TARGET:
        return 0
    per_path: dict[str, int] = defaultdict(int)
    seen_keys: set[tuple[str, str, str]] = set()
    for d in out.iterdir():
        meta = d / "meta.json"
        if meta.exists():
            m = json.loads(meta.read_text())
            per_path[m["path"]] += 1
            seen_keys.add((m["repo"], m["merge"], m["path"]))
    need = OURS_TARGET - have_ours
    print(f"need {need} more git-merge-ours cases in {out}")
    for name, gitdir in repos:
        if need <= 0:
            break
        if not gitdir.exists():
            print(f"skip {name}: missing {gitdir}")
            continue
        print(f"mining {name} ({gitdir})")
        got = mine_repo(name, gitdir, out, need, per_path, seen_keys)
        need -= got
    total_ours = sum(1 for p in out.iterdir() if p.is_dir() and label_is_ours(p))
    print(f"git-merge-ours labels: {total_ours}")
    return 0 if total_ours >= OURS_TARGET else 1


if __name__ == "__main__":
    sys.exit(main())
