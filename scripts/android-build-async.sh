#!/usr/bin/env bash
# Fire-and-forget Android APK builder.
#
# Uploads the build script to SepehrHomeserverdk, starts it in a tmux
# session so it survives SSH disconnects, then exits immediately.
# Progress and the finished APK URL arrive via ntfy.sh/wzp.
#
# Usage:
#   ./scripts/android-build-async.sh           # build current branch, arm64
#   ./scripts/android-build-async.sh --init    # also run cargo tauri android init
#   ./scripts/android-build-async.sh --rust    # force-clean Rust target cache
#   ./scripts/android-build-async.sh --no-pull # skip git fetch on remote
#   ./scripts/android-build-async.sh --wait    # block until done, then download APK
#
# When the build finishes, ntfy.sh/wzp will show:
#   "WZP Tauri arm64 [<hash>] ready! <rustypaste-url>"
# or on failure:
#   "WZP Tauri Android build FAILED [<hash>] (line N) log: <url>"

set -euo pipefail

REMOTE_HOST="SepehrHomeserverdk"
NTFY_TOPIC="https://ntfy.sh/wzp"
LOCAL_OUTPUT="target/tauri-android-apk"
TMUX_SESSION="wzp-android"
REMOTE_LOG="/tmp/wzp-tauri-build.log"
SSH_OPTS="-o ConnectTimeout=15 -o ServerAliveInterval=30 -o ServerAliveCountMax=6 -o LogLevel=ERROR"

BRANCH="${WZP_BRANCH:-$(git -C "$(dirname "$0")/.." branch --show-current 2>/dev/null || echo "")}"
DO_PULL=1
DO_INIT=0
BUILD_RELEASE=1
REBUILD_RUST=0
BUILD_ARCH="arm64"
DO_WAIT=0

for arg in "$@"; do
    case "$arg" in
        --pull)     DO_PULL=1 ;;
        --no-pull)  DO_PULL=0 ;;
        --init)     DO_INIT=1 ;;
        --debug)    BUILD_RELEASE=0 ;;
        --rust)     REBUILD_RUST=1 ;;
        --wait)     DO_WAIT=1 ;;
    esac
done

if [ -z "$BRANCH" ]; then
    echo "ERROR: could not determine branch (detached HEAD?). Set WZP_BRANCH=name."
    exit 1
fi

log()  { echo -e "\033[1;36m>>> $*\033[0m"; }
err()  { echo -e "\033[1;31mERROR: $*\033[0m" >&2; }
ssh_q() { ssh $SSH_OPTS "$REMOTE_HOST" "$@"; }

# ── Step 1: upload the remote build script ──────────────────────────────────
log "Uploading build script to $REMOTE_HOST..."
# Re-use the existing full build script (it already handles all logic).
scp $SSH_OPTS "$(dirname "$0")/build-tauri-android.sh" "$REMOTE_HOST:/tmp/wzp-tauri-build-full.sh"
ssh_q "chmod +x /tmp/wzp-tauri-build-full.sh"

# ── Step 2: launch in tmux (detached) ──────────────────────────────────────
log "Starting build in tmux session '$TMUX_SESSION' on $REMOTE_HOST..."
ssh_q "tmux kill-session -t $TMUX_SESSION 2>/dev/null; true"

# The full script accepts flags directly; pass them through.
REMOTE_FLAGS=""
[ "$DO_PULL"       = "1" ] || REMOTE_FLAGS="$REMOTE_FLAGS --no-pull"
[ "$DO_INIT"       = "1" ] && REMOTE_FLAGS="$REMOTE_FLAGS --init"
[ "$BUILD_RELEASE" = "0" ] && REMOTE_FLAGS="$REMOTE_FLAGS --debug"
[ "$REBUILD_RUST"  = "1" ] && REMOTE_FLAGS="$REMOTE_FLAGS --rust"

# Run via WZP_BRANCH so the remote script picks up the right branch
# (it calls `git branch --show-current` which would return the remote's
# currently checked-out branch, not necessarily the one we want).
ssh_q "tmux new-session -d -s $TMUX_SESSION \
  'WZP_BRANCH=$BRANCH bash /tmp/wzp-tauri-build-full.sh $REMOTE_FLAGS \
   2>&1 | tee $REMOTE_LOG; echo DONE_EXIT_CODE=\$? >> $REMOTE_LOG'"

log "Build dispatched! Notification on ntfy.sh/wzp when done."
echo ""
echo "  Monitor : ssh $REMOTE_HOST 'tail -f $REMOTE_LOG'"
echo "  Status  : ssh $REMOTE_HOST 'tail -5 $REMOTE_LOG'"
echo "  Attach  : ssh $REMOTE_HOST 'tmux attach -t $TMUX_SESSION'"
echo ""

# ── Step 3 (optional --wait): block until done, download APK ───────────────
if [ "$DO_WAIT" = "0" ]; then
    exit 0
fi

log "Waiting for build to finish (monitoring $REMOTE_LOG)..."
ssh_q "until grep -qE 'APK_REMOTE_PATH|FAILED|ERROR|DONE_EXIT_CODE' \
    $REMOTE_LOG 2>/dev/null; do sleep 20; done"

# Check for failure
if ssh_q "grep -q 'FAILED\|ERROR' $REMOTE_LOG 2>/dev/null" && \
   ! ssh_q "grep -q 'APK_REMOTE_PATH' $REMOTE_LOG 2>/dev/null"; then
    err "Build failed — check ntfy or: ssh $REMOTE_HOST 'cat $REMOTE_LOG'"
    exit 1
fi

# Grab APK paths from log
APK_REMOTES=$(ssh_q "grep '^APK_REMOTE_PATH=' $REMOTE_LOG | cut -d= -f2-")
if [ -z "$APK_REMOTES" ]; then
    err "No APK_REMOTE_PATH in log — build may have failed silently"
    ssh_q "tail -20 $REMOTE_LOG" >&2
    exit 1
fi

mkdir -p "$LOCAL_OUTPUT"
echo "$APK_REMOTES" | while IFS= read -r REMOTE_PATH; do
    [ -z "$REMOTE_PATH" ] && continue
    APK_NAME=$(basename "$REMOTE_PATH")
    log "Downloading $APK_NAME..."
    scp $SSH_OPTS "$REMOTE_HOST:$REMOTE_PATH" "$LOCAL_OUTPUT/$APK_NAME"
    echo "  $LOCAL_OUTPUT/$APK_NAME ($(du -h "$LOCAL_OUTPUT/$APK_NAME" | cut -f1))"
done

log "Done! APKs in $LOCAL_OUTPUT/"
ls -lh "$LOCAL_OUTPUT"/wzp-tauri-*.apk 2>/dev/null || true
