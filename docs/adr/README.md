# Architecture decision records

One ADR per DECIDED or OPEN spec item that we resolve. Short: problem, options, decision, consequences.

Use `/hord-adr` or copy `0000-template.md`. Number sequentially. Filename: `NNNN-kebab-slug.md`.

Dependency pins (not full ADRs): `tree-sitter-toml-ng` 0.7 is the maintained TOML grammar on crates.io; the name `tree-sitter-toml` is frozen at 0.20 and does not build against tree-sitter 0.25.

`serde_yaml_ng` 0.10 (hord-cli only): reads the YAML front matter of intent files (spec §10.3). It is the maintained fork of the archived `serde_yaml`; AGENTS.md forbids a hand-rolled parser.

Dependency additions (one line each):

- `toml` 1 (`hord-lang-toml`, feature `preserve_order`): decodes and prints TOML values for the generic lockfile merge (ADR 0013). It is Cargo's own TOML crate, so printed values match what Cargo writes, and it avoids a hand-rolled TOML value decoder.
- `semver` 1 (`hord-lang-rust`): orders `Cargo.lock` packages by semver version, as Cargo's `PackageId` does (ADR 0013).

`reflink-copy` 0.1 (hord-txn only, ADR 0016): safe wrapper over `clonefile(2)` / `FICLONE` for copy-on-write workspace checkouts, so no hord crate needs `unsafe` outside `hord-vfs`.
