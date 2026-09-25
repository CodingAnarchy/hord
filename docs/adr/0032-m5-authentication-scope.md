# ADR 0032: What M5 authentication covers, and where the rest is scheduled

- **Status:** accepted
- **Date:** 2026-09-25
- **Spec:** §10.5.4 (identity and authorization), §12 M5–M7; ADR 0021 (per-repo daemon), ADR 0024 (loopback-only binds without auth)
- **Blocks:** nothing; it schedules follow-up work

## Problem

M5 delivers token auth and scopes (§10.5.4). Several related pieces are either outside what §10.5.4 requires or deliberately different. Without a record, they read as silent gaps: transport security, OIDC, the local daemon's trust model, and signing of proposals that never touch a server.

## Options

For each item, the choice was between doing it in M5, scheduling it for a named later milestone, or declaring it out of scope by design. Decisions are below.

## Decision

- **The per-repo daemon takes no tokens (by design).** Its Unix socket or named pipe belongs to the OS user (ADR 0021), so a call to it is that user. The daemon verifies any signature that is present. It does not ask for bearer tokens.
- **Local proposals are signed in M5.** A proposal through the daemon (or `--no-daemon`) is signed with the user's key in `~/.hord/keys/`, created on first use, as §10.5.4 requires for every `ChangeRecord`.
- **TLS is M6.** M6 serves hord's own repository from a team host, the first time tokens cross a real network. M6 delivers TLS for `hord serve`: in-process `rustls`, or a documented terminating proxy. Until then, a non-loopback bind still needs `--insecure-bind` (ADR 0024).
- **OIDC is an M7 candidate.** §10.5.4 allows OIDC *or* a local user table for single-team deployments. The local table (M5) satisfies it through M6's single team.

## Consequences

- Spec §12: M6's Deliver line gains TLS, and M7's candidates gain OIDC login.
- M5's auth acceptance is judged on loopback and on the local daemon. No M5 test depends on TLS.
