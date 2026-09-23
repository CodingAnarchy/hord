# ADR 0013: Cargo.lock is a bridge in hord-lang-rust over a generic TOML merge

- **Status:** accepted
- **Date:** 2026-09-23
- **Spec:** §4.4, §11 (crate layout: "TOML Tier 1 + Cargo.lock adapter"), §12 M3
- **Blocks:** M3 acceptance "`Cargo.lock` concurrent dependency additions merge cleanly"

## Problem

Spec §4.4 asks for a purpose-built `Cargo.lock` adapter in M3, and §11 puts it in `hord-lang-toml`. A lockfile merge has to know Cargo's rules: how a dependency is spelled (`name`, `name version`, `name version (source)`), how packages are ordered (name, then semver version, then source kind), and the exact layout Cargo writes. If that code lives in `hord-lang-toml`, the TOML crate has package-manager knowledge, and every other TOML lockfile would need its own special case in that crate. The rule we want is that Rust-specific logic lives only in `hord-lang-rust`.

## Options

1. **Cargo.lock adapter in `hord-lang-toml` (spec §11 as written).** The adapter names `[[package]]` by package id, and a Cargo-specific merge sits next to the TOML grammar. Simple, but it makes the TOML crate Cargo-aware.
2. **Generic TOML merge in `hord-lang-toml`, and a Cargo bridge in `hord-lang-rust`.** `hord-lang-toml` names every `[[x]]` element by its leading scalar values and provides `merge::merge_docs`: arrays of tables merge as keyed sets, configured arrays merge as sets, and the result is emitted in a configured layout. `hord-lang-rust::cargo_lock` supplies Cargo's configuration and adds dependency re-spelling and canonical order.

## Decision

Option 2. `hord-lang-toml` has no Cargo knowledge. `hord_lang_rust::cargo_lock` provides `CargoLockAdapter` (the TOML adapter restricted to `Cargo.lock` paths) and `merge_cargo_lock`, the entry point structural rebase calls for `Cargo.lock` paths. This supersedes the §11 placement of the "Cargo.lock adapter" only.

## Consequences

- Every TOML file gets the leading-scalar naming for `[[x]]` elements, so `[[bin]] name = "x"` is `bin::x` instead of a shared `bin` name. The same change fixes table header names, which previously kept a trailing `]` (`foo]`).
- Another lockfile format (`uv.lock`, `poetry.lock`) can reuse the generic merge with its own configuration from its language crate.
- The generic merge re-emits the file in a canonical layout. It is meant for machine-written TOML, and comments other than the leading block are not preserved.
- On cargo.git history, every modern `Cargo.lock` (617) re-emits byte-identical, and the merge reproduces the committed lockfile for all 93 two-parent merges where both parents changed it (one hand-resolved commit is non-canonical input).

## Amendments (2026-09-23, from implementation)

- `CargoLockAdapter` reports its own language id, `cargo-lock`, not `toml`, so an adapter lookup by language alone is unambiguous. The registry order is `CargoLockAdapter`, Rust, TOML.
- Each `[[package]]` has its own NodeId. Two changes that each add a dependency to the same crate write the same `dependencies` node. That is a write-write conflict at §6.3, and `merge_cargo_lock` resolves it at rung 1.
