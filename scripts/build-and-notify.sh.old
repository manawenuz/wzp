#!/usr/bin/env bash
set -euo pipefail

# Build Android APK via Docker on SepehrHomeserverdk, upload to rustypaste,
# notify via ntfy.sh/wzp. Fire and forget.
#
# Usage:
#   ./scripts/build-and-notify.sh                      Build current local branch
#   ./scripts/build-and-notify.sh --branch opus-DRED   Build a specific branch
#   ./scripts/build-and-notify.sh --rust               Force Rust rebuild
#   ./scripts/build-and-notify.sh --no-pull            Skip git pull (use cached source)
#   ./scripts/build-and-notify.sh --install            Also download + adb install locally
#
# The remote builder pulls the requested branch from its `origin` (gitea:
# git.manko.yoga). Make sure you've pushed the branch to `origin` before
# running this script, otherwise the remote fetch will fail loudly.

REMOTE_HOST="SepehrHomeserverdk"
BASE_DIR="/mnt/storage/manBuilder"
NTFY_TOPIC="https://ntfy.sh/wzp"
LOCAL_OUTPUT="target/android-apk"
SSH_OPTS="-o ConnectTimeout=15 -o ServerAliveInterval=15 -o ServerAliveCountMax=4 -o LogLevel=ERROR"

REBUILD_RUST=0
DO_PULL=1
DO_INSTALL=0
# Default to whatever branch the local workspace is on — "build what I'm
# working on" is the intuitive behavior for iterative development.
BRANCH=$(git -C "$(dirname "$0")/.." branch --show-current 2>/dev/null || echo "")
while [ $# -gt 0 ]; do
    case "$1" in
        --rust) REBUILD_RUST=1 ;;
        --pull) DO_PULL=1 ;;
        --no-pull) DO_PULL=0 ;;
        --install) DO_INSTALL=1 ;;
        --branch)
            shift
            BRANCH="$1"
            ;;
        --branch=*) BRANCH="${1#--branch=}" ;;
        *) echo "Unknown arg: $1"; exit 1 ;;
    esac
    shift
done
if [ -z "$BRANCH" ]; then
    echo "ERROR: could not determine target branch (detached HEAD?). Pass --branch NAME."
    exit 1
fi
echo "Target branch: $BRANCH"

log() { echo -e "\033[1;36m>>> $*\033[0m"; }

ssh_cmd() { ssh -A $SSH_OPTS "$REMOTE_HOST" "$@"; }

# Upload the remote build script
log "Uploading build script to remote..."
ssh_cmd "cat > /tmp/wzp-docker-build.sh" <<'REMOTE_SCRIPT'
#!/usr/bin/env bash
set -euo pipefail

BASE_DIR="/mnt/storage/manBuilder"
NTFY_TOPIC="https://ntfy.sh/wzp"
REBUILD_RUST="${1:-0}"
DO_PULL="${2:-0}"
BRANCH="${3:-}"

if [ -z "$BRANCH" ]; then
    echo "ERROR: remote script invoked without a BRANCH argument"
    exit 1
fi

notify() { curl -s -d "$1" "$NTFY_TOPIC" > /dev/null 2>&1 || true; }

trap 'notify "WZP Android build FAILED [$BRANCH]! Check /tmp/wzp-build.log"' ERR

# Pull the requested branch. Previously this was hardcoded to
# feat/android-voip-client with `|| true` on the reset, which silently
# left the tree on whatever branch it was last on when the hardcoded
# branch didn't exist on origin. Now the branch is a parameter and any
# failure aborts the build so nobody ships an APK from the wrong source.
if [ "$DO_PULL" = "1" ]; then
    echo ">>> Pulling branch '$BRANCH' from origin..."
    cd "$BASE_DIR/data/source"
    git reset --hard HEAD 2>/dev/null || true
    git clean -fd 2>/dev/null || true
    git gc --prune=now 2>/dev/null || true
    git fetch origin "$BRANCH"
    git reset --hard "origin/$BRANCH"
    BUILT_HASH=$(git rev-parse --short HEAD)
    BUILT_SUBJECT=$(git log -1 --format=%s)
    echo ">>> HEAD after pull: $BUILT_HASH — $BUILT_SUBJECT"
fi

# Clean Rust if requested
if [ "$REBUILD_RUST" = "1" ]; then
    echo ">>> Cleaning Rust target..."
    rm -rf "$BASE_DIR/data/cache/target/aarch64-linux-android/release"
fi

# Fix perms
find "$BASE_DIR/data/source" "$BASE_DIR/data/cache" \
    ! -user 1000 -o ! -group 1000 2>/dev/null | \
    xargs -r chown 1000:1000 2>/dev/null || true

# Clean jniLibs
rm -rf "$BASE_DIR/data/source/android/app/src/main/jniLibs/arm64-v8a"

GIT_HASH=$(cd $BASE_DIR/data/source && git rev-parse --short HEAD 2>/dev/null || echo unknown)
notify "WZP Android build started [$BRANCH @ $GIT_HASH]..."

echo ">>> Building in Docker..."
docker run --rm --user 1000:1000 \
    -v "$BASE_DIR/data/source:/build/source" \
    -v "$BASE_DIR/data/cache/cargo-registry:/home/builder/.cargo/registry" \
    -v "$BASE_DIR/data/cache/cargo-git:/home/builder/.cargo/git" \
    -v "$BASE_DIR/data/cache/target:/build/source/target" \
    -v "$BASE_DIR/data/cache/gradle:/home/builder/.gradle" \
    wzp-android-builder bash -c '
set -euo pipefail
cd /build/source

echo ">>> Rust build..."
cargo ndk -t arm64-v8a -o android/app/src/main/jniLibs build --release -p wzp-android 2>&1 | tail -5

echo ">>> Checking .so files..."
# cargo-ndk may not copy libc++_shared.so — grab it from the NDK if missing
if [ ! -f android/app/src/main/jniLibs/arm64-v8a/libc++_shared.so ]; then
    echo ">>> libc++_shared.so missing, copying from NDK..."
    NDK_LIBCXX=$(find "$ANDROID_NDK_HOME" -name "libc++_shared.so" -path "*/aarch64-linux-android/*" | head -1)
    if [ -n "$NDK_LIBCXX" ]; then
        cp "$NDK_LIBCXX" android/app/src/main/jniLibs/arm64-v8a/
        echo "Copied from: $NDK_LIBCXX"
    else
        echo "WARNING: libc++_shared.so not found in NDK, APK may crash at runtime"
    fi
fi
ls -lh android/app/src/main/jniLibs/arm64-v8a/
[ -f android/app/src/main/jniLibs/arm64-v8a/libwzp_android.so ] || { echo "ERROR: libwzp_android.so missing!"; exit 1; }

echo ">>> APK build..."
cd android && chmod +x gradlew
./gradlew clean assembleDebug --no-daemon --warning-mode=none 2>&1 | tail -3
echo "APK_BUILT"
'

# Upload to rustypaste
echo ">>> Uploading to rustypaste..."
source "$BASE_DIR/.env"
APK=$(find "$BASE_DIR/data/source/android" -name "app-debug*.apk" -path "*/outputs/apk/*" | head -1)
if [ -n "$APK" ]; then
    URL=$(curl -s -F "file=@$APK" -H "Authorization: $rusty_auth_token" "$rusty_address")
    echo "UPLOAD_URL=$URL"
    notify "WZP Android [$BRANCH @ $GIT_HASH] done! APK: $URL"
    echo ">>> Done! APK at: $URL"
else
    notify "WZP Android FAILED [$BRANCH @ $GIT_HASH] - no APK"
    echo "ERROR: No APK found"
    exit 1
fi
REMOTE_SCRIPT

ssh_cmd "chmod +x /tmp/wzp-docker-build.sh"

# Run in tmux
log "Starting build in tmux (branch: $BRANCH)..."
ssh_cmd "tmux kill-session -t wzp-build 2>/dev/null; true"
ssh_cmd "tmux new-session -d -s wzp-build '/tmp/wzp-docker-build.sh $REBUILD_RUST $DO_PULL $BRANCH 2>&1 | tee /tmp/wzp-build.log'"

log "Build running! You'll get a notification on ntfy.sh/wzp with the download URL."
echo ""
echo "  Monitor:  ssh $REMOTE_HOST 'tail -f /tmp/wzp-build.log'"
echo "  Status:   ssh $REMOTE_HOST 'tail -5 /tmp/wzp-build.log'"
echo ""

# Optionally wait and install locally
if [ "$DO_INSTALL" = "1" ]; then
    log "Waiting for build to finish..."
    while true; do
        sleep 15
        if ssh_cmd "grep -q 'UPLOAD_URL\|ERROR' /tmp/wzp-build.log 2>/dev/null"; then
            break
        fi
    done

    URL=$(ssh_cmd "grep UPLOAD_URL /tmp/wzp-build.log | tail -1 | cut -d= -f2")
    if [ -n "$URL" ]; then
        log "Downloading APK..."
        mkdir -p "$LOCAL_OUTPUT"
        curl -s -o "$LOCAL_OUTPUT/wzp-debug.apk" "$URL"
        log "Installing..."
        adb uninstall com.wzp.phone 2>/dev/null || true
        adb install "$LOCAL_OUTPUT/wzp-debug.apk"
        log "Done!"
    else
        err "Build failed"
    fi
fi
