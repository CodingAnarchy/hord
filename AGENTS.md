# Hord

Semantic VCS. The spec is the contract: `docs/spec.md`.

## Authority

- **DECIDED** items in the spec are not up for relitigation. Changing one requires an ADR that supersedes it.
- **OPEN** items must be resolved with an ADR in `docs/adr/` before the choice is implemented.
- A milestone is done when its **acceptance suite in spec §12 passes**, not when the crates exist.

## Current target

M0 acceptance is green. Current work is **M1** (structural tier: `hord-lang*`, `hord-diff`). Do not start M2+ crates except as a documented, required dependency of an M1 deliverable.

## Engineering (spec §11.1)

- Rust edition 2024. MSRV = current stable; bump freely.
- `cargo clippy --all-targets -- -D warnings` and `cargo fmt --check` gate every landing.
- `#![forbid(unsafe_code)]` in every crate except `hord-vfs`.
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
