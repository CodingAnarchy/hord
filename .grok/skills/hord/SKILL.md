---
name: hord
description: >
  Implement Hord (semantic VCS) from docs/spec.md. Use when working in the
  hord repo, implementing a milestone (M0–M7), touching hord crates
  (hord-encoding, hord-core, hord-store, hord-git, lander, adapters), or
  when the user says implement hord, continue hord, build hord, or /hord.
metadata:
  short-description: "Implement Hord from the spec"
argument-hint: "[M0|M1|M2|M3|M4|M5|M6|M7|crate]"
---

# Hord

You are implementing Hord. The spec at `docs/spec.md` is the contract. Project rules in `AGENTS.md` always apply.

## Setup

1. Confirm cwd is the hord repo root (contains `docs/spec.md`). If not, stop and say so.
2. Read `AGENTS.md` and `.grok/skills/hord/references/sections.md`.
3. Infer the current milestone:
   - No `crates/` workspace → **M0**
   - Otherwise the first milestone in spec §12 whose acceptance suite is not green
4. If the user named a milestone or crate, use that. Do not skip ahead of the current milestone.

## Spec discipline

- Read the spec sections for this milestone from `references/sections.md`. Use `read_file` with offsets; do not load the whole spec unless necessary.
- **DECIDED** text is law. Do not reopen it in code comments, types, or ADRs except to record a supersession.
- **OPEN** items that this work must choose: stop and run the `hord-adr` skill first. Do not pick silently.
- Field names in spec types are normative; exact shapes are not. Do not invent parallel names for the same concept (`ObjectId`, `NodeId`, `ChangeRecord`, `Op`, …).

## Implement

1. Create only the crates listed for this milestone in spec §11 / §12. Leave later-milestone crates uncreated.
2. Match engineering conventions in `AGENTS.md` (edition 2024, clippy `-D warnings`, `forbid(unsafe_code)` except `hord-vfs`, `thiserror` vs `anyhow`, tokio, no hand-rolled encodings).
3. Canonical encoding is canonical CBOR (RFC 8949 §4.2.1) in `hord-encoding`, with golden vectors in-repo. Do not hand-roll a format.
4. Git bridge (M0) is byte-exact import/export via `gix`. Round-trip of tree SHAs is an invariant, not a stretch goal.
5. Prefer the dependency list in `AGENTS.md`. A new crate needs a one-line justification in the ADR log.

## Done

A milestone is done only when its **executable acceptance** in spec §12 passes. "The code exists" is not done.

After a slice of work:

- `cargo fmt --check`
- `cargo clippy --all-targets -- -D warnings`
- the tests that slice claims to cover

If you are inside Herdr and the user asked to parallelize across panes, switch to the `hord-herdr` skill. Do not spawn Herdr agents just because the milestone is large.
