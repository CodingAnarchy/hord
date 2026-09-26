# The git bridge (`hord git sync`)

The git bridge keeps a git remote, normally the GitHub repository, as a mirror of a hord log. People who use git tooling contribute through pull requests, and every change still goes through the lander. This page covers setup. The decisions behind it are [ADR 0036](adr/0036-git-bridge-sync-with-github.md) and [ADR 0037](adr/0037-bridge-scope.md), and the contract is spec §9.

## What it does

- **`main` belongs to the bridge.**
  - The bridge pushes each landed change to `main` as one commit, in log order.
  - Each commit has the `Hord-Change`, `Hord-Intent` and `Hord-Actor` trailers. Its tree is byte-identical to the change's projection.
  - Pushes are fast-forward only.
- **A pull request is one proposal.**
  - When a pull request is opened or pushed to, the bridge imports its head as one Tier 0 change. The change is based on the commit of `main` that the pull request branched from.
  - The intent is the pull request's title and body, and the actor is the head commit's git author.
  - The bridge submits the change to the lander and reports the outcome back on the pull request:
    - a `hord/lander` commit status (pending, success, or failure);
    - a comment with the conflict summary when the change parks, or the reason when it is rejected.
  - Once the change is on `main`, the bridge closes the pull request with a comment naming the landed change. GitHub shows the pull request as closed rather than merged: `main` carries the change as one commit.
  - A new push to the pull request supersedes its earlier proposal. If the lander is still working on the earlier proposal, the bridge waits for it to settle first. A superseded proposal that parked stays in the queue until someone resolves or drops it (`hord arbitrate`).
  - A pull request that does not branch from a commit of `main` gets an error status and a comment asking for a rebase.
- **Divergence** means `main` names a commit that the export of the log does not reach, for example after a manual push.
  - The bridge checks after every push it makes, every hour, and on demand with `--check`.
  - It records every check, passing or not, as a `BridgeChecked` event in the repository's event log. `hord watch` and `hord audit` read them there.
  - The bridge never repairs `main` on its own. If `main` only lags the log (it names an earlier export), that is not divergence: the next push catches up.

## Modes

```
hord git sync            # run until interrupted (Ctrl-C)
hord git sync --once     # one pass: export and push, submit pull requests, report outcomes
hord git sync --check    # compare main with the export; exit 1 if it has diverged; change nothing
hord git sync --repair   # FORCE-PUSH the export of the log to main
```

`--repair` discards every commit on `main` that the export does not contain. Run `--check` first, and save anything worth keeping from the diverged commits as a pull request.

The daemon follows the repository's event stream and exports each landing when it happens. It saves its position in `<work_dir>/state.json`, along with the last change it pushed and each pull request's proposal. After a restart it resumes from there, so it neither skips nor repeats a change, a comment, or a status.

## Setup

1. **Create a token.** Use a GitHub token (a fine-grained personal access token or an app installation token) for the mirror repository, with these permissions:
   - Contents: read and write (to push `main` and fetch `refs/pull/*/head`);
   - Pull requests: read and write (to list, comment on, and close pull requests);
   - Commit statuses: read and write.

   Store the token in a file on the host, outside the repository, readable only by the bridge's user. The bridge refuses a token file inside the repository's working tree, except under `.hord/`.

2. **Protect `main`.** In the GitHub repository's settings, add a branch protection rule (or a ruleset) for `main`:
   - restrict who can push to `main` to the bridge's account or app only;
   - allow force pushes by the bridge only, so that `--repair` works;
   - do not require pull request reviews on `main`: reviews happen in hord.

   Nothing else may write to `main`. A manual push is divergence, and the bridge reports it.

3. **Give the bridge a hord token** when it follows a `hord serve` that requires auth. An admin mints it with the `bridge` scope, which lets it submit a pull request's change on behalf of its git author and record divergence checks (ADR 0037):

   ```
   hord token mint --agent git-bridge --model none --harness hord-git-sync \
       --scope read --scope bridge --key-out /etc/hord/bridge.pem
   hord login origin --token <token> --key-file /etc/hord/bridge.pem
   ```

   The bridge records the key id of that login as the voucher of every pull request's change. The id appears on the change's provenance and on its `Submitted` event. A `bridge` token cannot submit as an agent, without a git commit ref, or signed, and it cannot review or arbitrate. Against a local repository, or a server without auth, no token is needed.

4. **Write the config.** The default location is `.hord/bridge.toml`; another path can be passed with `--config`:

   ```toml
   # The mirror: a git URL, with no credentials in it.
   remote = "https://github.com/owner/hord.git"
   # The GitHub token (step 1). Used for git over HTTPS and the REST API.
   token_file = "/etc/hord/github-token"
   # Pull request polling, in seconds (default 60).
   poll_secs = 60
   # Divergence checks, in seconds (default 3600: hourly).
   check_secs = 3600
   # The bridge's work directory: its bare export repository and state.
   # Default: .hord/bridge in the repository.
   work_dir = "/var/lib/hord/bridge"
   # The hord repository to follow: a remote name or http(s) address.
   # Default: the repository in the current directory, through its daemon.
   follow = "origin"

   # Pull requests. Without this table the bridge only exports and checks.
   [github]
   repository = "owner/hord"
   api = "https://api.github.com"   # or a GitHub Enterprise API root
   ```

5. **First push.** If the GitHub repository already has history, import it first (`hord init --from-git`). Exported commits carry trailers, so they differ from the original commits. The first `hord git sync --check` therefore reports divergence. Run `hord git sync --repair` once to replace `main` with the export. After that, the bridge only fast-forwards.

6. **Run it** under the host's service manager, for example a systemd unit that runs `hord git sync` in the repository's directory, restarts it on failure, and stops it with SIGINT.

## Tests

`cargo test -p hord-git --test bridge --test bridge_auth` and `cargo test -p hord-cli --test git_sync` run the bridge against a local bare repository as the mirror and a scripted pull request source (`hord_git::sync::ScriptedPulls`). The tests use no network. `crates/hord-git/tests/bridge.rs` also checks spec §9's round trip on hord's own history: `export(import(repo))` reproduces the tree SHA of every commit reachable from `HEAD`.
