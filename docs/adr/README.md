# Architecture decision records

One ADR per DECIDED or OPEN spec item that we resolve. Short: problem, options, decision, consequences.

Use `/hord-adr` or copy `0000-template.md`. Number sequentially. Filename: `NNNN-kebab-slug.md`.

Dependency pins (not full ADRs): `tree-sitter-toml-ng` 0.7 is the maintained TOML grammar on crates.io; the name `tree-sitter-toml` is frozen at 0.20 and does not build against tree-sitter 0.25.
