# Architecture decision records

One ADR per DECIDED or OPEN spec item that we resolve. Short: problem, options, decision, consequences.

Use `/hord-adr` or copy `0000-template.md`. Number sequentially. Filename: `NNNN-kebab-slug.md`.

Dependency pins (not full ADRs): `tree-sitter-toml-ng` 0.7 is the maintained TOML grammar on crates.io; the name `tree-sitter-toml` is frozen at 0.20 and does not build against tree-sitter 0.25.

`serde_yaml_ng` 0.10 (hord-cli only): reads the YAML front matter of intent files (spec §10.3). It is the maintained fork of the archived `serde_yaml`; AGENTS.md forbids a hand-rolled parser.

Dependency additions (one line each):

- `toml` 1 (`hord-lang-toml`, feature `preserve_order`): decodes and prints TOML values for the generic lockfile merge (ADR 0013). It is Cargo's own TOML crate, so printed values match what Cargo writes, and it avoids a hand-rolled TOML value decoder.
- `semver` 1 (`hord-lang-rust`): orders `Cargo.lock` packages by semver version, as Cargo's `PackageId` does (ADR 0013).

`reflink-copy` 0.1 (hord-txn only, ADR 0016): safe wrapper over `clonefile(2)` / `FICLONE` for copy-on-write workspace checkouts, so no hord crate needs `unsafe` outside `hord-vfs`.

`windows-sys` 0.61 (hord-cli, Windows only, feature `Win32_Foundation`, ADR 0027): `SetHandleInformation`, so the daemon does not inherit the CLI's stdio.

`ignore` 0.4 (hord-txn only, ADR 0016): git's `.gitignore` matching semantics (anchoring, `**`, negation, directory-only patterns) for walking a `Directory` checkout, plus its parallel directory walker for the propose `lstat` pass. It is ripgrep's crate; AGENTS.md forbids a hand-rolled glob parser.

`globset` 0.4 (hord-policy only): compiles the `paths` globs of policy rules (spec §7.2). It is ripgrep's glob crate, already in the tree under `ignore`; AGENTS.md forbids a hand-rolled glob parser. `toml` 1 is also used by `hord-policy` to parse `policy.toml`, with spans for line and column errors.

ADR 0024 server-foundation dependencies (`hord-api`, `hord-server`, `hord-remote`, and `hord-txn` for `LocalRepo` and the lander task):

- `tonic` 0.14 and `tonic-prost` 0.14: the gRPC runtime and its prost codec for the generated client and server stubs (ADR 0024). Since tonic 0.14, prost code generation lives in `tonic-prost-build`, which replaces the ADR's `tonic-build` (it depends on it).
- `tonic-prost-build` 0.14 (build only): generates the prost types and tonic stubs from `hord.proto`, and writes the descriptor set.
- `prost` 0.14 and `prost-types` 0.14: protobuf encoding of the generated messages, and `FileDescriptorSet` decoding for the generated JSON Schema.
- `protoc-bin-vendored` 3 (build only): a vendored `protoc`, so building needs no system install (ADR 0024).
- `pbjson` 0.9 and `pbjson-build` 0.9 (build): the canonical protobuf JSON mapping as `serde` impls, for `hord --json` (ADR 0024).
- `async-trait` 0.1: `RepoBackend` is an object-safe async trait (`&dyn RepoBackend`, `Box<dyn RepoBackend>`), which native async traits are not.
- `tokio-stream` 0.1: the `Stream` type of `EventStream` and its channel wrappers.
- `tokio-util` 0.7 (hord-txn): `CancellationToken` for `Lander::spawn(repo, cancel)`.
- `redb` 2 (hord-txn, already a workspace dependency): the persisted event log `.hord/events.redb` (spec §10.5.3 `EventCursor`), kept apart from the store's index.
- `tonic-web` 0.14 (hord-server): gRPC-Web on the same port as gRPC, for the M5 web UI (ADR 0024).
- `axum` 0.8 (hord-server, dev in hord-remote; already in the tree under tonic's router): the plain-HTTP `GET /schema.json` route beside the gRPC services, and a webhook receiver in tests. AGENTS.md prefers it.
- `tower` 0.5 (hord-server, hord-remote; already in the tree under tonic): the `/r/<name>/` routing layer, request-activity tracking, and the client's path-prefix and local-endpoint connector.
- `http` 1, `hyper` 1, `hyper-util` 0.1, `http-body-util` 0.1, `bytes` 1 (hord-server, hord-remote; all already in the tree under tonic): request types for those layers, the webhook POST client (`hyper-util`'s legacy client over `HttpConnector`), and `TokioIo` for Unix-socket and named-pipe connections.
- `blake3` 1 (hord-api; already a workspace dependency): names a repository's daemon endpoint by hashing its canonical path.
- `toml` 1 (hord-server, hord-cli; already a workspace dependency): `server.toml` and `.hord/remotes.toml`.

`rustc-demangle` 0.1 (hord-verify-rust only, ADR 0022): demangles the Rust symbol names in `llvm-cov export` so a test's own function maps to its definition. It is the demangler the standard library uses, and AGENTS.md forbids a hand-rolled parser.

M5 identity and authorization dependencies (spec §10.5.4):

- `ed25519-dalek` 3 (hord-core, feature `pem`): Ed25519 signatures on `ChangeRecord.signature`, `Evidence.signature`, and other signed hord messages, with PKCS#8 PEM key files. It is the maintained dalek-cryptography implementation (RustCrypto `signature` traits); AGENTS.md forbids a hand-rolled format, and PKCS#8 is the standard one for private keys.
- `getrandom` 0.4 (hord-core, hord-server; already in the tree): the OS random source for key seeds and bearer tokens, without pulling in a `rand` stack.
- `argon2` 0.6 (hord-server): Argon2id password hashes (PHC strings) for the `hord login` user table. It is RustCrypto's implementation of the current OWASP-recommended password hash; AGENTS.md forbids hand-rolled hashing.
- `blake3` 1 and `hex` 0.4 (hord-server, hord-core; already workspace dependencies): the auth file stores each bearer token's BLAKE3 hash, never the token; key ids are hex.
