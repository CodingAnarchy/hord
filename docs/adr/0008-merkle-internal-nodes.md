# ADR 0008: Internal nodes do not hash raw bytes

- **Status:** accepted
- **Date:** 2026-09-23
- **Spec:** §3.3 (DECIDED), §3.9 (DECIDED)
- **Blocks:** M1 corpus time (parse intern)

## Problem

Spec §3.3 defines `ObjectId(node) = blake3(canonical(node))` over the stored node, which includes `raw`. Parsing a file content-addresses every node. An internal node's `raw` is the concatenation of its children, and its `normalized` hash is BLAKE3 over the concatenation of their stripped text. Every source byte is therefore copied and hashed once per ancestor. On a 224 KB Rust file that is about 6,600 nodes and about 37 ms, against about 14 ms for tree-sitter. The M1 apply(diff) corpus parses each changed blob and spends most of that time here. Reusing the CBOR buffer and replacing the node map did not move it. There is no stored history to migrate; existing `ObjectId`s may change.

## Options

1. **Keep hashing `raw` on every node.** Same `ObjectId`s. A hand-specialized encoder can skip serde overhead, but every ancestor still feeds the subtree bytes into BLAKE3. Spec §3.9 already forbids a second encoding.

2. **Drop `raw` from the internal stored object only.** The id is still BLAKE3 of the canonical stored bytes (`kind`, `lang`, `normalized`, `children`, `name`). Leaves are unchanged, so leaf ids stay. `normalized` for an internal node is still BLAKE3 of the concatenated stripped text, so the O(subtree) copy and hash remain.

3. **Merkle internal nodes.** An internal node's stored object has no `raw`. Its `normalized` id is BLAKE3 of the canonical CBOR array of its children's `normalized` ids, in child order. Its `ObjectId` is BLAKE3 of the canonical CBOR of that stored object. A leaf is unchanged: `raw` is stored, and `normalized` is BLAKE3 of the stripped token text. Projection of an internal node is `concat(children.raw)`. A whitespace-only edit changes a leaf's `raw` and therefore every ancestor's `ObjectId`, and it changes no `normalized` id.

## Decision

Option 3. Only leaves hash source bytes. Internal nodes are Merkle nodes over their children, still canonical CBOR via `cbor2`.

## Consequences

Leaf `ObjectId`s stay. Internal `ObjectId`s and internal `normalized` ids change, including the root of every parsed file. Identical child sequences still share one stored node. `Store::put` keeps hashing the stored bytes; an internal node simply has no `raw` field to hash. In-memory trees may cache the concatenation, but that cache is not stored and is not an input to either hash.

We will not put `raw` or stripped text back into an internal node's hash without a new ADR. We will not hand-roll a non-CBOR preimage to make this faster.
