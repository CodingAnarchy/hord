#!/bin/sh
# Run hord's own repository hord-first on this machine (ADR 0040): a local
# `hord serve` over TLS with auth, kept running by a launchd user agent, and
# client directories for people and agents.
#
#   deploy/local/hord-local.sh setup          build, import, TLS, auth, launchd
#   deploy/local/hord-local.sh start|stop|restart|status|logs
#   deploy/local/hord-local.sh agent NAME [MODEL]   mint a token, make a client
#   deploy/local/hord-local.sh uninstall      remove the launchd agent (keeps data)
#
# State lives in $HORD_LOCAL (default ~/hord-server):
#   bin/hord        the server's own copy of the binary (rebuilt by `setup`)
#   src/            a git clone of hord's main, the import source
#   repo/           the hosted hord repository (.hord/ holds the store)
#   etc/            auth.toml, TLS files, the admin's password (owner-only)
#   clients/NAME/   a client directory per person or agent (its own HORD_HOME)
#   logs/           the server's log
set -eu

BASE=${HORD_LOCAL:-"$HOME/hord-server"}
HORD="$BASE/bin/hord"
ADDR=${HORD_LOCAL_ADDR:-127.0.0.1:7878}
URL="https://$ADDR"
LABEL=dev.hord.local
PLIST="$HOME/Library/LaunchAgents/$LABEL.plist"
ADMIN=${HORD_LOCAL_ADMIN:-$(id -un)}
ROOT=$(cd "$(dirname "$0")/../.." && pwd)

say() { printf 'hord-local: %s\n' "$*"; }
die() { printf 'hord-local: %s\n' "$*" >&2; exit 1; }

build() {
    say "building hord (release) from $ROOT"
    (cd "$ROOT" && cargo build --release -q -p hord-cli)
    install -d "$BASE/bin"
    install -m 0755 "$ROOT/target/release/hord" "$HORD"
}

import() {
    if [ ! -d "$BASE/src/.git" ]; then
        origin=$(git -C "$ROOT" remote get-url origin)
        say "cloning $origin (main) into $BASE/src"
        git clone -q --branch main "$origin" "$BASE/src"
    fi
    if [ ! -d "$BASE/repo/.hord" ]; then
        say "importing hord's git history into $BASE/repo"
        install -d "$BASE/repo"
        (cd "$BASE/repo" && HORD_HOME="$BASE/home" HORD_NO_DAEMON=1 "$HORD" init --from-git "$BASE/src")
    fi
}

tls() {
    t="$BASE/etc/tls"
    [ -f "$t/cert.pem" ] && return
    say "creating a local CA and a certificate for localhost"
    install -d -m 0700 "$t"
    openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes \
        -keyout "$t/ca.key" -out "$t/ca.pem" -days 3650 -subj "/CN=hord local CA" 2>/dev/null
    openssl req -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes \
        -keyout "$t/key.pem" -out "$t/host.csr" -subj "/CN=localhost" 2>/dev/null
    printf 'subjectAltName=DNS:localhost,IP:127.0.0.1\nextendedKeyUsage=serverAuth\n' > "$t/ext.cnf"
    openssl x509 -req -in "$t/host.csr" -CA "$t/ca.pem" -CAkey "$t/ca.key" -CAcreateserial \
        -days 825 -out "$t/cert.pem" -extfile "$t/ext.cnf" 2>/dev/null
    chmod 0600 "$t/key.pem" "$t/ca.key"
    rm -f "$t/host.csr" "$t/ext.cnf"
}

auth() {
    e="$BASE/etc"
    [ -f "$e/auth.toml" ] && return
    say "creating the auth file with admin user $ADMIN"
    install -d -m 0700 "$e"
    umask 077
    openssl rand -base64 24 > "$e/$ADMIN.password"
    HORD_NO_DAEMON=1 "$HORD" user add "$ADMIN" --auth-file "$e/auth.toml" --password-stdin \
        --scope read --scope propose --scope review:human --scope arbitrate --scope admin \
        < "$e/$ADMIN.password"
}

config() {
    cat > "$BASE/repo/.hord/server.toml" <<EOF
# Written by deploy/local/hord-local.sh. Loopback only, TLS, tokens required.
bind = "$ADDR"

[tls]
cert = "$BASE/etc/tls/cert.pem"
key = "$BASE/etc/tls/key.pem"

[auth]
file = "$BASE/etc/auth.toml"
EOF
}

plist() {
    install -d "$BASE/logs" "$HOME/Library/LaunchAgents"
    # The lander runs cargo to verify changes, so the agent needs the
    # toolchain on its PATH.
    cat > "$PLIST" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>$LABEL</string>
  <key>ProgramArguments</key>
  <array>
    <string>$HORD</string><string>serve</string>
    <string>--repo</string><string>$BASE/repo</string>
  </array>
  <key>WorkingDirectory</key><string>$BASE/repo</string>
  <key>EnvironmentVariables</key>
  <dict>
    <key>HORD_HOME</key><string>$BASE/home</string>
    <key>PATH</key><string>$HOME/.cargo/bin:/usr/local/bin:/opt/homebrew/bin:/usr/bin:/bin</string>
  </dict>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>StandardOutPath</key><string>$BASE/logs/hord.log</string>
  <key>StandardErrorPath</key><string>$BASE/logs/hord.log</string>
</dict>
</plist>
EOF
}

client() {
    name=$1
    c="$BASE/clients/$name"
    [ -d "$c/.hord" ] && return
    install -d -m 0700 "$c" "$c/home"
    (
        cd "$c"
        export HORD_HOME="$c/home" HORD_NO_DAEMON=1
        "$HORD" init >/dev/null
        "$HORD" remote add origin "$URL" --ca-file "$BASE/etc/tls/ca.pem" >/dev/null
        "$HORD" remote set-default origin >/dev/null
    )
}

start() {
    launchctl bootstrap "gui/$(id -u)" "$PLIST" 2>/dev/null || launchctl kickstart -k "gui/$(id -u)/$LABEL"
    i=0
    until curl -s --cacert "$BASE/etc/tls/ca.pem" -o /dev/null "$URL/" || [ $i -ge 30 ]; do
        sleep 1; i=$((i + 1))
    done
    status
}

stop() { launchctl bootout "gui/$(id -u)/$LABEL" 2>/dev/null || true; say "stopped"; }

status() {
    if launchctl print "gui/$(id -u)/$LABEL" >/dev/null 2>&1; then
        say "running as launchd agent $LABEL; $URL"
    else
        say "not running"
    fi
    tail -n 3 "$BASE/logs/hord.log" 2>/dev/null || true
}

case "${1:-}" in
setup)
    build; import; tls; auth; config; plist
    client "$ADMIN"
    start
    (cd "$BASE/clients/$ADMIN" && HORD_HOME="$BASE/clients/$ADMIN/home" HORD_NO_DAEMON=1 \
        "$HORD" login origin --user "$ADMIN" --password-stdin < "$BASE/etc/$ADMIN.password" >/dev/null)
    say "ready: web UI $URL (trust $BASE/etc/tls/ca.pem), admin client $BASE/clients/$ADMIN"
    say "admin password: $BASE/etc/$ADMIN.password"
    ;;
agent)
    name=${2:?usage: hord-local.sh agent NAME [MODEL]}
    model=${3:-claude-opus-5-5}
    client "$name"
    a="$BASE/clients/$ADMIN"
    token=$(cd "$a" && HORD_HOME="$a/home" HORD_NO_DAEMON=1 "$HORD" token mint --agent "$name" \
        --model "$model" --harness claude-code --scope read --scope propose \
        --key-out "$BASE/clients/$name/home/agent.pem" | sed -n 's/^token //p')
    [ -n "$token" ] || die "token mint printed no token"
    (cd "$BASE/clients/$name" && HORD_HOME="$BASE/clients/$name/home" HORD_NO_DAEMON=1 \
        "$HORD" login origin --token "$token" --key-file "$BASE/clients/$name/home/agent.pem" >/dev/null)
    say "agent $name ready: cd $BASE/clients/$name && export HORD_HOME=$BASE/clients/$name/home"
    ;;
start) start ;;
stop) stop ;;
restart) stop; start ;;
status) status ;;
logs) tail -f "$BASE/logs/hord.log" ;;
uninstall) stop; rm -f "$PLIST"; say "launchd agent removed; data kept in $BASE" ;;
*) sed -n '2,20p' "$0"; exit 2 ;;
esac
