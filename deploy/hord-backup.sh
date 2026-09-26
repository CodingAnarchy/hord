#!/bin/sh
# Back up a hord team host (docs/hosting.md, "Back up and restore").
#
#   hord-backup.sh <repo> <dest-dir>
#
# Stops hord serve for the few seconds a copy takes: the index and the event
# log are redb databases, which are only consistent when no process holds
# them. Objects are content-addressed and never rewritten. Writes
# <dest-dir>/hord-<UTC time>.tar.gz with the repository's .hord/ and
# /etc/hord (auth file, TLS files).
set -eu
repo=${1:?usage: hord-backup.sh <repo> <dest-dir>}
dest=${2:?usage: hord-backup.sh <repo> <dest-dir>}
stamp=$(date -u +%Y%m%dT%H%M%SZ)
out="$dest/hord-$stamp.tar.gz"
mkdir -p "$dest"
systemctl stop hord
trap 'systemctl start hord' EXIT
# Workspaces (.hord/ws) are scratch checkouts, rebuilt on demand.
tar -czf "$out" --exclude=.hord/ws -C "$repo" .hord -C / etc/hord
echo "$out"
