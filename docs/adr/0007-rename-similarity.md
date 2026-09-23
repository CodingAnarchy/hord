# ADR 0007: Rename similarity metric and threshold

- **Status:** accepted
- **Date:** 2026-09-22
- **Spec:** §3.4 step 4, §14 item 1 (OPEN)
- **Blocks:** M2 identity stability and rename precision

## Problem

`default_identify` carries a `NodeId` when the body is unchanged (exact or moved) or the qualified name is unchanged under the same parent (named). A rename that also edits the body matches none of those. Spec §3.4 step 4 leaves the similarity metric and threshold open, with a hint to start at a tree-edit-distance ratio of at least 0.8. M2 cannot claim the 97% stability sample or the 95% rename-precision sample until this pair is fixed. A false rename welds two definitions' histories together, so the metric has to prefer precision over recall.

## Options

1. **Dice on `normalized` descendant hashes, threshold 0.8.** For two unmatched definitions of the same `NodeKind`, collect the multiset of `normalized` hashes of the definition node and every descendant. Similarity is `2 * |intersection| / (|left| + |right|)`. Pair greedily, highest score first. This is the similarity GumTree uses for recovery, which matches ADR 0002, and it needs no new crate. It is not optimal tree-edit distance. Shared boilerplate can still clear 0.8 when two short bodies are mostly the same shape.

2. **Optimal tree-edit-distance ratio ≥ 0.8.** This is the spec's hint read literally: `1 - ted(a, b) / max(|a|, |b|)`. It is the right number for a small tree and the wrong cost for every definition in cargo (`O(n^3)` or a new edit-script crate). ADR 0002 already refused an edit-script crate for the diff. Doing it only for rename does not make the cubic step cheap.

3. **Line or token Jaccard via `diffy`, threshold 0.8.** `diffy` is already a dependency. The score ignores structure, so constructors, empty methods, and trait stubs look alike after a rename. That is the failure mode most likely to miss the 95% precision gate.

4. **Dice as in option 1, but only when the simple names are also similar.** Cuts boilerplate false matches. Also drops a real rename whose new name shares nothing with the old one (`foo` to `handle_request`), which is the case the 97% stability sample is for. Name equality is already step 2.

## Decision

Option 2, on the trivia-stripped subtree, with a hard size cap. After steps 1–3, consider unmatched definitions of the same `NodeKind`. Build an ordered tree of the non-trivia nodes: an internal label is the `NodeKind`, a leaf label is the kind plus the stripped token text. Insert and delete cost 1. Relabel costs 1. The ratio is `1 - ted / max(count(a), count(b))`. Accept a pair at or above 0.8.

A ratio of 0.8 already requires the two counts to be within 20% of the larger one. Do not attempt the distance otherwise. If either subtree has more than 256 non-trivia nodes, it is not a rename. The implementation must be worst-case cubic per pair (APTED or equivalent) and must stop early when a lower bound already puts the ratio under 0.8. Assign one-to-one, highest ratio first; break ties by the base `NodeId` bytes, then the result node's preorder index. Emit `Op::Rename` and `IdentityDelta::DerivedFrom`.

## Consequences

Rename stays inside `identify` on the two trees it is given. A rename across files is found only when both files are in that pair of trees. A definition over 256 non-trivia nodes, or a pair whose sizes differ by more than 20%, is a birth and a death, not a guess. Kind-only trees are not allowed: leaf text is part of the label, or distinct functions with the same shape become false renames.

We do not add a grammar-coupled edit-script crate, a line-similarity rename, or a name-similarity gate. A pure ordered-tree distance crate is allowed only with a one-line justification in this ADR's history if the in-crate cubic algorithm is what proves too slow. rust-analyzer is not part of this decision (spec §4.2: after M2 metrics). If the 500-definition sample misses 97% stability or 95% rename precision, change the threshold, the 256 cap, or the metric in a new ADR. Do not retune 0.8 in the implementation.
