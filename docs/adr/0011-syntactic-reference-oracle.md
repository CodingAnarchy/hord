# ADR 0011: Grade reference recall on syntactic names

- **Status:** accepted
- **Date:** 2026-09-23
- **Spec:** §12 M2 (DECIDED, superseded for the reference oracle), §4.2, ADR 0009, ADR 0010
- **Blocks:** M2 reference recall

## Problem

Spec §12 requires recall ≥ 95% against rust-analyzer find-references. On cargo that sample is 5550/32943 (16.85%) after path dependencies. The misses are mostly trait methods (`CargoPathExt::assert_build_dir_layout`) and attributes that sit outside the function (`#[cargo_test]`). Spec §4.2 forbids type inference, trait resolution, and macro expansion, and ADR 0009 forbids embedding rust-analyzer to supply them. A gate that counts those sites cannot pass. The lexical stand-in (712/712) only checks that a name string occurs, so it is not this oracle either.

## Options

1. **Keep find-references as the pass/fail bar.** M2 stays red unless Tier 2 grows a type checker or links rust-analyzer. Both contradict the decisions above.

2. **Lower 95% until today's number passes.** The bar would describe this corpus, not a rule. The next language would need another number.

3. **Grade a separate syntactic walk, and keep find-references as a printed report.** The walk is tree-sitter in the eval, not a call into the resolver, so a resolver bug can fail it. For each sampled definition, a site counts only when source names it without types or expansion:
   - A path resolves to it in the same crate, or through a manifest link to a package whose source is in the snapshot (ADR 0010).
   - A call or selector with no type counts when this definition is one visible same-named function or method in that crate or a linked package. Emitting every such candidate is a hit. Emitting one type-chosen candidate is not required.
   - An attribute or decorator immediately outside a definition belongs to that definition.
   - A site that exists only inside a macro expansion, or only because a type checker picked one impl, is not in the denominator.
   Recall of those sites stays ≥ 95%. Precision stays secondary. Over-approximation stays acceptable. rust-analyzer's count is still printed and still does not authorize `ra_ap_*`.

## Decision

Option 3. The M2 reference gate is syntactic name recall at 95%. Find-references is a report, not the bar.

## Consequences

We will not put rust-analyzer find-references back on the pass/fail gate without a new ADR. We will not add trait resolution, type inference, or macro expansion in order to pass M2. The resolver still has to attach an outer attribute to the next definition, and an unresolved call still has to include same-named definitions in manifest-linked packages. Those are how this oracle is met. They are not permission to embed `ra_ap_*`.
