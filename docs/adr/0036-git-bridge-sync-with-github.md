# ADR 0036: The git bridge mirrors main and takes pull requests as proposals

- **Status:** accepted
- **Date:** 2026-09-26
- **Spec:** §9 (Sync), §12 M6 ("the git mirror is what GitHub sees", "bridge sync never diverges")
- **Blocks:** M6 bridge (`hord git sync`)

## Problem

§9 says a bridge daemon keeps a git remote as a mirror of the log, and that incoming pushes to the mirror are imported as proposals that go through the lander. It does not say which pushes, or how a mirror that accepts pushes can also "never diverge" from the log. M6 makes GitHub that mirror, with humans on git tooling contributing through it.

## Options

1. **Mirror `main`, pull requests as proposals.**
   - Hord is the source of truth. GitHub's `main` is written only by the bridge: branch protection allows only the bridge's token.
   - Each landed change is exported with its `Hord-*` trailers.
   - A pull request is imported as one proposal and goes through the lander. The outcome is reported back on the pull request.
2. **Any push is a proposal**, including a push to `main`. A rejected push to `main` is force-reset to the log's export, so `main` can briefly show commits that never land.
3. **Mirror `main`, one proposal per commit.** This keeps git's granularity, at the cost of partial landings to reconcile.

## Decision

Option 1.

- **`main` belongs to the bridge.** It is always the export of the log, commit for commit, with `Hord-Change`, `Hord-Intent` and `Hord-Actor` trailers (§9). The bridge pushes each landed change in log order. A branch-protection rule makes the bridge's token the only writer.
- **A pull request is one proposal.**
  - When a pull request is opened or updated, the bridge imports its head as a proposal against the landing head (Tier 0 read and write sets, §9).
  - The intent is the pull request's title and body, and the git author is the actor.
  - The bridge submits the proposal, reports the lander's outcome on the pull request (a commit status, plus a comment with the conflict summary when it parks), and closes the pull request once the change lands on `main`.
  - A new push to the pull request supersedes its earlier proposal.
- **Divergence** means GitHub's `main` is not exactly the export of the log. The bridge checks every hour, and after every push it makes. It reports divergence as an event and does not repair it silently. Repairing is an explicit `hord git sync --repair`, a force-push of the export.
- **The GitHub side is behind a trait.** Tests use a local bare repository as the mirror and a scripted pull request source. The live implementation uses the GitHub REST API with a token from the bridge's config.

## Consequences

- Git users contribute with the pull requests they already use. Nothing reaches `main` without the lander, so M6's "all contributions go through the lander" holds.
- Commits inside a pull request are not kept as separate changes; the landed change is one commit on `main`. Option 3 remains open for a later ADR if per-commit history is needed.
- The bridge needs a GitHub token with contents and pull-request write access. It is kept in the host's config, never in the repository.
