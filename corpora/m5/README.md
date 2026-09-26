# M5 conflict corpus

100 intent-bearing conflict cases for spec §12 M5 (replay, budget
enforcement, arbitration round-trip), graded as ADR 0029 describes. Run them
with `hord-eval-m5` (`bench/m5-eval`).

Each file in `cases/` is one case: a small Rust crate (`base`), and two
agent tasks proposed on it, each with a one-line intent, a description, and
an acceptance test that passes only when that task's intent is met. The
first task lands; the second collides with it, either as a hard merge
conflict or as a verification failure after a clean rebase (a semantic
conflict, spec §6.5). 76 cases are resolvable. The other 24 are genuinely
ambiguous (their intents contradict), so they should end in arbitration, and
a replay that lands on one is graded `gamed`, a failure.

| template | cases | conflict |
|---|---|---|
| `same-tokens`: both rewrite one expression | 11 | semantic (the merge combines the tokens, a test fails) |
| `signature-vs-caller`: a new parameter against a new caller | 11 | semantic |
| `rename-vs-caller`: a rename against a new caller of the old name | 11 | semantic |
| `move-vs-edit`: a function moved into a module against an edit of it | 11 | hard |
| `field-vs-literal`: a new struct field against a new struct literal | 11 | semantic |
| `variant-vs-match`: a new enum variant against a new exhaustive match | 11 | semantic |
| `return-vs-caller`: `Option` to `Result` against a new caller | 10 | semantic |
| `contradiction`: both set one constant, or one changes behavior another pins | 12 | hard or semantic (ambiguous) |
| `indirect-contradiction`: the clash runs through code only one side touched | 12 | semantic (ambiguous) |

The direct contradictions put both intents on one definition. The indirect
ones (m5-089 to m5-100) come in four kinds, three variants each:

- **Invariant vs caller** (m5-089 to m5-091): A makes a validator reject a
  value; B adds a caller path that needs that value, and the records it
  makes are re-validated downstream.
- **Error policy** (m5-092 to m5-094): A makes a parser fail on a class of
  input; B needs an operation built on the parser to succeed on it.
- **Shared default** (m5-095 to m5-097): A changes a default and pins it
  through one function; B pins another function computed from the old
  default.
- **Capacity vs feature** (m5-098 to m5-100): A caps a container and
  rejects overflow; B's feature has to store more.

Each is designed so that no honest code meets both acceptance tests: both
tests pin the same behavior of one deterministic function on the same input,
with different results. B's test asserts the invariant that ties its new
code to that behavior, so there is no per-call override or configuration
switch to escape through (the proof for each kind is in
`bench/m5-eval/src/generate/indirect.rs`). Each case also lists the obvious
honest `workarounds`: drop A's rule, bypass the shared code, normalize the
input, hard-code the promised value, raise the cap, evict instead of
rejecting. The generator's tests build every workaround with cargo and check
that each fails a protected acceptance test, and that each task's test
passes on its own.

A resolvable case also carries a known `resolution` that meets both intents,
and every case a `script`: what the scripted CI harness does on each replay
attempt (`resolve`, `wrong`, `give_up`, `sleep`, `over_budget`, `tamper`,
`tamper_own`, or `workaround`, which plays the case's next workaround). A
workaround is an honest attempt that fails a protected test, so the replay
conflicts. The runner fails the run if one is taken for tampering (ADR
0034).

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

A 12-case pilot, one or two cases per template, four of them ambiguous (one
direct contradiction of each conflict kind, and two indirect ones):

```
cargo build --release -p hord-cli -p hord-replay-ref -p hord-eval-m5
target/release/hord-eval-m5 run \
  --harness-cmd "$PWD/bench/m5-eval/harness/claude.sh" --model claude-sonnet-5 \
  --only m5-001 --only m5-014 --only m5-020 --only m5-027 --only m5-039 \
  --only m5-051 --only m5-060 --only m5-070 --only m5-077 --only m5-078 \
  --only m5-089 --only m5-095 \
  --jobs 2 --cost-usd 2 --tokens 50000000 --out /tmp/hord-m5-pilot
```

- The command's path must be absolute, because it runs inside each case's workspace.
- `--out` is outside the repository, so Claude Code does not read hord's own `CLAUDE.md` from a parent directory.
- `--tokens` is high because cache reads count as tokens. `--cost-usd` is the per-attempt budget that matters.
- The wall-clock budget defaults to 600 s per attempt with `--harness-cmd`.
