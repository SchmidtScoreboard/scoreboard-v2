#!/usr/bin/env bash
# Run git against this repo when macOS refuses `access()` on files inside
# ~/Documents.
#
# THE SYMPTOM
#   fatal: unable to access '.git/config': Operation not permitted
#   ...while `cat .git/config` works fine.
#
# THE CAUSE
#   ~/Documents is TCC-protected ("Files and Folders" in Privacy & Security).
#   A process without that grant can still `open()` many files — which is why
#   `cat`, editors, and cargo mostly work — but `access(path, R_OK)` is denied,
#   and that is the exact call git makes before reading a config file. Files the
#   process created itself are exempt, which is why the failure looks random:
#   whatever you just wrote works, everything older does not.
#
# THE REAL FIX
#   Grant the terminal (or whatever runs git) access to Documents:
#   System Settings -> Privacy & Security -> Files and Folders, or Full Disk
#   Access. Do that and this script is unnecessary.
#
# THE WORKAROUND
#   Keep the working tree where it is — in Documents, synced, unchanged — and
#   move only the git DIRECTORY somewhere untouched by TCC for the duration of
#   a command. The tree stays authoritative and iCloud keeps syncing it; git
#   just stops being asked to probe files it is not allowed to probe.
#
#   The git dir is copied out, the command runs, and the result is copied back,
#   so ~/Documents keeps the history. Nothing lives outside Documents between
#   invocations.
#
# USAGE
#   scripts/git-icloud.sh status
#   scripts/git-icloud.sh add -A
#   scripts/git-icloud.sh commit -m "..."
set -euo pipefail
cd "$(dirname "$0")/.."

REPO="$PWD"
WORK="${TMPDIR:-/tmp}/scoreboard-gitdir.$$"

cleanup() { rm -rf "$WORK"; }
trap cleanup EXIT

# Copy the git dir out, run there, copy it back. `cp` uses open(), which is
# permitted; only access() is not.
cp -R "$REPO/.git" "$WORK"
git --git-dir="$WORK" --work-tree="$REPO" "$@"
status=$?

# Only write back when the command could have changed history. A read-only
# command (status, log, diff) leaves the repo untouched, and skipping the copy
# keeps those fast and removes any chance of clobbering.
case "${1:-}" in
    status | log | diff | show | rev-parse | ls-files | branch)
        ;;
    *)
        rm -rf "$REPO/.git"
        cp -R "$WORK" "$REPO/.git"
        ;;
esac

exit $status
