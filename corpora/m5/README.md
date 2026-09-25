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
case's conflict summary. Each case runs under its own `hord serve`, and
each parked case is resolved from its web workbench: the runner posts the
workbench's "pick ours" form, the UI signs the decision with a key the
runner provides, and the runner checks that the resolution lands with both
parents and a signed `Arbitrated` event.

## A real-model pilot

`bench/m5-eval/harness/claude.sh` is a model command for `hord-replay-ref`.
It runs the Claude Code CLI (`claude -p`) in the replay workspace on the
prompt from stdin, with only file reading and editing and `cargo
check/test/build` allowed (`--restricted`, `--permission-mode dontAsk`, no
permission bypass). It caps spend at the attempt's cost budget
(`--max-budget-usd`), and writes the tokens (input, output, and cache) and
`total_cost_usd` it reports to `$HORD_REPLAY_USAGE` for the lander's budget
check. `HORD_M5_MODEL` picks the model (default `claude-sonnet-5`).
`HORD_M5_MAX_TURNS` adds `--max-turns`. The CLI's help does not list that
flag, so it is off unless set.

A 10-case pilot, one or two cases per template, two of them ambiguous:

```
cargo build --release -p hord-cli -p hord-replay-ref -p hord-eval-m5
target/release/hord-eval-m5 run \
  --harness-cmd "$PWD/bench/m5-eval/harness/claude.sh" --model claude-sonnet-5 \
  --only m5-001 --only m5-014 --only m5-020 --only m5-027 --only m5-039 \
  --only m5-051 --only m5-064 --only m5-076 --only m5-089 --only m5-090 \
  --jobs 2 --cost-usd 2 --tokens 50000000 --out /tmp/hord-m5-pilot
```

- The command's path must be absolute, because it runs inside each case's workspace.
- `--out` is outside the repository, so Claude Code does not read hord's own `CLAUDE.md` from a parent directory.
- `--tokens` is high because cache reads count as tokens. `--cost-usd` is the per-attempt budget that matters.
- The wall-clock budget defaults to 600 s per attempt with `--harness-cmd`.
