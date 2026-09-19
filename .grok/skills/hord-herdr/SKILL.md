---
name: hord-herdr
description: >
  Fan out Grok or Claude agents inside Herdr to implement Hord in parallel
  crate slices. Use when the user asks to use Herdr, spawn grok or claude
  panes, parallelize hord work, run a team of agents, or /hord-herdr.
  Requires HERDR_ENV=1.
metadata:
  short-description: "Parallel Grok/Claude agents in Herdr for Hord"
argument-hint: "[M0|M1|M2|M3|M4|M5|M6|M7]"
---

# Hord × Herdr

Coordinate coding agents in sibling Herdr panes. You orchestrate; they implement. Follow the `hord` skill for spec discipline. Supported pane kinds: **Grok** (`--kind grok`) and **Claude** (`--kind claude`).

## Gate

```bash
test "${HERDR_ENV:-}" = 1
```

If this fails, do not inspect or control Herdr. Tell the user to open a Herdr workspace with cwd set to this repo, start Grok or Claude in a pane, and rerun `/hord-herdr`.

Then learn the live CLI (`herdr --help`, `herdr agent`, `herdr pane`). Do not run bare `herdr`.

## Repo cwd

Resolve the hord root (directory containing `docs/spec.md`). Every split pane must use `--cwd` of that root so each session loads project skills (Grok: `.grok/skills/`; Claude: `.claude/skills/`, a symlink to the same files).

If the calling pane is not in that root, split with `--cwd <hord-root>` anyway. Do not create a new workspace unless the user asked.

## Slice the milestone

1. Follow the `hord` skill to pick the milestone (argument or inferred).
2. Read `.grok/skills/hord/references/sections.md` and spec §12 for that milestone.
3. Split work on **crate boundaries** that do not share files. Typical M0 slices:
   - `hord-encoding` (ObjectId, canonical CBOR, golden vectors)
   - `hord-core` types (depends on encoding)
   - `hord-store` (depends on encoding + core)
   - `hord-git` import/export (depends on store)
   - `hord-cli` `init` / `ws` / `status` / `log` / `git` (depends on the rest)
4. Launch independent slices first. Dependent slices wait until their upstream agent is `idle`/`done` and the files exist.
5. Cap live agents at 3 unless the user asked for more. Prefer one agent per crate, not per function.

OPEN decisions stay on **this** orchestrating session via `/hord-adr`. Do not ask two panes (Grok or Claude) to pick different answers to the same OPEN item.

## Kind

The orchestrator chooses `--kind` per pane. Mix Grok and Claude across independent crates when useful.

- **Default: `grok`.**
- **Use `claude`** when the user asked for Claude, `grok` is not ready (`agent_not_ready` / missing binary), or a second independent crate benefits from a different agent.
- If `herdr agent start` fails for one kind, retry the other on that pane before giving up.
- Do not assign kinds to “sides” of an OPEN decision.

## Launch

Default to a sibling pane in the current tab. Keep focus on the caller (`--no-focus`). Split a wide pane right, a tall pane down.

```bash
herdr pane layout --pane "$HERDR_PANE_ID"
herdr pane split --current --direction right --cwd "<hord-root>" --no-focus
```

The new pane must be an idle shell. Start the chosen kind:

```bash
herdr agent start <name> --kind grok --pane <pane-id>
herdr agent start <name> --kind claude --pane <pane-id>
```

Names: `[a-z][a-z0-9_-]{0,31}`, unique, crate-derived (`encoding`, `core`, `store`, `git-bridge`, `cli`).

If `agent start` returns `agent_not_ready`, wait until idle before prompting. Do not resubmit blindly.

## Prompt

Each agent gets a self-contained prompt. They do not share this conversation.

```text
You are implementing one Hord crate slice in this repo.

Follow the project skill /hord and AGENTS.md (same files for Grok and Claude). Spec is docs/spec.md.
Milestone: <M?>
Your slice: <crate>
In scope: <files / types / acceptance>
Out of scope: everything else. Do not create later-milestone crates.

Read spec sections: <list from references/sections.md>
If you hit an OPEN decision, stop and write the question to docs/adr/_open-<name>.md; do not pick it.

When done: cargo fmt, clippy -D warnings, and the tests for this slice.
Reply with files changed and how to run the tests.
```

Submit with:

```bash
herdr agent prompt <name> "<prompt>" --wait --timeout 600000
```

`--wait` is enough. Do not add `--until` for normal work. If the result is `blocked`, `agent_prompt_stalled`, or `timeout`, run `herdr agent get` and `herdr agent read <name> --source recent-unwrapped --lines 120` before sending anything else.

## Collect

- Independent slices may run at the same time.
- After each agent settles, `herdr agent read` the recap. If `docs/adr/_open-<name>.md` exists, handle it here with `/hord-adr`, delete that scratch file, and only then prompt them to continue.
- Do not close panes you created unless the user asked.
- Do not `herdr server stop`.

When all slices for the milestone are in, run the milestone acceptance from spec §12 on this pane (or a dedicated test pane via `herdr pane run`). Report what passed and what is still red.
