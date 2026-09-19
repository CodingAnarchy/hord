# ADR 0003: Large generated files

- **Status:** proposed
- **Date:** 2026-09-19
- **Spec:** §14 item 10 (OPEN)
- **Blocks:** M1 (adapter vs blob choice)

## Problem

Bindings and protobuf output can be huge. Parsing them is expensive; storing them as blobs loses structural merge. M1 acceptance requires `project(parse(f)) == f` for **every** `.rs` and `.toml` file in the M0 corpora, including generated ones.

## Options

1. **Parse every adapter-matched file, no size cutoff.** Simple, satisfies M1 losslessness, may be slow on generated `*.rs`.
2. **Size cutoff (e.g. 1 MiB) → blob even for `.rs`/`.toml`.** Faster import; fails M1 acceptance on corpora files over the cutoff.
3. **Name heuristics (`*.generated.rs`, `bindings.rs`) → blob.** Misses some generated files, false-positives others; still fails losslessness if a corpus `.rs` is skipped.

## Decision

Option 1 for M1: if `LangAdapter::matches` is true, parse. No size cutoff. Unmatched files stay blobs (Tier 0). Revisit a cutoff after measuring parse cost on cargo/tokio, via a new ADR.

## Consequences

Adapters must be lossless on generated Rust/TOML in the corpora. Blob-tier remains the default for languages without an adapter. A later cutoff must preserve the M1 losslessness suite or explicitly shrink it.
