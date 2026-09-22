# M1 merge corpus

200 three-way cases mined from real git merges in `rust-lang/cargo` and
`tokio-rs/tokio` where Git reported a content conflict (`git merge-tree`
and/or `git merge-file`). Spec §12 M1.

Each directory `NNNN/` holds:

- `base.{rs,toml,md}` — merge-base blob
- `ours.{rs,toml,md}` — first parent (branch merged into)
- `theirs.{rs,toml,md}` — second parent
- `result.{rs,toml,md}` — blob at the merge commit (human resolution)
- `meta.json` — repo, SHAs, path

`.rs` / `.toml` go through `hord_diff::merge`. `.md` is blob-tier
(`merge_blob` / diffy).

Remine:

```bash
python3 bench/m1-eval/scripts/mine_merges.py
```

Evaluate:

```bash
cargo run -p hord-eval-m1 --release -- --merges-only
```

Targets: auto-resolve ≥ 70% of Git-conflicted cases; 100% of auto-resolutions
parse; ≥ 95% of auto-resolutions match `result` (byte-equal, trivia-stripped
equal, or the same definition `normalized` set). Name presence alone does
not count (ADR 0005).
