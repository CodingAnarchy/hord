# OPEN (agent `ladder`): take-theirs across a definition moved to another file

- **Raised:** 2026-09-25, while making `hord arbitrate --pick theirs` follow identity
- **Spec:** §3.4 (node identity), §5.2 (structural merge), §6.4 rung 3; ADR 0020 (file move identity)

## Question

"Take theirs" must splice the parked change's text of a contested definition at head's current location of that `NodeId`, so that a definition head moved is not duplicated. That works when head moved the definition within its file: the `NodeId` is carried, and the splice follows it.

When head moves a definition to another file (for example `pub fn parse` from `src/lib.rs` into a new `src/util.rs` with `pub use util::parse`), hord records it as a `Death` of the old id and a `Birth` of a new one. There is no `DerivedFrom` link between them (checked: a probe landing such a move records `[Birth(new), Death(old)]`). Nothing says the new `parse` is the old one, so take-theirs has no location to follow. It re-adds the parked side's `parse` at its old place in `src/lib.rs`, which duplicates it. Verification then fails, and the change goes back to the arbitration queue. It never lands broken, but the move-vs-edit case cannot be resolved by `--pick theirs`.

Is a definition moved across files the same definition to hord?

## Options

1. **Carry identity across files (amend ADR 0020).** When a change deletes a definition in one file and adds one in another with the same kind and qualified-name tail, and an equal or near-equal body, record `DerivedFrom { node: new, from: old }`, or keep the old id. Everything that follows identity benefits: take-theirs, blame, `node_history`, and the rung-1 merge, which could then combine move + edit itself. This is an identity-heuristic decision, so it needs an ADR and the M2 identity corpus rerun.
2. **Take-theirs matches by name and content only.** When a contested definition is gone from head's file, look for a definition head's landed changes gave birth to, anywhere, with the same kind and name and the base's content, and splice there. This is local to arbitration, with no identity change, but it is a heuristic nothing else shares, and blame and history still see a death and a birth.
3. **Leave cross-file moves to the arbiter.** Keep the current behavior: re-add at the old place, verification fails, and the change stays parked. The arbiter uses `--edit` or a candidate.

**Recommendation:** option 1. The same gap shows up in blame and history, not only in arbitration, and ADR 0020 already decided the analogous question for whole files. Option 2 is a reasonable stopgap if M5 cannot wait for an identity ADR.

## A related finding (rung 1, outside this slice)

A move of a definition *within* a file against an edit of it does not reach arbitration at all: the rung-1 rebase lands both. Probe: head moves `pub fn delta() { 4 }` into a new `pub mod math { … }` in `src/lib.rs` (same `NodeId`, recorded as `DerivedFrom` itself); a change on the old base edits `delta` to return 44. The lander lands the edit as a *new* top-level `pub fn delta() { 44 }` beside the moved, unedited `math::delta`. The result compiles, so verification does not catch it. The expected result is the moved `math::delta` returning 44, or a hard conflict. This belongs to the structural merge (hord-diff / the rebase), not to the ladder.
