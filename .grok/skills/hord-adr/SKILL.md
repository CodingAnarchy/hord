---
name: hord-adr
description: >
  Write a Hord architecture decision record for a spec OPEN (or superseding a
  DECIDED) item. Use when resolving identity heuristics, diff algorithm,
  rust-analyzer, read-set collection, test selection, policy language, remote
  protocol, replay, or any spec §14 question, or when the user runs /hord-adr.
metadata:
  short-description: "Write a Hord ADR"
argument-hint: "<open question or spec section>"
---

# Hord ADR

Write one short ADR per decision. Do not implement the choice in the same turn unless the user already approved the ADR.

## Locate the question

1. Read spec §14 and the section cited by the user (or by the current milestone in `.grok/skills/hord/references/sections.md`).
2. If the item is **DECIDED** and the user is not asking to supersede it, stop. Quote the decided text.
3. List existing files in `docs/adr/` matching `NNNN-*.md`, ignoring `0000-template.md`. Next number is max+1, zero-padded to 4 digits.

## Write

Copy structure from `docs/adr/0000-template.md` into `docs/adr/NNNN-kebab-slug.md`.

Keep it short:

- **Problem** — what blocks the milestone if we don't decide
- **Options** — 2–4 real options with tradeoffs, not strawmen
- **Decision** — one choice, two sentences max
- **Consequences** — what we will not revisit without a new ADR

Status is `proposed` until the user accepts it, then `accepted`.

## After writing

Show the user the path and the decision. Ask them to accept, pick another option, or edit. Do not start coding the decision until they accept.
