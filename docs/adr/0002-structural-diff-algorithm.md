# ADR 0002: Structural diff algorithm

- **Status:** accepted
- **Date:** 2026-09-19
- **Spec:** §5.1, §14 item 2 (OPEN)
- **Blocks:** M1 (`hord-diff`)

## Problem

M1 must emit `Vec<Op>` such that `apply(base, diff(base, result)) == result` after projection. The spec leaves the matching algorithm OPEN. `hord-diff` cannot start until we pick one.

## Options

1. **GumTree-style with `normalized` anchors.** Top-down exact matches on `normalized` hashes, then bottom-up recovery of moved/renamed definitions. No new crate. Matches the spec's "start with" language. Sub-definition edits collapse to `Replace` on the enclosing definition. Not optimal; correctness is `apply` identity.
2. **`tree-sitter-edit` / similar edit-script crate.** Reuse a published tree-edit distance. New dep, grammar-shaped trees rather than Hord `Node`s, so we still wrap. Faster to a first script, worse fit to `Op::{Move,Rename}`.
3. **Patience/histogram on serialized trees.** Treat the CST as a token sequence. Easy via `diffy` (already a preferred dep) but throws away structure we just parsed.

## Decision

Option 1: GumTree-style matching anchored on `normalized` hashes, definition granularity for `Op`s. Implement in `hord-diff` with no new crate. Optimality is a quality metric, not a gate.

## Consequences

`hord-diff` does not take `tree-sitter-edit` or a line-diff of CSTs. Rename similarity (spec §3.4 step 4) stays OPEN until M2; M1 `identify` uses exact/named/moved only. Changing the matcher requires a new ADR.
