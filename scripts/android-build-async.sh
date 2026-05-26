#!/usr/bin/env bash
# Fire-and-forget Android APK builder.
#
# Runs ./scripts/build-tauri-android.sh inside a LOCAL tmux session so the
# build survives terminal disconnects. The wrapped script SSHes to
# SepehrHomeserverdk on its own — we don't try to upload+run anything on
# the remote (that would re-SSH from the remote to itself, which fails).
#
# Usage:
#   ./scripts/android-build-async.sh           # build current branch, arm64
#   ./scripts/android-build-async.sh --init    # also run cargo tauri android init
#   ./scripts/android-build-async.sh --rust    # force-clean Rust target cache
#   ./scripts/android-build-async.sh --no-pull # skip git fetch on remote
#   ./scripts/android-build-async.sh --debug   # debug APK
#   ./scripts/android-build-async.sh --release-debuggable  # release APK with run-as dumps
#   ./scripts/android-build-async.sh --wait    # block until done, then tail status
#
# Progress / completion: ntfy.sh/wzp (handled by build-tauri-android.sh).
# Monitor locally:  tmux attach -t wzp-android-local
#                   tail -f /tmp/wzp-tauri-build-local.log

set -euo pipefail

TMUX_SESSION="wzp-android-local"
LOCAL_LOG="/tmp/wzp-tauri-build-local.log"
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
BUILD_SCRIPT="$SCRIPT_DIR/build-tauri-android.sh"

if ! command -v tmux >/dev/null 2>&1; then
    echo "ERROR: tmux is not installed locally. Install with: brew install tmux"
    exit 1
fi

if [ ! -x "$BUILD_SCRIPT" ]; then
    echo "ERROR: $BUILD_SCRIPT not found or not executable"
    exit 1
fi

BRANCH="${WZP_BRANCH:-$(git -C "$REPO_DIR" branch --show-current 2>/dev/null || echo "")}"
if [ -z "$BRANCH" ]; then
    echo "ERROR: could not determine branch (detached HEAD?). Set WZP_BRANCH=name."
    exit 1
fi

DO_WAIT=0
PASS_ARGS=()
for arg in "$@"; do
    case "$arg" in
        --wait) DO_WAIT=1 ;;
        *)      PASS_ARGS+=("$arg") ;;
    esac
done

log() { echo -e "\033[1;36m>>> $*\033[0m"; }

# Kill any prior session that might still be hanging around.
tmux kill-session -t "$TMUX_SESSION" 2>/dev/null || true

# Write a launcher script — avoids fragile quoting inside `tmux new-session`.
LAUNCHER="$(mktemp -t wzp-android-launcher.XXXXXX)"
chmod +x "$LAUNCHER"
{
    echo "#!/usr/bin/env bash"
    echo "set -o pipefail"
    echo "cd $(printf %q "$REPO_DIR")"
    echo "export WZP_BRANCH=$(printf %q "$BRANCH")"
    printf 'bash %q' "$BUILD_SCRIPT"
    for a in "${PASS_ARGS[@]:-}"; do
        [ -z "$a" ] && continue
        printf ' %q' "$a"
    done
    echo " 2>&1 | tee $(printf %q "$LOCAL_LOG")"
    echo "echo DONE_EXIT_CODE=\$? >> $(printf %q "$LOCAL_LOG")"
} > "$LAUNCHER"

# Create the log file up front so `tail -f` works immediately.
: > "$LOCAL_LOG"

log "Starting local tmux session '$TMUX_SESSION' (branch: $BRANCH)..."
log "Build script: $BUILD_SCRIPT ${PASS_ARGS[*]:-}"
log "Launcher:     $LAUNCHER"
log "Local log:    $LOCAL_LOG"

tmux new-session -d -s "$TMUX_SESSION" -c "$REPO_DIR" "bash $LAUNCHER; exec bash"

# Verify the session actually started.
sleep 1
if ! tmux has-session -t "$TMUX_SESSION" 2>/dev/null; then
    echo "ERROR: tmux session '$TMUX_SESSION' failed to start. Launcher contents:"
    cat "$LAUNCHER"
    exit 1
fi

log "Build dispatched! ntfy.sh/wzp will notify on completion."
echo ""
echo "  Monitor : tail -f $LOCAL_LOG"
echo "  Status  : tail -5 $LOCAL_LOG"
echo "  Attach  : tmux attach -t $TMUX_SESSION"
echo "  Kill    : tmux kill-session -t $TMUX_SESSION"
echo ""

if [ "$DO_WAIT" = "0" ]; then
    exit 0
fi

log "Waiting for build to finish (watching $LOCAL_LOG)..."
until grep -qE 'DONE_EXIT_CODE|APK_REMOTE_PATH=|FAILED' "$LOCAL_LOG" 2>/dev/null; do
    sleep 20
done

log "Build session ended. Last 20 lines:"
tail -20 "$LOCAL_LOG"
