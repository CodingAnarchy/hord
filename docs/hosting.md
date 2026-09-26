# Hosting hord's repository on a team host

This runbook sets up M6's team host (spec §12 M6): hord's own repository, served by `hord serve` over TLS, with every contribution going through the lander. The files it installs are in [`deploy/`](../deploy/).

- `deploy/server.toml`: bind address, TLS, and auth file.
- `deploy/hord.service`: a systemd unit.
- `deploy/hord-backup.sh`: a cold backup of the store and its secrets.

The git mirror on GitHub is kept by the bridge (ADR 0036). Its setup is in `docs/bridge.md`.

## What runs

One `hord serve --repo` process hosts the repository:

- the lander, which is the only writer of the log;
- gRPC (the CLI, agents, and the bridge), gRPC-Web, and the web UI, all on one TLS port (7878 in the samples);
- the repository's local endpoint (a Unix socket), which only the `hord` OS user can reach and which takes no tokens.

State lives in the repository's `.hord/`, plus `/etc/hord` for the auth file and TLS files.

## 1. Prepare the host

- A Linux host with a DNS name, such as `hord.example.org`, and port 7878 reachable by the team.
- A `hord` user whose home is `/srv/hord`:

  ```sh
  useradd --system --home-dir /srv/hord --create-home hord
  install -d -o hord -g hord /srv/hord/home
  install -d -m 0750 -o root -g hord /etc/hord /etc/hord/tls
  ```

- The `hord` binary, built from this repository:

  ```sh
  cargo build --release -p hord-cli
  install -m 0755 target/release/hord /usr/local/bin/hord
  ```

- A Rust toolchain that the `hord` user owns. The lander runs `cargo` to verify changes (spec §6.5):

  ```sh
  sudo -u hord env CARGO_HOME=/srv/hord/cargo RUSTUP_HOME=/srv/hord/rustup \
    sh -c 'curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal'
  ```

## 2. Import hord's repository

Import the git history once. From then on the git repository on GitHub is a mirror, never a source.

```sh
sudo -u hord git clone https://github.com/CodingAnarchy/hord /srv/hord/src
sudo -u hord mkdir /srv/hord/hord
cd /srv/hord/hord
sudo -u hord env HORD_HOME=/srv/hord/home hord init --from-git /srv/hord/src
```

`hord init --from-git` writes the imported history straight into the log, so none of it came through the lander. Start any `hord audit` window after the import. A window that includes it reports each imported change as a store edit.

## 3. TLS

`hord serve` terminates TLS itself, with rustls (ADR 0032). With `[tls]` set, it may bind any address; `--insecure-bind` is only for plaintext. Choose one of these certificates:

- **A public CA** (Let's Encrypt or similar). Point `[tls] cert` at the full chain and `[tls] key` at the key. Clients trust it through the system's roots and need no flags. Reload after renewal: `systemctl restart hord`.

- **The team's own CA.** Create it once and keep `ca.key` offline:

  ```sh
  openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes \
    -keyout ca.key -out ca.pem -days 3650 -subj "/CN=hord team CA"
  openssl req -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes \
    -keyout /etc/hord/tls/key.pem -out host.csr -subj "/CN=hord.example.org"
  openssl x509 -req -in host.csr -CA ca.pem -CAkey ca.key -CAcreateserial \
    -days 825 -out /etc/hord/tls/cert.pem \
    -extfile <(printf "subjectAltName=DNS:hord.example.org\nextendedKeyUsage=serverAuth")
  chgrp hord /etc/hord/tls/key.pem && chmod 0640 /etc/hord/tls/key.pem
  ```

  Give `ca.pem` to every client. Each client passes it once with `hord remote add … --ca-file ca.pem`, or sets `HORD_CA_FILE=/path/to/ca.pem` for every `https` remote. The client trusts the system's roots plus that CA.

- **A terminating proxy instead.** If TLS must end at an existing proxy, leave `[tls]` out and run `hord serve --bind 127.0.0.1:7878` (loopback, so no `--insecure-bind`).
  - The proxy must speak HTTP/2 to hord, because gRPC needs it. `hord serve` accepts cleartext HTTP/2 (h2c).
  - With Caddy:

    ```
    hord.example.org {
        reverse_proxy h2c://127.0.0.1:7878
    }
    ```

  - With nginx, use `grpc_pass grpc://127.0.0.1:7878;` for the `hord.v1.` paths and `proxy_pass` for the web UI.
  - Bearer tokens then travel in the clear between the proxy and hord, so keep them on the same host.

## 4. Configure and start

```sh
install -m 0640 -o hord -g hord deploy/server.toml /srv/hord/hord/.hord/server.toml
install -m 0644 deploy/hord.service /etc/systemd/system/hord.service
systemctl daemon-reload
systemctl enable --now hord
journalctl -u hord    # first line: "hord serve: https://0.0.0.0:7878 (hord, TLS from …, tokens from /etc/hord/auth.toml)"
```

Edit `server.toml` for your certificate paths. A test parses the sample, so its shape stays current. The web UI is at `https://hord.example.org:7878/`.

## 5. Users and agent tokens

The auth file holds users (Argon2 password hashes), minted tokens (BLAKE3 hashes only), and the public keys bound to each actor (spec §10.5.4). Create it on the host as root:

```sh
# People: pick the scopes each person needs.
hord user add matt --auth-file /etc/hord/auth.toml \
  --scope read --scope propose --scope review:human --scope arbitrate --scope admin
hord user add ada --auth-file /etc/hord/auth.toml --scope read --scope propose --scope review:human
chgrp hord /etc/hord/auth.toml && chmod 0640 /etc/hord/auth.toml
```

A person logs in from their own clone. The login binds their key, `~/.hord/keys/<user>.pem`, created on first use, to their account:

```sh
hord login origin --user ada
```

An admin mints agent tokens. Each token is bound to one `Actor::Agent` and to a new signing key, so the server sets an agent's provenance from its token, not from what the agent claims:

```sh
hord token mint --agent coder-1 --model claude-opus-5-5 --harness claude-code \
  --scope read --scope propose --key-out coder-1.pem
hord token mint --agent reviewer-1 --model claude-sonnet-5 --harness claude-code \
  --scope read --scope review:agent-reviewer --key-out reviewer-1.pem
hord token mint --agent bridge --model none --harness hord-git-sync \
  --scope bridge --key-out bridge.pem     # the git bridge (ADR 0037)
```

Hand each agent its token and key file over a private channel. The server keeps neither in the clear. To revoke a token, remove it from the auth file and restart.

## 6. How agents and people contribute

Nobody pushes to git. Every change goes through the lander, so it gets an intent, provenance from the token, and the evidence head's policy requires. Nothing else writes the log. The flow is the same for agents and people:

```sh
hord init                                           # an empty clone: the local object cache
hord remote add origin https://hord.example.org:7878 --ca-file ca.pem   # --ca-file only for a private CA
hord remote set-default origin
hord login origin --token "$HORD_TOKEN" --key-file coder-1.pem         # people: hord login origin --user <name>

hord ws new                                         # a workspace at origin's head; prints its id and directory
# … edit files in the workspace directory …
hord verify -w <ws>                                 # optional: evidence now, before the lander asks
hord propose -w <ws> --intent intent.md             # a signed ChangeRecord; its objects go to the server
hord submit <change>
hord watch --change <change>                        # ConflictCheck → Verifying → Landed | Parked
```

The intent file is Markdown with YAML front matter:

```markdown
---
summary: Serve TLS from server.toml
refs:
  - issue: 42
acceptance:
  - test: login_submit_and_events_work_over_tls
---
Why the change is needed, and what a reviewer should look at.
```

When a change parks:

- For a review, the reviewer runs `hord review <change> --as human --approve -m "…"` or approves it in the web UI. The author then submits again.
- For a conflict, the replay harness tries first. If it gives up, a person with `arbitrate` resolves it in the web UI's workbench or with `hord arbitrate <change> --pick ours|theirs|<change>`.

Both actions are signed and go through the API. `hord audit` checks this.

People who prefer git open pull requests on GitHub. The bridge imports each one as a proposal on its author's behalf and reports the lander's outcome on the pull request (ADR 0036, ADR 0037). `main` on GitHub is written only by the bridge.

## 7. Back up and restore

The store is `.hord/`:

- `objects/`: content-addressed and never rewritten;
- `index.redb` and `events.redb`: the index, queue, and event log;
- `recordings/`, and `server.toml`.

Secrets are in `/etc/hord`. redb files are consistent only when no process holds them, so back up cold. The whole stop, copy, and start takes seconds:

```sh
deploy/hord-backup.sh /srv/hord/hord /var/backups/hord   # stops hord, tars .hord/ (not ws/) and /etc/hord, starts hord
```

Run it daily from a systemd timer or cron, and copy the archives off the host. The archive holds the auth file and the TLS key, so store it as a secret.

To restore:

```sh
systemctl stop hord
mv /srv/hord/hord/.hord /srv/hord/hord/.hord.broken
tar -xzf /var/backups/hord/hord-<time>.tar.gz -C /srv/hord/hord .hord
tar -xzf /var/backups/hord/hord-<time>.tar.gz -C / etc/hord
chown -R hord:hord /srv/hord/hord/.hord
systemctl start hord
hord log --remote origin | tail -1                       # from a clone: head is back
hord audit --remote origin --since <backup date>         # nothing landed outside the lander
```

Changes that landed after the backup are gone from the host, but not from the world:

- clones still hold their objects, so authors can `hord submit` them again;
- the bridge's `--check` reports GitHub's `main` as ahead of the log, and `hord git sync --repair` makes the mirror match the restored log again (ADR 0036).

## 8. Checking M6's acceptance

`hord audit` checks the measurable M6 criteria over a window of the log. It runs from any clone against the host, or on the host itself:

```sh
hord audit --remote origin --since 2026-10-01              # text report; exits 1 on a violation
hord audit --remote origin --since 2026-10-01 --json       # the AuditReport message (hord.proto)
hord audit --remote origin --since 2026-10-01 --require-bridge
```

It checks:

- every landed change has an intent and provenance: a record signed by its author with a key bound to them, vouched for by the bridge, replayed by the harness, or resolved by an arbiter;
- every landed change has passing evidence, and head's policy at its landing base allows it when judged again;
- every review and arbitration is signed by a key bound to the actor who made it;
- nothing is in the log without the lander's `Landed` event;
- the bridge's divergence checks all passed, with none more than 65 minutes apart (`--max-bridge-gap`).

Bridge-vouched changes are counted apart from signed ones. Until the bridge has recorded a check, the report says "no bridge checks recorded". That is a note, and becomes a violation only with `--require-bridge`.

For the thirty-day window, run it nightly with `--since` fixed at the day self-hosting started, and alert on a non-zero exit.
