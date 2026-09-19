# Hord (Claude)

This repository is agent-agnostic. **Follow `AGENTS.md`.** The spec is `docs/spec.md`.

Project skills live in `.grok/skills/` (Grok) and are the same files as `.claude/skills/` (this symlink). Load `/hord`, `/hord-adr`, and `/hord-herdr` from there.

Inside Herdr, the orchestrating agent chooses `--kind grok` or `--kind claude` per pane (`/hord-herdr`).
