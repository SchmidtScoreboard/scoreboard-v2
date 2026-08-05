#!/usr/bin/env bash
# Run git against this repo when macOS refuses `access()` on files inside
# ~/Documents.
#
# THE SYMPTOM
#   fatal: unable to access '.git/config': Operation not permitted
#   ...while `cat .git/config` works fine.
#
# THE CAUSE
#   iCloud EVICTION, not permissions. When iCloud reclaims space it turns a
#   file into a placeholder ("dataless"). `open()` triggers a download and
#   waits, so cat/editors/compilers mostly work. `access()` does not — it
#   returns EPERM on a dataless file instead of fetching it, and access(R_OK)
#   is exactly what git calls before reading a config. Whichever files happen
#   to be materialized work, so the failure looks intermittent.
#
#   No permission is being denied. Granting Full Disk Access does nothing.
#
# THE REAL FIX
#   Pin the repo so iCloud stops evicting it: right-click the folder in Finder
#   and choose "Keep Downloaded".
#
#   Failing that, force a read of whatever is evicted and it materializes:
#       find .git -type f -exec cat {} + > /dev/null
#
# THIS SCRIPT
#   Copies the git dir out of the synced path, runs the command, copies it
#   back. That works, but on a large repo the copy is slow for exactly the
#   reason above — every evicted object has to download. Prefer pinning.
#
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
