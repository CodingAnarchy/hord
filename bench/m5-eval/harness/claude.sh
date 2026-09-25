#!/bin/sh
# A model command for hord-replay-ref (--harness-cmd of hord-eval-m5): runs
# the Claude Code CLI non-interactively in the replay workspace (the current
# directory) on the prompt it reads from stdin, then reports the attempt's
# usage to $HORD_REPLAY_USAGE for the lander's budget check (ADR 0028).
#
# Environment:
#   HORD_M5_MODEL        model (default claude-sonnet-5)
#   HORD_M5_MAX_TURNS    agentic turn cap, passed as --max-turns when set
#   HORD_REPLAY_COST_USD the attempt's cost budget (set by hord-replay-ref
#                        from the replay request); passed as --max-budget-usd
#   HORD_M5_CLAUDE       the claude binary (default: claude on PATH)
#
# Tools: file reading and editing, and Bash only for cargo check, cargo
# test, and cargo build. --restricted confines the file tools to the working
# directory and ignores user and project settings; --permission-mode dontAsk
# denies anything not allowed here instead of prompting.
#
# `claude.sh --extract-usage < output.json` prints the usage JSON for a
# saved `claude -p --output-format json` result (used by the tests).
set -eu

model="${HORD_M5_MODEL:-claude-sonnet-5}"

fail() {
    echo "claude.sh: $*" >&2
    exit 1
}

# One number from the top-level JSON result: `key` as it appears first in
# the text (the snake_case usage fields do not recur in modelUsage, which
# is camelCase). Empty when absent. Used only without jq.
field() {
    tr -d '\n' | sed -n "s/.*\"$1\"[[:space:]]*:[[:space:]]*\([0-9.eE+-]*\).*/\1/p" | head -n 1
}

# The usage JSON for a claude -p --output-format json result on stdin.
extract_usage() {
    out=$(cat)
    if command -v jq >/dev/null 2>&1 && [ -z "${HORD_M5_NO_JQ:-}" ]; then
        printf '%s' "$out" | jq -e '
            if (.usage | type) != "object" or (.total_cost_usd | type) != "number"
            then error("the result has no usage or total_cost_usd") else . end
            | {
                tokens: ((.usage.input_tokens // 0) + (.usage.output_tokens // 0)
                    + (.usage.cache_creation_input_tokens // 0)
                    + (.usage.cache_read_input_tokens // 0)),
                cost_usd: .total_cost_usd,
                model: ((.modelUsage // {} | keys | first) // $model)
              }' --arg model "$model" -c ||
            fail "the claude result lacks usage: $out"
        return
    fi
    input=$(printf '%s' "$out" | field input_tokens)
    output=$(printf '%s' "$out" | field output_tokens)
    cost=$(printf '%s' "$out" | field total_cost_usd)
    [ -n "$input" ] && [ -n "$output" ] && [ -n "$cost" ] ||
        fail "the claude result lacks usage: $out"
    created=$(printf '%s' "$out" | field cache_creation_input_tokens)
    read=$(printf '%s' "$out" | field cache_read_input_tokens)
    tokens=$((input + output + ${created:-0} + ${read:-0}))
    printf '{"tokens":%s,"cost_usd":%s,"model":"%s"}\n' "$tokens" "$cost" "$model"
}

if [ "${1:-}" = "--extract-usage" ]; then
    extract_usage
    exit 0
fi

[ -n "${HORD_REPLAY_USAGE:-}" ] || fail "HORD_REPLAY_USAGE is not set: run under hord-replay-ref"

set -- -p \
    --model "$model" \
    --output-format json \
    --restricted \
    --no-session-persistence \
    --permission-mode dontAsk \
    --permission-prompts none \
    --tools "Read,Edit,Write,Glob,Grep,Bash" \
    --allowedTools "Read" "Edit" "Write" "Glob" "Grep" \
    "Bash(cargo check *)" "Bash(cargo test *)" "Bash(cargo build *)"
if [ -n "${HORD_M5_MAX_TURNS:-}" ]; then
    set -- "$@" --max-turns "$HORD_M5_MAX_TURNS"
fi
if [ -n "${HORD_REPLAY_COST_USD:-}" ]; then
    set -- "$@" --max-budget-usd "$HORD_REPLAY_COST_USD"
fi

result=$("${HORD_M5_CLAUDE:-claude}" "$@") || {
    status=$?
    # Report what was spent, if the CLI said, before failing the attempt.
    printf '%s' "$result" | extract_usage >"$HORD_REPLAY_USAGE" 2>/dev/null || true
    fail "claude exited with status $status: $result"
}
printf '%s' "$result" | extract_usage >"$HORD_REPLAY_USAGE"
if printf '%s' "$result" | grep -q '"is_error"[[:space:]]*:[[:space:]]*true'; then
    fail "claude reported an error: $result"
fi
