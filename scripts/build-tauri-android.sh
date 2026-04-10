#!/usr/bin/env bash
set -euo pipefail

# =============================================================================
# WZ Phone — Tauri 2.x Mobile Android APK build
#
# Builds the desktop/ Tauri app as an Android APK via cargo-tauri inside the
# wzp-android-builder Docker image on SepehrHomeserverdk. Uploads the APK to
# rustypaste, fires ntfy.sh/wzp notifications at start + finish, and SCPs the
# APK back locally.
#
# Same pattern as build-and-notify.sh but for the Tauri mobile pipeline:
#   - Source: desktop/src-tauri/  (not android/)
#   - Build:  cargo tauri android build  (not gradlew assembleDebug)
#   - Output: desktop/src-tauri/gen/android/.../*.apk
#
# Usage:
#   ./scripts/build-tauri-android.sh                  # full pipeline (debug)
#   ./scripts/build-tauri-android.sh --release        # release APK
#   ./scripts/build-tauri-android.sh --no-pull        # skip git fetch
#   ./scripts/build-tauri-android.sh --rust           # force-clean rust target
#   ./scripts/build-tauri-android.sh --init           # also run `cargo tauri android init`
#
# Environment:
#   WZP_BRANCH   Branch to build (default: feat/desktop-audio-rewrite)
# =============================================================================

REMOTE_HOST="SepehrHomeserverdk"
BASE_DIR="/mnt/storage/manBuilder"
NTFY_TOPIC="https://ntfy.sh/wzp"
LOCAL_OUTPUT="target/tauri-android-apk"
BRANCH="${WZP_BRANCH:-feat/desktop-audio-rewrite}"
SSH_OPTS="-o ConnectTimeout=15 -o ServerAliveInterval=15 -o ServerAliveCountMax=4 -o LogLevel=ERROR"

REBUILD_RUST=0
DO_PULL=1
DO_INIT=0
BUILD_RELEASE=0
for arg in "$@"; do
    case "$arg" in
        --rust)     REBUILD_RUST=1 ;;
        --pull)     DO_PULL=1 ;;
        --no-pull)  DO_PULL=0 ;;
        --init)     DO_INIT=1 ;;
        --release)  BUILD_RELEASE=1 ;;
        -h|--help)
            sed -n '3,30p' "$0"
            exit 0
            ;;
    esac
done

log() { echo -e "\033[1;36m>>> $*\033[0m"; }
ssh_cmd() { ssh -A $SSH_OPTS "$REMOTE_HOST" "$@"; }

notify_local() { curl -s -d "$1" "$NTFY_TOPIC" > /dev/null 2>&1 || true; }

mkdir -p "$LOCAL_OUTPUT"

log "Uploading remote build script..."
ssh_cmd "cat > /tmp/wzp-tauri-build.sh" <<'REMOTE_SCRIPT'
#!/usr/bin/env bash
set -euo pipefail

BASE_DIR="/mnt/storage/manBuilder"
NTFY_TOPIC="https://ntfy.sh/wzp"
BRANCH="${1:-feat/desktop-audio-rewrite}"
DO_PULL="${2:-1}"
REBUILD_RUST="${3:-0}"
DO_INIT="${4:-0}"
BUILD_RELEASE="${5:-0}"

LOG_FILE=/tmp/wzp-tauri-build.log
GIT_HASH="unknown"  # populated after fetch
ENV_FILE="$BASE_DIR/.env"

notify() { curl -s -d "$1" "$NTFY_TOPIC" > /dev/null 2>&1 || true; }

# Upload a file to rustypaste; print URL on stdout (or empty on failure).
upload_to_rustypaste() {
    local file="$1"
    [ ! -f "$ENV_FILE" ] && { echo ""; return; }
    # shellcheck disable=SC1090
    source "$ENV_FILE"
    if [ -n "${rusty_address:-}" ] && [ -n "${rusty_auth_token:-}" ]; then
        curl -s -F "file=@$file" -H "Authorization: $rusty_auth_token" "$rusty_address" || echo ""
    else
        echo ""
    fi
}

# On failure: upload the build log to rustypaste, then notify with hash + url.
on_error() {
    local line="$1"
    local log_url
    log_url=$(upload_to_rustypaste "$LOG_FILE" || echo "")
    if [ -n "$log_url" ]; then
        notify "WZP Tauri Android build FAILED [$GIT_HASH] (line $line)
log: $log_url"
    else
        notify "WZP Tauri Android build FAILED [$GIT_HASH] (line $line) — log upload failed, see $LOG_FILE on remote"
    fi
}
trap 'on_error $LINENO' ERR

exec > >(tee "$LOG_FILE") 2>&1

if [ "$DO_PULL" = "1" ]; then
    echo ">>> git fetch + reset $BRANCH"
    cd "$BASE_DIR/data/source"
    git reset --hard HEAD 2>/dev/null || true
    # NOTE: deliberately do NOT run `git clean -fd` here. It would wipe the
    # tauri-generated `desktop/src-tauri/gen/android/` scaffold (gradlew,
    # settings.gradle, etc.) which is expensive to recreate and breaks
    # subsequent builds with "gradlew not found".
    git gc --prune=now 2>/dev/null || true
    git fetch origin "$BRANCH" 2>&1 | tail -3
    git checkout "$BRANCH" 2>/dev/null || git checkout -b "$BRANCH" "origin/$BRANCH"
    git reset --hard "origin/$BRANCH"
    git submodule update --init || true
fi

GIT_HASH=$(cd "$BASE_DIR/data/source" && git rev-parse --short HEAD 2>/dev/null || echo unknown)
GIT_MSG=$(cd "$BASE_DIR/data/source" && git log -1 --pretty=%s 2>/dev/null | head -c 60 || echo "?")
notify "WZP Tauri Android build STARTED [$GIT_HASH] — $GIT_MSG"

# Fix perms so uid 1000 can write
find "$BASE_DIR/data/source" "$BASE_DIR/data/cache" \
    ! -user 1000 -o ! -group 1000 2>/dev/null | \
    xargs -r chown 1000:1000 2>/dev/null || true

# Optionally clean rust target for android triples
if [ "$REBUILD_RUST" = "1" ]; then
    echo ">>> Cleaning Rust android target dirs..."
    rm -rf "$BASE_DIR/data/cache/target/aarch64-linux-android" \
           "$BASE_DIR/data/cache/target/armv7-linux-androideabi" \
           "$BASE_DIR/data/cache/target/i686-linux-android" \
           "$BASE_DIR/data/cache/target/x86_64-linux-android"
fi

# Profile flag
PROFILE_FLAG="--debug"
[ "$BUILD_RELEASE" = "1" ] && PROFILE_FLAG=""

# Persist ~/.android (where the auto-generated debug.keystore lives) so every
# build is signed with the SAME key. Without this, every fresh container gets
# a new debug keystore and `adb install -r` fails with INSTALL_FAILED_UPDATE_
# INCOMPATIBLE because the signature changed.
mkdir -p "$BASE_DIR/data/cache/android-home"
chown 1000:1000 "$BASE_DIR/data/cache/android-home" 2>/dev/null || true

docker run --rm \
    --user 1000:1000 \
    -e DO_INIT="$DO_INIT" \
    -e PROFILE_FLAG="$PROFILE_FLAG" \
    -v "$BASE_DIR/data/source:/build/source" \
    -v "$BASE_DIR/data/cache/cargo-registry:/home/builder/.cargo/registry" \
    -v "$BASE_DIR/data/cache/cargo-git:/home/builder/.cargo/git" \
    -v "$BASE_DIR/data/cache/target:/build/source/target" \
    -v "$BASE_DIR/data/cache/gradle:/home/builder/.gradle" \
    -v "$BASE_DIR/data/cache/android-home:/home/builder/.android" \
    wzp-android-builder \
    bash -c '
set -euo pipefail
cd /build/source/desktop

echo ">>> npm install"
npm install --silent 2>&1 | tail -5 || npm install 2>&1 | tail -20

cd src-tauri

# Run init if forced, OR if the gradle wrapper is missing. Just checking
# for `gen/android` is not enough — Tauri creates a few subdirectories
# during build (app/, buildSrc/, .gradle/) that survive a partial wipe and
# would make a naive `[ ! -d gen/android ]` check return false even though
# the build wrapper itself is gone.
if [ "${DO_INIT}" = "1" ] || [ ! -x gen/android/gradlew ]; then
    echo ">>> cargo tauri android init"
    cargo tauri android init 2>&1 | tail -20
fi

# ─── wzp-native standalone cdylib (built with cargo-ndk, not cargo-tauri) ──
# Produces libwzp_native.so which wzp-desktop dlopens at runtime via
# libloading. Split exists because cargo-tauri`s linker wiring pulls
# bionic private symbols into any cdylib with cc::Build C++, causing
# __init_tcb+4 SIGSEGV. cargo-ndk uses the same linker path as the
# legacy wzp-android crate which works.
echo ">>> cargo ndk build -p wzp-native --release"
JNI_ABI_DIR=gen/android/app/src/main/jniLibs/arm64-v8a
mkdir -p "$JNI_ABI_DIR"
(
    cd /build/source
    cargo ndk -t arm64-v8a -o desktop/src-tauri/gen/android/app/src/main/jniLibs \
        build --release -p wzp-native 2>&1 | tail -10
)
if [ -f "$JNI_ABI_DIR/libwzp_native.so" ]; then
    ls -lh "$JNI_ABI_DIR/libwzp_native.so"
else
    echo ">>> WARNING: libwzp_native.so not produced"
fi

# ─── libc++_shared.so — required by wzp-native at runtime ──────────────
# wzp-native/build.rs uses cpp_link_stdlib(Some("c++_shared")) which adds
# a NEEDED entry for libc++_shared.so to libwzp_native.so. cargo-ndk does
# NOT copy the actual libc++_shared.so into jniLibs, so unless we copy it
# explicitly, the APK ships without it and Android's dynamic linker fails
# the dlopen with "library libc++_shared.so not found" at runtime. Same
# fix that build-and-notify.sh has had for the legacy wzp-android path
# (lines 126-134 there) — ported here for the Tauri pipeline.
if [ ! -f "$JNI_ABI_DIR/libc++_shared.so" ]; then
    echo ">>> libc++_shared.so missing, copying from NDK..."
    NDK_LIBCXX=$(find "$ANDROID_NDK_HOME" -name "libc++_shared.so" -path "*/aarch64-linux-android/*" | head -1)
    if [ -n "$NDK_LIBCXX" ]; then
        cp "$NDK_LIBCXX" "$JNI_ABI_DIR/"
        ls -lh "$JNI_ABI_DIR/libc++_shared.so"
    else
        echo ">>> ERROR: libc++_shared.so not found in NDK — APK will crash at dlopen time"
        exit 1
    fi
fi

echo ">>> cargo tauri android build ${PROFILE_FLAG} --target aarch64 --apk"
cargo tauri android build ${PROFILE_FLAG} --target aarch64 --apk

echo ""
echo ">>> Build artifacts:"
find gen/android -name "*.apk" -exec ls -lh {} \; 2>/dev/null
'

# Locate the produced APK
APK=$(find "$BASE_DIR/data/source/desktop/src-tauri/gen/android" -name "*.apk" -type f 2>/dev/null | head -1)
if [ -z "$APK" ] || [ ! -f "$APK" ]; then
    LOG_URL=$(upload_to_rustypaste "$LOG_FILE" || echo "")
    if [ -n "$LOG_URL" ]; then
        notify "WZP Tauri Android build [$GIT_HASH]: no APK produced
log: $LOG_URL"
    else
        notify "WZP Tauri Android build [$GIT_HASH]: no APK produced — log upload failed"
    fi
    exit 1
fi
APK_SIZE=$(du -h "$APK" | cut -f1)

RUSTY_URL=$(upload_to_rustypaste "$APK" || echo "")
if [ -n "$RUSTY_URL" ]; then
    notify "WZP Tauri Android build OK [$GIT_HASH] ($APK_SIZE)
$RUSTY_URL"
else
    notify "WZP Tauri Android build OK [$GIT_HASH] ($APK_SIZE) — rustypaste upload skipped"
fi

# Print path so the local script can grab it
echo "APK_REMOTE_PATH=$APK"
REMOTE_SCRIPT

ssh_cmd "chmod +x /tmp/wzp-tauri-build.sh"

notify_local "WZP Tauri Android build dispatched (branch=$BRANCH, release=$BUILD_RELEASE)"
log "Triggering remote build (branch=$BRANCH)..."

# Run; capture full output, last line is APK_REMOTE_PATH=...
REMOTE_OUTPUT=$(ssh_cmd "/tmp/wzp-tauri-build.sh '$BRANCH' '$DO_PULL' '$REBUILD_RUST' '$DO_INIT' '$BUILD_RELEASE'" || true)
echo "$REMOTE_OUTPUT" | tail -60

APK_REMOTE=$(echo "$REMOTE_OUTPUT" | grep '^APK_REMOTE_PATH=' | tail -1 | cut -d= -f2-)
if [ -n "$APK_REMOTE" ]; then
    log "Downloading APK to $LOCAL_OUTPUT/wzp-tauri.apk..."
    scp $SSH_OPTS "$REMOTE_HOST:$APK_REMOTE" "$LOCAL_OUTPUT/wzp-tauri.apk"
    echo "  $LOCAL_OUTPUT/wzp-tauri.apk ($(du -h "$LOCAL_OUTPUT/wzp-tauri.apk" | cut -f1))"
else
    log "No APK produced — see ntfy / remote log /tmp/wzp-tauri-build.log"
    exit 1
fi
