# ADR 0004: lasso for NodeKind / LangId

- **Status:** accepted
- **Date:** 2026-09-19
- **Spec:** §3.3 (interned CST nodes)
- **Blocks:** none

## Problem

Every CST node allocated a `Box<str>` for `NodeKind` and `LangId`. A mutexed `HashMap` intern table would serialize the parse path.

## Decision

Use `lasso` (`ThreadedRodeo`) to intern kind and language strings. Wire form is still CBOR text. New dep: concurrent intern pool, no hand-rolled map.
