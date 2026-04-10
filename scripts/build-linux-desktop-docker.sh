#!/usr/bin/env bash
set -euo pipefail

# =============================================================================
# WZ Phone — Linux x86_64 Tauri desktop build (Docker on SepehrHomeserverdk)
#
# Cross-compiles the Tauri desktop binary for Linux x86_64 inside the
# wzp-linux-desktop-builder image (a thin extension of wzp-android-builder
# that adds GTK3 + WebKit2GTK 4.1 + libsoup-3.0 + appindicator dev packages).
#
# Fires an ntfy.sh/wzp notification on build start and build completion, and
# uploads the resulting .deb + raw binary to rustypaste.
#
# Usage:
#   ./scripts/build-linux-desktop-docker.sh                # Full pipeline
#   ./scripts/build-linux-desktop-docker.sh --no-pull      # Skip git fetch
#   ./scripts/build-linux-desktop-docker.sh --rust         # Clean Rust target
#   ./scripts/build-linux-desktop-docker.sh --image-build  # (Re)build image
#
# Environment:
#   WZP_BRANCH   Branch to build (default: feat/desktop-audio-rewrite)
# =============================================================================

REMOTE_HOST="SepehrHomeserverdk"
BASE_DIR="/mnt/storage/manBuilder"
NTFY_TOPIC="https://ntfy.sh/wzp"
LOCAL_OUTPUT="target/linux-desktop"
BRANCH="${WZP_BRANCH:-feat/desktop-audio-rewrite}"
SSH_OPTS="-o ConnectTimeout=15 -o ServerAliveInterval=15 -o ServerAliveCountMax=4 -o LogLevel=ERROR"

REBUILD_RUST=0
DO_PULL=1
IMAGE_BUILD=0
for arg in "$@"; do
    case "$arg" in
        --rust)         REBUILD_RUST=1 ;;
        --pull)         DO_PULL=1 ;;
        --no-pull)      DO_PULL=0 ;;
        --image-build)  IMAGE_BUILD=1 ;;
        -h|--help)
            sed -n '3,25p' "$0"
            exit 0
            ;;
    esac
done

log() { echo -e "\033[1;36m>>> $*\033[0m"; }
ssh_cmd() { ssh $SSH_OPTS "$REMOTE_HOST" "$@"; }

notify_local() { curl -s -d "$1" "$NTFY_TOPIC" > /dev/null 2>&1 || true; }

mkdir -p "$LOCAL_OUTPUT"

# ─── Optional: (re)build the docker image on the remote ────────────────────
if [ "$IMAGE_BUILD" = "1" ]; then
    log "Uploading Dockerfile.linux-desktop-builder to remote..."
    scp $SSH_OPTS "$(dirname "$0")/Dockerfile.linux-desktop-builder" \
        "$REMOTE_HOST:$BASE_DIR/Dockerfile.linux-desktop-builder"

    log "Triggering remote image build (fire-and-forget)..."
    ssh_cmd "cd $BASE_DIR && \
        nohup docker build -f Dockerfile.linux-desktop-builder \
            -t wzp-linux-desktop-builder . \
            > /tmp/wzp-linux-desktop-image-build.log 2>&1 & \
        echo 'image build PID: '\$!"
    notify_local "WZP Linux desktop image build dispatched"
    log "Image build running in background on $REMOTE_HOST."
    log "Tail the log with:  ssh $REMOTE_HOST 'tail -f /tmp/wzp-linux-desktop-image-build.log'"
    exit 0
fi

# ─── Upload remote build runner script ─────────────────────────────────────
log "Uploading remote build script..."
ssh_cmd "cat > /tmp/wzp-linux-desktop-build.sh" <<'REMOTE_SCRIPT'
#!/usr/bin/env bash
set -euo pipefail

BASE_DIR="/mnt/storage/manBuilder"
NTFY_TOPIC="https://ntfy.sh/wzp"
BRANCH="${1:-feat/desktop-audio-rewrite}"
DO_PULL="${2:-1}"
REBUILD_RUST="${3:-0}"

LOG_FILE=/tmp/wzp-linux-desktop-build.log
GIT_HASH="unknown"
ENV_FILE="$BASE_DIR/.env"

notify() { curl -s -d "$1" "$NTFY_TOPIC" > /dev/null 2>&1 || true; }

# Upload to rustypaste; print URL on stdout (or empty on failure).
upload_to_rustypaste() {
    local file="$1"
    [ ! -f "$file" ] && { echo ""; return; }
    # shellcheck disable=SC1090
    source "$ENV_FILE"
    if [ -n "${rusty_address:-}" ] && [ -n "${rusty_auth_token:-}" ]; then
        curl -s -F "file=@$file" -H "Authorization: $rusty_auth_token" "$rusty_address" || echo ""
    else
        echo ""
    fi
}

on_error() {
    local line="$1"
    local log_url
    log_url=$(upload_to_rustypaste "$LOG_FILE" || echo "")
    if [ -n "$log_url" ]; then
        notify "WZP Linux desktop build FAILED [$GIT_HASH] (line $line)
log: $log_url"
    else
        notify "WZP Linux desktop build FAILED [$GIT_HASH] (line $line) — log upload failed"
    fi
}
trap 'on_error $LINENO' ERR

exec > >(tee "$LOG_FILE") 2>&1

# ── git fetch + reset the target branch ───────────────────────────────────
if [ "$DO_PULL" = "1" ]; then
    echo ">>> git fetch + reset $BRANCH"
    cd "$BASE_DIR/data/source"
    git reset --hard HEAD 2>/dev/null || true
    git gc --prune=now 2>/dev/null || true
    git fetch origin "$BRANCH" 2>&1 | tail -3
    git checkout "$BRANCH" 2>/dev/null || git checkout -b "$BRANCH" "origin/$BRANCH"
    git reset --hard "origin/$BRANCH"
    git submodule update --init --recursive || true
fi

GIT_HASH=$(cd "$BASE_DIR/data/source" && git rev-parse --short HEAD 2>/dev/null || echo unknown)
GIT_MSG=$(cd "$BASE_DIR/data/source" && git log -1 --pretty=%s 2>/dev/null | head -c 60 || echo "?")
notify "WZP Linux desktop build STARTED [$GIT_HASH] — $GIT_MSG"

# Fix perms so builder uid 1000 can read/write the mounted source.
find "$BASE_DIR/data/source" "$BASE_DIR/data/cache-linux-desktop" \
    ! -user 1000 -o ! -group 1000 2>/dev/null | \
    xargs -r chown 1000:1000 2>/dev/null || true

if [ "$REBUILD_RUST" = "1" ]; then
    echo ">>> Cleaning Linux desktop Rust target dir..."
    rm -rf "$BASE_DIR/data/cache-linux-desktop/target/x86_64-unknown-linux-gnu" \
           "$BASE_DIR/data/cache-linux-desktop/target/release"
fi

# ── Docker run ─────────────────────────────────────────────────────────────
# Cache volumes:
#   - cargo-registry / cargo-git: shared with the android builder — both use
#     the same crates, so the download cache is worth sharing.
#   - cache-linux-desktop/target: separate target tree for the desktop build
#     to keep it isolated from the Linux CLI build (build-linux-docker.sh
#     uses cache-linux/target for wzp-relay / wzp-client).

mkdir -p "$BASE_DIR/data/cache/cargo-registry" \
         "$BASE_DIR/data/cache/cargo-git" \
         "$BASE_DIR/data/cache-linux-desktop/target"
chown -R 1000:1000 "$BASE_DIR/data/cache-linux-desktop/target" 2>/dev/null || true

docker run --rm \
    --user 1000:1000 \
    -v "$BASE_DIR/data/source:/build/source" \
    -v "$BASE_DIR/data/cache/cargo-registry:/home/builder/.cargo/registry" \
    -v "$BASE_DIR/data/cache/cargo-git:/home/builder/.cargo/git" \
    -v "$BASE_DIR/data/cache-linux-desktop/target:/build/source/target" \
    wzp-linux-desktop-builder \
    bash -c '
set -euo pipefail

cd /build/source/desktop

echo ">>> npm install"
npm install --silent 2>&1 | tail -5 || npm install 2>&1 | tail -20

echo ">>> npm run build"
npm run build 2>&1 | tail -5

echo ">>> cargo tauri build (produces .deb + .AppImage + raw binary)"
cd src-tauri
# tauri-cli is already installed in the base image via the Android
# builder RUN step. It produces target/release/wzp-desktop (raw ELF)
# plus bundles under target/release/bundle/{deb,appimage}/.
cargo tauri build 2>&1 | tail -40

echo ""
echo ">>> Build artifacts:"
ls -lh /build/source/target/release/wzp-desktop 2>/dev/null || echo "NO BINARY"
ls -lh /build/source/target/release/bundle/deb/*.deb 2>/dev/null || echo "NO DEB"
ls -lh /build/source/target/release/bundle/appimage/*.AppImage 2>/dev/null || echo "NO APPIMAGE"
'

# Locate the produced artifacts
BIN="$BASE_DIR/data/cache-linux-desktop/target/release/wzp-desktop"
DEB=$(ls "$BASE_DIR/data/cache-linux-desktop/target/release/bundle/deb/"*.deb 2>/dev/null | head -1 || true)
APPIMAGE=$(ls "$BASE_DIR/data/cache-linux-desktop/target/release/bundle/appimage/"*.AppImage 2>/dev/null | head -1 || true)

if [ ! -f "$BIN" ]; then
    LOG_URL=$(upload_to_rustypaste "$LOG_FILE" || echo "")
    if [ -n "$LOG_URL" ]; then
        notify "WZP Linux desktop build [$GIT_HASH]: no binary produced
log: $LOG_URL"
    else
        notify "WZP Linux desktop build [$GIT_HASH]: no binary produced — log upload failed"
    fi
    exit 1
fi

BIN_SIZE=$(du -h "$BIN" | cut -f1)

# Prefer to ship the .deb if we got one, otherwise fall back to the raw binary.
ARTIFACT="$BIN"
ARTIFACT_KIND="binary"
if [ -n "$DEB" ] && [ -f "$DEB" ]; then
    ARTIFACT="$DEB"
    ARTIFACT_KIND="deb"
    ARTIFACT_SIZE=$(du -h "$DEB" | cut -f1)
else
    ARTIFACT_SIZE="$BIN_SIZE"
fi

RUSTY_URL=$(upload_to_rustypaste "$ARTIFACT" || echo "")
if [ -n "$RUSTY_URL" ]; then
    notify "WZP Linux desktop build OK [$GIT_HASH] ($ARTIFACT_KIND, $ARTIFACT_SIZE)
$RUSTY_URL"
else
    notify "WZP Linux desktop build OK [$GIT_HASH] ($ARTIFACT_KIND, $ARTIFACT_SIZE) — rustypaste upload skipped"
fi

# Print paths so the local script can scp them back
echo "BIN_REMOTE_PATH=$BIN"
[ -n "$DEB" ] && echo "DEB_REMOTE_PATH=$DEB"
[ -n "$APPIMAGE" ] && echo "APPIMAGE_REMOTE_PATH=$APPIMAGE"
REMOTE_SCRIPT

ssh_cmd "chmod +x /tmp/wzp-linux-desktop-build.sh"

notify_local "WZP Linux desktop build dispatched (branch=$BRANCH)"
log "Triggering remote build (branch=$BRANCH)..."

# Run; last lines are *_REMOTE_PATH=...
REMOTE_OUTPUT=$(ssh_cmd "/tmp/wzp-linux-desktop-build.sh '$BRANCH' '$DO_PULL' '$REBUILD_RUST'" || true)
echo "$REMOTE_OUTPUT" | tail -80

BIN_REMOTE=$(echo "$REMOTE_OUTPUT" | grep '^BIN_REMOTE_PATH=' | tail -1 | cut -d= -f2-)
DEB_REMOTE=$(echo "$REMOTE_OUTPUT" | grep '^DEB_REMOTE_PATH=' | tail -1 | cut -d= -f2-)
APPIMAGE_REMOTE=$(echo "$REMOTE_OUTPUT" | grep '^APPIMAGE_REMOTE_PATH=' | tail -1 | cut -d= -f2-)

if [ -n "$BIN_REMOTE" ]; then
    log "Downloading wzp-desktop binary to $LOCAL_OUTPUT/..."
    scp $SSH_OPTS "$REMOTE_HOST:$BIN_REMOTE" "$LOCAL_OUTPUT/wzp-desktop"
    echo "  $LOCAL_OUTPUT/wzp-desktop ($(du -h "$LOCAL_OUTPUT/wzp-desktop" | cut -f1))"
fi

if [ -n "$DEB_REMOTE" ]; then
    log "Downloading .deb to $LOCAL_OUTPUT/..."
    scp $SSH_OPTS "$REMOTE_HOST:$DEB_REMOTE" "$LOCAL_OUTPUT/"
    ls -lh "$LOCAL_OUTPUT"/*.deb
fi

if [ -n "$APPIMAGE_REMOTE" ]; then
    log "Downloading .AppImage to $LOCAL_OUTPUT/..."
    scp $SSH_OPTS "$REMOTE_HOST:$APPIMAGE_REMOTE" "$LOCAL_OUTPUT/"
    ls -lh "$LOCAL_OUTPUT"/*.AppImage
fi

if [ -z "$BIN_REMOTE" ]; then
    log "No binary produced — see ntfy / remote log /tmp/wzp-linux-desktop-build.log"
    exit 1
fi
