# ADR 0012: Read sets are access log ∪ outgoing references ∪ declarations

- **Status:** accepted
- **Date:** 2026-09-23
- **Spec:** §3.5, §6.1, §6.3, §14 item 4 (OPEN)
- **Blocks:** M3

## Problem

§6.3 compares `read_set` against landed `write_set`s. §3.5 names three sources for the read set (access log, adapter references from written nodes, declarations) and requires a superset, but not how each is collected, how far references reach, or whether agents must declare. Too little and the lander lands a change whose assumptions moved (a false negative, which the M3 simulation requires to be 0). Too much and the ≤ 10 % false-positive target fails. M3 has no VFS (§6.1, M7), so a `Directory` workspace cannot observe reads.

## Options

1. **Access log only.** Exact for `InMemory` agents, empty for `Directory` agents until the VFS exists. Anything an agent read by `cat` or `grep` is missed. Under-declares by default.

2. **Adapter references only.** The read set is every definition that a written definition names (outgoing `References` edges in the result snapshot). Deterministic and independent of the harness. Misses reads that do not show up as a name in the written code (the agent read `g`'s docs, then changed `f` to match).

3. **Union, declarations optional.** `read_set = access_log.reads ∪ refs(write_set) ∪ declared`:
   - **Access log.** `InMemory` reads are recorded at definition granularity. Reading a node records its `NodeId`. Reading a file or a byte range records every definition that the read overlaps, plus the file's path. `Directory` workspaces log writes only in M3 and report reads as unobserved.
   - **References.** One hop. Add every target of an outgoing `References` edge from each written definition, taken from the result snapshot. Add the base snapshot's targets too, so dropping a call still reads what it used to call. Do not add dependents (edges into the written node). Verification handles those (§6.5).
   - **Declarations.** Optional `reads:` list in the intent front matter or API, as names or `NodeId`s. Unresolvable names are an error at `propose`.
   - **Blob tier.** Paths are identities (§6.3). Reading or writing a blob-tier file adds its path to the matching set.

4. **Union with required declarations.** Same as option 3, but `propose` rejects a change with no declared reads. Agents over-declare to avoid rejection, and that pushes up false positives without adding safety over reference edges.

## Decision

Option 3. The read set is the union of the definition-granularity access log, one hop of outgoing references from written definitions (base and result), and optional declarations. No source is trusted over another; they are unioned.

## Consequences

- `Directory` workspaces under-declare reads that do not appear as names until the VFS lands (§15 risk). `hord status` says reads are unobserved. `strict_reads` policy (M4) is the escape hatch.
- The M3 oracle defines "true overlap" as write∩write plus write∩(what the agent read through the API ∪ what its written code names). A false negative against that definition is a bug. A conflict outside it counts as a false positive.
- If the simulation's false-positive rate goes over 10 % because of reference edges, we narrow the hop (for example, drop base-snapshot targets). We do not drop references entirely without a new ADR.
- Requiring declarations, and reaching more than one hop, need a new ADR.
