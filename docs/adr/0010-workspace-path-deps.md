# ADR 0010: Resolve packages whose source is in the snapshot

- **Status:** accepted
- **Date:** 2026-09-23
- **Spec:** §4.2 (DECIDED, superseded for path deps only), §12 M2, ADR 0009
- **Blocks:** M2 reference recall on a multi-crate workspace

## Problem

Spec §4.2 resolves reference edges by name within one crate. On cargo HEAD, rust-analyzer find-references for the 300-function sample is 363/32943 (1.10%). 28183 of those misses are a name edge that does not resolve to the definition's `NodeId`. The sampled functions are mostly `cargo-test-support` helpers, and the sites are calls from `tests/testsuite` and other workspace crates. Dropping `tests/testsuite` still leaves at most about 67%. ADR 0009 forbids embedding rust-analyzer crates to close this. The 300-definition sample stays as specified. Attribute sites that sit outside a definition are a separate gap and are not this decision.

## Options

1. **Stay inside the crate.** The recall gate stays red on cargo. Registry and path dependencies are treated the same: an extern path is an unresolved name.

2. **Resolve another package only when its source is in the snapshot and this language's manifest links the name.** The snapshot is the boundary for every language. A Cargo workspace root, an npm workspace, or a Go workspace is how that adapter *discovers* packages. It is not one shared namespace. For Rust, each `Cargo.toml` path dependency (`path = "..."`, including dev and build dependencies, and `package = "..."` renames) maps that extern name onto the crate root at that path, and `cargo_test_support::project` resolves into that crate's module tree. `workspace = true` takes the path from `[workspace.dependencies]` in the workspace manifest that owns the package. Several matching roots are an over-approximation. Crates.io and git dependencies stay unresolved. No feature resolution, no trait resolution, no type inference. Still tree-sitter. `Cargo.toml` is read with the existing TOML adapter. A later language adds its own manifest reading. It does not reuse Cargo's.

3. **One namespace at the workspace root.** Every package in the snapshot is visible under its directory or package name from every file. Two packages that both define `foo::bar` collide, and a crate sees packages it does not depend on.

4. **Match any extern name to any crate root, ignoring manifests.** A workspace member and a registry crate of the same name collide, and a renamed path dependency points at the wrong package.

## Decision

Option 2. The snapshot is the resolution boundary. Rust crosses a crate boundary only along a path dependency.

## Consequences

Spec §4.2's phrase "within the crate" does not apply to a name whose target package is in the snapshot and linked by that language's manifest. For Rust that link is a path dependency, not workspace membership alone. Registry and git dependencies stay unresolved. We will not resolve those, and we will not do feature selection or type-directed method resolution, without a new ADR. Another language does not join a global workspace-root namespace. We will not change the 300-definition sample, and we will not embed `ra_ap_*`, to make the recall number pass.
