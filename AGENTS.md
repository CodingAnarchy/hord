# Hord

Semantic VCS. The spec is the contract: `docs/spec.md`.

## Authority

- **DECIDED** items in the spec are not up for relitigation. Changing one requires an ADR that supersedes it.
- **OPEN** items must be resolved with an ADR in `docs/adr/` before the choice is implemented.
- A milestone is done when its **acceptance suite in spec §12 passes**, not when the crates exist.

## Current target

M0–M3 acceptance is green. Current work is **M4** (verification and policy: `hord-verify`, `hord-verify-rust`, `hord-policy`, and the server foundation `hord-api`, `hord-remote`, `hord-server`). Do not start M5+ crates except as a documented, required dependency of an M4 deliverable.

Acceptance harnesses (release mode; corpora in `$HORD_CORPORA` or `~/.cache/hord/corpora`) must stay green:

- M0: `cargo run -p hord-eval --release`
- M1: `cargo run -p hord-eval-m1 --release`
- M2: `cargo run -p hord-eval-m2 --release`
- M3: `cargo run -p hord-eval-m3 --release` (100-agent concurrency simulation, workspaces, `Cargo.lock`)

M3 decisions that constrain later work: read sets (ADR 0012), lander merge mode (0014), file-root ids and content-derived write sets (0015), copy-on-write directory workspaces (0016), and the `Cargo.lock` bridge living in `hord-lang-rust` (0013). Language-specific logic belongs only in its language adapter crate.

## Engineering (spec §11.1)

- Rust edition 2024. MSRV = current stable; bump freely.
- `cargo clippy --all-targets -- -D warnings` and `cargo fmt --check` gate every landing.
- `#![forbid(unsafe_code)]` in every crate except `hord-vfs`. One exception: `hord-cli` denies it and allows one Windows-only function that keeps the CLI's stdio out of the daemon (ADR 0027).
- Property tests (`proptest`) for encoding round-trip, parse/project losslessness, `apply(base, diff(base, result)) == result`, and specified merge commutativity.
- Every public type documented. `cargo doc` warnings are errors.
- Errors: `thiserror` in libraries, `anyhow` only in `hord-cli`.
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
