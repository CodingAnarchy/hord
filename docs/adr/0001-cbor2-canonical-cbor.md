# ADR 0001: Use cbor2 for canonical CBOR

- **Status:** accepted
- **Date:** 2026-09-18
- **Spec:** §3.9 (DECIDED)
- **Blocks:** M0

## Problem

The preferred dependency list has no CBOR crate. Spec §3.9 requires RFC 8949 §4.2.1 via a Rust crate with a canonicalization mode.

## Options

1. **cbor2.** `to_canonical_vec` implements RFC 8949 §4.2.1 (preferred serialization, definite lengths, map keys sorted by encoded bytes).
2. **ciborium.** Preferred integer encoding, but its canonical module is RFC 8949 §4.2.3 length-first, not §4.2.1.
3. **dcbor.** Extra profile rules (numeric reduction, NFC) beyond §4.2.1.

## Decision

Use `cbor2` (`to_canonical_vec`) in `hord-encoding`.

## Consequences

`ObjectId`s are BLAKE3-256 of cbor2's RFC 8949 §4.2.1 output. Changing CBOR crates needs a new ADR and golden vectors.
