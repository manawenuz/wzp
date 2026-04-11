#!/usr/bin/env bash
set -euo pipefail

# Build WarzonePhone Linux x86_64 binaries via Docker on SepehrHomeserverdk.
# Reuses same Docker image as Android build (has Rust + cmake + build tools).
# Fire and forget — notifies via ntfy.sh/wzp with rustypaste URL.
#
# Usage:
#   ./scripts/build-linux-docker.sh              Build + upload + notify
#   ./scripts/build-linux-docker.sh --pull       Git pull before building
#   ./scripts/build-linux-docker.sh --clean      Clean Rust target cache
#   ./scripts/build-linux-docker.sh --install    Download binaries locally after build

REMOTE_HOST="SepehrHomeserverdk"
BASE_DIR="/mnt/storage/manBuilder"
NTFY_TOPIC="https://ntfy.sh/wzp"
LOCAL_OUTPUT="target/linux-x86_64"
SSH_OPTS="-o ConnectTimeout=15 -o ServerAliveInterval=15 -o ServerAliveCountMax=4 -o LogLevel=ERROR"

# Branch to build. Default matches the current active development branch
# (opus-DRED-v2 as of 2026-04-11). Override with `WZP_BRANCH=<name> ./build-linux-docker.sh`
# if you need a different one — e.g. to rebuild the relay from a feature
# branch for A/B testing.
WZP_BRANCH="${WZP_BRANCH:-opus-DRED-v2}"

DO_PULL=1
DO_CLEAN=0
DO_INSTALL=0
for arg in "$@"; do
    case "$arg" in
        --pull) DO_PULL=1 ;;
        --no-pull) DO_PULL=0 ;;
        --clean) DO_CLEAN=1 ;;
        --install) DO_INSTALL=1 ;;
    esac
done

log() { echo -e "\033[1;36m>>> $*\033[0m"; }
err() { echo -e "\033[1;31mERROR: $*\033[0m" >&2; }

ssh_cmd() { ssh $SSH_OPTS "$REMOTE_HOST" "$@"; }

# Upload build script to remote
log "Uploading build script..."
ssh_cmd "cat > /tmp/wzp-linux-build.sh" <<'REMOTE_SCRIPT'
#!/usr/bin/env bash
set -euo pipefail

BASE_DIR="/mnt/storage/manBuilder"
NTFY_TOPIC="https://ntfy.sh/wzp"
DO_PULL="${1:-0}"
DO_CLEAN="${2:-0}"
BRANCH="${3:-opus-DRED-v2}"

notify() { curl -s -d "$1" "$NTFY_TOPIC" > /dev/null 2>&1 || true; }

trap 'notify "WZP Linux build FAILED! Check /tmp/wzp-linux-build.log"' ERR

if [ "$DO_PULL" = "1" ]; then
    echo ">>> Pulling latest ($BRANCH)..."
    cd "$BASE_DIR/data/source"
    git reset --hard HEAD 2>/dev/null || true
    git clean -fd 2>/dev/null || true
    git gc --prune=now 2>/dev/null || true
    git fetch origin "$BRANCH" 2>&1 | tail -3
    git checkout "$BRANCH" 2>/dev/null || git checkout -b "$BRANCH" "origin/$BRANCH"
    git reset --hard "origin/$BRANCH" 2>/dev/null || true
fi

if [ "$DO_CLEAN" = "1" ]; then
    echo ">>> Cleaning Linux target cache..."
    rm -rf "$BASE_DIR/data/cache-linux/target"
fi

# Ensure cache dirs exist (separate from Android cache)
mkdir -p "$BASE_DIR/data/cache-linux/target" \
         "$BASE_DIR/data/cache-linux/cargo-registry" \
         "$BASE_DIR/data/cache-linux/cargo-git"

# Fix perms
find "$BASE_DIR/data/source" "$BASE_DIR/data/cache-linux" \
    ! -user 1000 -o ! -group 1000 2>/dev/null | \
    xargs -r chown 1000:1000 2>/dev/null || true

GIT_HASH=$(cd "$BASE_DIR/data/source" && git rev-parse --short HEAD 2>/dev/null || echo "unknown")
notify "WZP Linux x86_64 build started [$GIT_HASH]..."

echo ">>> Building in Docker..."
docker run --rm --user 1000:1000 \
    -v "$BASE_DIR/data/source:/build/source" \
    -v "$BASE_DIR/data/cache-linux/cargo-registry:/home/builder/.cargo/registry" \
    -v "$BASE_DIR/data/cache-linux/cargo-git:/home/builder/.cargo/git" \
    -v "$BASE_DIR/data/cache-linux/target:/build/source/target" \
    wzp-android-builder bash -c '
set -euo pipefail
cd /build/source

echo ">>> Building relay + client + web + bench..."
cargo build --release --bin wzp-relay --bin wzp-client --bin wzp-web --bin wzp-bench 2>&1 | tail -5

echo ">>> Building audio client..."
cargo build --release --bin wzp-client --features audio 2>&1 | tail -3
cp target/release/wzp-client target/release/wzp-client-audio
cargo build --release --bin wzp-client 2>&1 | tail -3

echo ">>> Binaries:"
ls -lh target/release/wzp-relay target/release/wzp-client target/release/wzp-client-audio target/release/wzp-web target/release/wzp-bench

echo ">>> Packaging..."
tar czf /tmp/wzp-linux-x86_64.tar.gz \
    -C target/release wzp-relay wzp-client wzp-client-audio wzp-web wzp-bench

echo "BINARIES_BUILT"
'

# Upload to rustypaste
echo ">>> Uploading to rustypaste..."
source "$BASE_DIR/.env"
TARBALL="$BASE_DIR/data/cache-linux/target/release/../../../wzp-linux-x86_64.tar.gz"
# Docker wrote to /tmp inside container, copy from target mount
docker run --rm \
    -v "$BASE_DIR/data/cache-linux/target:/build/target" \
    wzp-android-builder bash -c \
    "cp /build/target/release/wzp-relay /build/target/release/wzp-client /build/target/release/wzp-client-audio /build/target/release/wzp-web /build/target/release/wzp-bench /tmp/ && tar czf /tmp/wzp-linux-x86_64.tar.gz -C /tmp wzp-relay wzp-client wzp-client-audio wzp-web wzp-bench && cat /tmp/wzp-linux-x86_64.tar.gz" \
    > /tmp/wzp-linux-x86_64.tar.gz

URL=$(curl -s -F "file=@/tmp/wzp-linux-x86_64.tar.gz" -H "Authorization: $rusty_auth_token" "$rusty_address")
if [ -n "$URL" ]; then
    echo "UPLOAD_URL=$URL"
    notify "WZP Linux x86_64 [$GIT_HASH] ready! $URL"
    echo ">>> Done! Binaries at: $URL"
else
    notify "WZP Linux build FAILED - upload error"
    echo "ERROR: upload failed"
    exit 1
fi
REMOTE_SCRIPT

ssh_cmd "chmod +x /tmp/wzp-linux-build.sh"

# Run in tmux
log "Starting Linux build in tmux..."
ssh_cmd "tmux kill-session -t wzp-linux 2>/dev/null; true"
ssh_cmd "tmux new-session -d -s wzp-linux '/tmp/wzp-linux-build.sh $DO_PULL $DO_CLEAN $WZP_BRANCH 2>&1 | tee /tmp/wzp-linux-build.log'"

log "Build running! Notification on ntfy.sh/wzp when done."
echo ""
echo "  Monitor:  ssh $REMOTE_HOST 'tail -f /tmp/wzp-linux-build.log'"
echo "  Status:   ssh $REMOTE_HOST 'tail -5 /tmp/wzp-linux-build.log'"
echo ""

# Optionally wait and download
if [ "$DO_INSTALL" = "1" ]; then
    log "Waiting for build..."
    while true; do
        sleep 15
        if ssh_cmd "grep -q 'UPLOAD_URL\|ERROR' /tmp/wzp-linux-build.log 2>/dev/null"; then
            break
        fi
    done

    URL=$(ssh_cmd "grep UPLOAD_URL /tmp/wzp-linux-build.log | tail -1 | cut -d= -f2")
    if [ -n "$URL" ]; then
        log "Downloading binaries..."
        mkdir -p "$LOCAL_OUTPUT"
        curl -s -o "$LOCAL_OUTPUT/wzp-linux-x86_64.tar.gz" "$URL"
        tar xzf "$LOCAL_OUTPUT/wzp-linux-x86_64.tar.gz" -C "$LOCAL_OUTPUT/"
        rm "$LOCAL_OUTPUT/wzp-linux-x86_64.tar.gz"
        ls -lh "$LOCAL_OUTPUT"/wzp-*
        log "Done! Binaries in $LOCAL_OUTPUT/"
    else
        err "Build failed"
    fi
fi
