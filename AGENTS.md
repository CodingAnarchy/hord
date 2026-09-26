# Hord

Semantic VCS. The spec is the contract: `docs/spec.md`.

## Authority

- **DECIDED** items in the spec are not up for relitigation. Changing one requires an ADR that supersedes it.
- **OPEN** items must be resolved with an ADR in `docs/adr/` before the choice is implemented.
- A milestone is done when its **acceptance suite in spec §12 passes**, not when the crates exist.

## Current target

M0–M5 acceptance is green. Current work is **M6** (self-hosting: hord's own repository served by `hord serve` on a team host, the git mirror, TLS for `hord serve`, web UI views 4–6, and the usability check moved from M5 by ADR 0035). Do not start M7 work except as a documented, required dependency of an M6 deliverable.

Acceptance harnesses (release mode; corpora in `$HORD_CORPORA` or `~/.cache/hord/corpora`) must stay green:

- M0: `cargo run -p hord-eval --release`
- M1: `cargo run -p hord-eval-m1 --release`
- M2: `cargo run -p hord-eval-m2 --release`
- M3: `cargo run -p hord-eval-m3 --release` (100-agent concurrency simulation, workspaces, `Cargo.lock`), also with `--server`, `--policy`, and `--server --policy`. CI runs these with `--report-throughput`, because shared runners are too noisy to gate the 20 changes/s target; that gate is checked on an idle machine without the flag.
- M4: the selection gate is the `M4 selection gate` workflow (`.github/workflows/m4-eval.yml`), dispatched on CI only (15 chains of 10 cargo commits plus full-suite samples, several hours). Do not run it locally. It passes on zero *confirmed* misses and a median selected share of at most 20% for write sets of 5 or fewer (ADR 0023 as amended).

- M5: `cargo run -p hord-eval-m5 --release -- run` (the 100-case conflict corpus with its scripted harness; CI's `M5 conflict corpus` job gates correctness only). The replay-rate criterion needs a real model: see the pilot command in `corpora/m5/README.md`. Never run a model from an agent without the user's go-ahead: it spends their usage.

M4 was accepted on 2026-09-26:
- Gate run 36214480176 on main `12cbd88`: 0 confirmed misses in 163 graded faults, and a 3.1% median selected share for write sets of 5 or fewer.
- Idle-machine throughput: 35.8–47.6 changes/s across the six M3 variants, against a target of 20.

M5 was accepted on 2026-09-26:
- Conflict corpus v4 with Sonnet 5 (`claude-sonnet-5`), $4.09: 76 of 100 resolved by replay (all 76 resolvable cases), against a target of 60%. All 24 contradictions (12 direct, 12 indirect) parked with honest explanations. 0 gamed, 0 tampered, 0 budget violations.
- Every parked case was resolved from the workbench with a signed `Arbitrated` event.
- A human rated 24 of 24 parked summaries sufficient to resolve, against a target of 90%.
- The 5-person usability check moved to M6 (ADR 0035).

Decisions that constrain later work:
- M3: read sets (ADR 0012), lander merge mode (0014), file-root ids and content-derived write sets (0015), copy-on-write directory workspaces (0016), and the `Cargo.lock` bridge living in `hord-lang-rust` (0013). Language-specific logic belongs only in its language adapter crate.
- M4: coverage-derived test selection with conservative fallbacks (ADR 0022; its 2026-09-25 amendments end staleness once a test re-ran on the change and scope test-file edits to their module), the selection safety harness (0023; a miss is a confirmed miss), gRPC with `hord.proto` as the one schema and the per-repo daemon (0024, 0021), evidence beside snapshots and the verifier contract (0025), the policy file and evidence qualifiers (0026), and the Windows daemon stdio exception to `forbid(unsafe_code)` (0027).
- M5: policy stays declarative TOML (ADR 0028), one replay per attempt judged by evidence (0029), the UI as a server-side client of the gRPC API plus the `Changes` service (0030), reviews carrying across a clean rebase (0031), M5 authentication scope with TLS in M6 and OIDC an M7 candidate (0032), exact-body cross-file moves keeping identity (0033), and replays never changing, shadowing, or disabling the acceptance tests they must satisfy, checked by a pinned run (0034).

## Engineering (spec §11.1)

- Rust edition 2024. MSRV = current stable; bump freely.
- `cargo clippy --all-targets -- -D warnings` and `cargo fmt --check` gate every landing.
- `#![forbid(unsafe_code)]` in every crate except `hord-vfs`. One exception: `hord-cli` denies it and allows one Windows-only function that keeps the CLI's stdio out of the daemon (ADR 0027).
- Property tests (`proptest`) for encoding round-trip, parse/project losslessness, `apply(base, diff(base, result)) == result`, and specified merge commutativity.
- Every public type documented. `cargo doc` warnings are errors.
- Errors: `thiserror` in libraries, `anyhow` only in `hord-cli`.
- No `.unwrap()`, in library, binary, bench, or test code. Enforced by `clippy::unwrap_used`:
  - Code that can fail returns the error with `?`, adding context where the crate's error type allows.
  - `.expect("…")` is only for a true invariant, and its message says why the call cannot fail.
  - Tests return `Result` and use `?`, or use `.expect("…")` naming what was being attempted, so a failure says what broke.
  - `unwrap_or_else(|e| panic!(…))` counts as an unwrap. A test that needs the error in its message returns `Result` and uses `map_err(|e| format!("{context}: {e}"))?`.
- Imports go in `use` statements at the top of the file, or of the inline module (`mod tests { use … }`), not as full paths inline (`crate::a::b::f()`, `std::collections::BTreeMap::new()`). Exceptions:
  - a qualified path that disambiguates two items with the same name;
  - a path inside a macro body, where hygiene needs it.
  Existing code is not yet converted. Apply the rule to code you write or touch.
- Async: `tokio`. No blocking I/O in async contexts.
- No hand-rolled parsers, hashes, or serialization formats. Use `tree-sitter`, `blake3`, canonical CBOR.
- Deterministic everything: same inputs → same `ObjectId`s. Non-determinism is a P0 bug.

Preferred dependencies: `tree-sitter`, `tree-sitter-rust`, `tree-sitter-toml`, `blake3`, `redb`, `zstd`, `gix`, `tokio`, `hyper`/`axum`, `proptest`, `clap`, `serde`, `ulid`, `diffy`. New deps need a one-line justification in the ADR log.

## Layout

Crate map is spec §11. ADRs are `docs/adr/NNNN-slug.md`. Intent files (when proposing changes) are Markdown with YAML front matter (`summary`, `refs`, `acceptance`).

## Skills

Same skill files for Grok and Claude:

- Canonical: `.grok/skills/` (`hord`, `hord-adr`, `hord-herdr`)
- Claude Code also loads `.claude/skills/` (symlink to `.grok/skills/`)

Follow `/hord` for implementation. Use `/hord-adr` for OPEN decisions. Use `/hord-herdr` only inside Herdr to fan panes out.

## Herdr

`herdr agent start` supports `--kind grok` and `--kind claude`. The **orchestrating** agent picks the kind per pane (mix allowed). Default to Grok unless Claude is a better fit (user asked, the other kind is not ready, or independent crates benefit from a second implementation). Both kinds must `--cwd` this repo root so they load these skills. OPEN items stay on the orchestrator; do not let two panes answer the same OPEN.
