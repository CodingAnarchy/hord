# M5 conflict corpus

100 intent-bearing conflict cases for spec §12 M5 (replay, budget
enforcement, arbitration round-trip), graded as ADR 0029 describes. Run them
with `hord-eval-m5` (`bench/m5-eval`).

Each file in `cases/` is one case: a small Rust crate (`base`), and two
agent tasks proposed on it, each with a one-line intent, a description, and
an acceptance test that passes only when that task's intent is met. The
first task lands; the second collides with it, either as a hard merge
conflict or as a verification failure after a clean rebase (a semantic
conflict, spec §6.5). Twelve cases are genuinely ambiguous (their intents
contradict), so they should end in arbitration.

| template | cases | conflict |
|---|---|---|
| `same-tokens`: both rewrite one expression | 13 | semantic (the merge combines the tokens, a test fails) |
| `signature-vs-caller`: a new parameter against a new caller | 13 | semantic |
| `rename-vs-caller`: a rename against a new caller of the old name | 12 | semantic |
| `move-vs-edit`: a function moved into a module against an edit of it | 12 | hard |
| `field-vs-literal`: a new struct field against a new struct literal | 13 | semantic |
| `variant-vs-match`: a new enum variant against a new exhaustive match | 12 | semantic |
| `return-vs-caller`: `Option` to `Result` against a new caller | 13 | semantic |
| `contradiction`: both set one constant, or one changes behavior another pins | 12 | hard or semantic (ambiguous) |

A resolvable case also carries a known `resolution` that meets both intents,
and every case a `script`: what the scripted CI harness does on each replay
attempt (`resolve`, `wrong`, `give_up`, `sleep`, `over_budget`).

## How the cases were made

`hord-eval-m5 generate` (`bench/m5-eval/src/generate.rs`) writes every file
from the templates above; each case's `made_by` names the generator version,
template, and variant. The files are the generator's output, checked in so
the corpus is reviewable data; a test in `hord-eval-m5` fails if they drift.
To change a case, change the generator and regenerate:

```
cargo run -p hord-eval-m5 -- generate --out corpora/m5/cases
```

## Running

```
cargo build --release -p hord-cli -p hord-replay-ref -p hord-eval-m5
target/release/hord-eval-m5 run                        # scripted harness (CI)
target/release/hord-eval-m5 run --harness-cmd 'claude -p "$(cat "$HORD_REPLAY_PROMPT_FILE")"' --model claude
```

The runner needs git and cargo with cargo-llvm-cov (the lander's verifier
runs each case's tests). It writes `report.json`, and `review.md` and
`review.csv` for the human "sufficient to resolve" rating of every parked
case's conflict summary.
