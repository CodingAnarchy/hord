# ADR 0009: Do not embed rust-analyzer in Tier 2

- **Status:** accepted
- **Date:** 2026-09-23
- **Spec:** §4.2 (OPEN), §12 M2, §14.3
- **Blocks:** M2 reference-recall oracle and the Tier 2 backend

## Problem

Spec §4.2 leaves open whether `ra_ap_syntax`, `ra_ap_hir`, and `ra_ap_ide_db` replace the tree-sitter resolver, and says to decide after M2 metrics, weighing build time, API stability, and memory. Spec §12 also wants reference recall ≥ 95% against rust-analyzer's find-references. The metrics we have are from the tree-sitter adapter, not from that oracle: on cargo, 500/500 same-name functions kept their `NodeId`, rename precision was 27/27, a lexical stand-in recalled 712/712 identifier sites on 300 functions, and a warm `node_history` lookup over 14,526 definitions was 0.01 ms. Find-references itself has not been run. The `rust-analyzer` component is not installed here. Embedding the crates would put a second, unstable compiler frontend on the deterministic parse path before that gap is measured.

## Options

1. **Embed `ra_ap_*` as the Tier 2 backend now.** Precise references and macro expansion. The crates have no stability guarantee, compile times and resident memory are those of rust-analyzer, and analysis depends on a sysroot, cargo metadata, and build scripts. Reference edges would no longer be a pure function of snapshot bytes.

2. **Keep the tree-sitter resolver. Do not depend on `ra_ap_*`.** Writes stay sound and reads stay a conservative over-approximation, as §4.2 already requires. The §12 recall gate may shell out to the official `rust-analyzer` binary as an external oracle. A miss does not by itself authorize embedding.

3. **Embed `ra_ap_*` only inside `hord-eval-m2`.** The resolver stays tree-sitter, but the workspace still builds rust-analyzer to grade it. That pays the compile and API cost without putting hir on the store path.

## Decision

Option 2. Tier 2 stays the tree-sitter resolver. rust-analyzer is an external measurement tool, not a library dependency.

## Consequences

No `ra_ap_syntax`, `ra_ap_hir`, or `ra_ap_ide_db` dependency in this workspace. We will not replace or wrap the Tier 2 resolver with rust-analyzer's crates without a new ADR. A later sample under 95% find-references recall is evidence for that ADR. It is not permission to add the crates in the meantime.
