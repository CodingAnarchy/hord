# Spec sections to read per milestone

Do not copy spec text. Open `docs/spec.md` at these sections before implementing.

| Milestone | Read |
|---|---|
| M0 | §3.1–3.2, §3.7, §3.9, §8.1, §9 (import/export only), §10.1–10.2 (`init`, `ws`, `status`, `log`, `git import/export`), §11, §12 M0 |
| M1 | §3.3–3.4, §4.1–4.3 (Tier 1), §5, §11 (`hord-lang*`, `hord-diff`), §12 M1, OPEN #2 and #10 |
| M2 | §3.4, §3.8, §4.2–4.3 (Tier 2), §8.1 index tables, §10.2 (`blame`, `query`, `log --node`), §12 M2, OPEN #1 and #3 |
| M3 | §3.5, §6, §4.4 (`Cargo.lock`), §10.2 (`submit`, `queue`, `land --local`, `conflicts`), §12 M3, OPEN #4 |
| M4 | §3.6, §6.5, §7, §8.2, §10.5.1–10.5.3, §12 M4, OPEN #5 and #7 |
| M5 | §6.4, §6.6, §10.4 views 1–3, §10.5.4, §12 M5, OPEN #6 and #8 |
| M6 | §9 sync, §10.4 views 4–6, §12 M6 |
| M7 | §14 items 9–15, §12 M7 |

OPEN questions that block a milestone are listed in spec §14. Write an ADR (`/hord-adr`) before implementing a choice.
