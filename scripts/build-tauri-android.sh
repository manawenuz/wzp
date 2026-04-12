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
#   ./scripts/build-tauri-android.sh                  # full pipeline (debug, arm64 only)
#   ./scripts/build-tauri-android.sh --release        # release APK
#   ./scripts/build-tauri-android.sh --no-pull        # skip git fetch
#   ./scripts/build-tauri-android.sh --rust           # force-clean rust target
#   ./scripts/build-tauri-android.sh --init           # also run `cargo tauri android init`
#   ./scripts/build-tauri-android.sh --arch arm64     # arm64 only (default)
#   ./scripts/build-tauri-android.sh --arch armv7     # armv7 only (smaller APK)
#   ./scripts/build-tauri-android.sh --arch all       # both arm64 + armv7 (separate APKs)
#
# Environment:
#   WZP_BRANCH   Branch to build (default: feat/desktop-audio-rewrite)
# =============================================================================

REMOTE_HOST="SepehrHomeserverdk"
BASE_DIR="/mnt/storage/manBuilder"
NTFY_TOPIC="https://ntfy.sh/wzp"
LOCAL_OUTPUT="target/tauri-android-apk"
BRANCH="${WZP_BRANCH:-$(git -C "$(dirname "$0")/.." branch --show-current 2>/dev/null || echo "")}"
SSH_OPTS="-o ConnectTimeout=15 -o ServerAliveInterval=15 -o ServerAliveCountMax=4 -o LogLevel=ERROR"

REBUILD_RUST=0
DO_PULL=1
DO_INIT=0
BUILD_RELEASE=0
BUILD_ARCH="arm64"
NEXT_IS_ARCH=0
for arg in "$@"; do
    if [ "$NEXT_IS_ARCH" = "1" ]; then
        BUILD_ARCH="$arg"
        NEXT_IS_ARCH=0
        continue
    fi
    case "$arg" in
        --rust)     REBUILD_RUST=1 ;;
        --pull)     DO_PULL=1 ;;
        --no-pull)  DO_PULL=0 ;;
        --init)     DO_INIT=1 ;;
        --release)  BUILD_RELEASE=1 ;;
        --arch)     NEXT_IS_ARCH=1 ;;
        -h|--help)
            sed -n '3,32p' "$0"
            exit 0
            ;;
    esac
done

# Validate --arch
case "$BUILD_ARCH" in
    arm64|armv7|all) ;;
    *) echo "ERROR: --arch must be arm64, armv7, or all (got: $BUILD_ARCH)"; exit 1 ;;
esac

if [ -z "$BRANCH" ]; then
    echo "ERROR: could not determine target branch (detached HEAD?). Pass WZP_BRANCH=name."
    exit 1
fi
echo "Target branch: $BRANCH  arch: $BUILD_ARCH"

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
BUILD_ARCH="${6:-arm64}"

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

# ─── Determine target architectures ──────────────────────────────────────
# Maps BUILD_ARCH to cargo-ndk ABI names and cargo-tauri target names.
# BUILD_ARCH=arm64 → one APK; BUILD_ARCH=armv7 → one APK; BUILD_ARCH=all → two APKs.
case "$BUILD_ARCH" in
    arm64)  ARCH_LIST="arm64" ;;
    armv7)  ARCH_LIST="armv7" ;;
    all)    ARCH_LIST="arm64 armv7" ;;
esac

# Mapping functions (used inside docker via env vars)
# cargo-ndk ABI:   arm64-v8a | armeabi-v7a
# cargo-tauri:     aarch64   | armv7
# NDK sysroot:     aarch64-linux-android | arm-linux-androideabi

docker run --rm \
    --user 1000:1000 \
    -e DO_INIT="$DO_INIT" \
    -e PROFILE_FLAG="$PROFILE_FLAG" \
    -e BUILD_ARCH="$BUILD_ARCH" \
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

# ─── Arch list from BUILD_ARCH env var ───────────────────────────────────
case "${BUILD_ARCH}" in
    arm64)  ARCHS="arm64" ;;
    armv7)  ARCHS="armv7" ;;
    all)    ARCHS="arm64 armv7" ;;
    *)      ARCHS="arm64" ;;
esac

ndk_abi() {
    case "$1" in
        arm64) echo "arm64-v8a" ;;
        armv7) echo "armeabi-v7a" ;;
    esac
}

tauri_target() {
    case "$1" in
        arm64) echo "aarch64" ;;
        armv7) echo "armv7" ;;
    esac
}

ndk_sysroot_dir() {
    case "$1" in
        arm64) echo "aarch64-linux-android" ;;
        armv7) echo "arm-linux-androideabi" ;;
    esac
}

# ─── wzp-native standalone cdylib (built with cargo-ndk, not cargo-tauri) ──
# Produces libwzp_native.so which wzp-desktop dlopens at runtime via
# libloading. Split exists because cargo-tauri linker wiring pulls
# bionic private symbols into any cdylib with cc::Build C++, causing
# __init_tcb+4 SIGSEGV. cargo-ndk uses the same linker path as the
# legacy wzp-android crate which works.
JNILIBS_BASE=gen/android/app/src/main/jniLibs

for ARCH in $ARCHS; do
    ABI=$(ndk_abi "$ARCH")
    SYSROOT_DIR=$(ndk_sysroot_dir "$ARCH")
    JNI_ABI_DIR="$JNILIBS_BASE/$ABI"
    mkdir -p "$JNI_ABI_DIR"

    echo ">>> cargo ndk build -p wzp-native --release -t $ABI"
    (
        cd /build/source
        cargo ndk -t "$ABI" -o "desktop/src-tauri/$JNILIBS_BASE" \
            build --release -p wzp-native 2>&1 | tail -10
    )
    if [ -f "$JNI_ABI_DIR/libwzp_native.so" ]; then
        ls -lh "$JNI_ABI_DIR/libwzp_native.so"
    else
        echo ">>> WARNING: libwzp_native.so not produced for $ABI"
    fi

    # ─── libc++_shared.so — required by wzp-native at runtime ────────────
    # wzp-native/build.rs uses cpp_link_stdlib(Some("c++_shared")) which adds
    # a NEEDED entry for libc++_shared.so to libwzp_native.so. cargo-ndk does
    # NOT copy the actual libc++_shared.so into jniLibs, so unless we copy it
    # explicitly, the APK ships without it and the Android dynamic linker
    # fails the dlopen with "library libc++_shared.so not found" at runtime.
    if [ ! -f "$JNI_ABI_DIR/libc++_shared.so" ]; then
        echo ">>> libc++_shared.so missing for $ABI, copying from NDK..."
        NDK_LIBCXX=$(find "$ANDROID_NDK_HOME" -name "libc++_shared.so" -path "*/${SYSROOT_DIR}/*" | head -1)
        if [ -n "$NDK_LIBCXX" ]; then
            cp "$NDK_LIBCXX" "$JNI_ABI_DIR/"
            ls -lh "$JNI_ABI_DIR/libc++_shared.so"
        else
            echo ">>> ERROR: libc++_shared.so not found in NDK for $ABI — APK will crash at dlopen time"
            exit 1
        fi
    fi
done

# ─── Build per-arch APKs ────────────────────────────────────────────────
# When building for a single arch, only that arch jniLibs dir exists so
# the APK is naturally single-arch and smaller.
# When building --arch all, we produce SEPARATE per-arch APKs by:
#   1. Building each target individually with cargo tauri android build
#   2. Temporarily hiding the other arch jniLibs so the APK only contains one
# This keeps APKs small (~15-20MB instead of ~30-40MB for universal).

APK_OUTPUT_DIR="/build/source/target/apk-output"
mkdir -p "$APK_OUTPUT_DIR"

for ARCH in $ARCHS; do
    TARGET=$(tauri_target "$ARCH")
    ABI=$(ndk_abi "$ARCH")

    # If building all, temporarily hide other arches to get single-arch APK
    if [ "${BUILD_ARCH}" = "all" ]; then
        for OTHER_ARCH in $ARCHS; do
            OTHER_ABI=$(ndk_abi "$OTHER_ARCH")
            if [ "$OTHER_ABI" != "$ABI" ] && [ -d "$JNILIBS_BASE/$OTHER_ABI" ]; then
                mv "$JNILIBS_BASE/$OTHER_ABI" "$JNILIBS_BASE/_hide_$OTHER_ABI"
            fi
        done
    fi

    echo ""
    echo ">>> cargo tauri android build ${PROFILE_FLAG} --target $TARGET --apk"
    cargo tauri android build ${PROFILE_FLAG} --target "$TARGET" --apk

    # Copy produced APK with arch suffix
    BUILT_APK=$(find gen/android -name "*.apk" -newer "$APK_OUTPUT_DIR" -type f 2>/dev/null | head -1)
    if [ -z "$BUILT_APK" ]; then
        BUILT_APK=$(find gen/android -name "*.apk" -type f 2>/dev/null | sort -t/ -k1 | tail -1)
    fi
    if [ -n "$BUILT_APK" ]; then
        cp "$BUILT_APK" "$APK_OUTPUT_DIR/wzp-tauri-${ARCH}.apk"
        echo ">>> $ARCH APK: $(ls -lh "$APK_OUTPUT_DIR/wzp-tauri-${ARCH}.apk" | awk "{print \$5}")"
    fi

    # Restore hidden arches
    if [ "${BUILD_ARCH}" = "all" ]; then
        for OTHER_ARCH in $ARCHS; do
            OTHER_ABI=$(ndk_abi "$OTHER_ARCH")
            if [ "$OTHER_ABI" != "$ABI" ] && [ -d "$JNILIBS_BASE/_hide_$OTHER_ABI" ]; then
                mv "$JNILIBS_BASE/_hide_$OTHER_ABI" "$JNILIBS_BASE/$OTHER_ABI"
            fi
        done
    fi
done

echo ""
echo ">>> Build artifacts:"
ls -lh "$APK_OUTPUT_DIR/"*.apk 2>/dev/null || echo "  (none)"
'

# ─── Collect and upload APKs ────────────────────────────────────────────
# target/ is mounted from cache, not source
APK_OUTPUT="$BASE_DIR/data/cache/target/apk-output"
APK_LIST=$(find "$APK_OUTPUT" -name "wzp-tauri-*.apk" -type f 2>/dev/null | sort)

if [ -z "$APK_LIST" ]; then
    LOG_URL=$(upload_to_rustypaste "$LOG_FILE" || echo "")
    if [ -n "$LOG_URL" ]; then
        notify "WZP Tauri Android build [$GIT_HASH]: no APK produced
log: $LOG_URL"
    else
        notify "WZP Tauri Android build [$GIT_HASH]: no APK produced — log upload failed"
    fi
    exit 1
fi

# Upload each APK and collect URLs
NOTIFY_MSG="WZP Tauri Android build OK [$GIT_HASH] ($BUILD_ARCH)"
APK_PATHS=""
for APK in $APK_LIST; do
    APK_NAME=$(basename "$APK")
    APK_SIZE=$(du -h "$APK" | cut -f1)
    RUSTY_URL=$(upload_to_rustypaste "$APK" || echo "")
    if [ -n "$RUSTY_URL" ]; then
        NOTIFY_MSG="$NOTIFY_MSG
$APK_NAME ($APK_SIZE): $RUSTY_URL"
    else
        NOTIFY_MSG="$NOTIFY_MSG
$APK_NAME ($APK_SIZE) — upload skipped"
    fi
    APK_PATHS="$APK_PATHS $APK"
done
notify "$NOTIFY_MSG"

# Print paths so the local script can grab them
for APK in $APK_LIST; do
    echo "APK_REMOTE_PATH=$APK"
done
REMOTE_SCRIPT

ssh_cmd "chmod +x /tmp/wzp-tauri-build.sh"

notify_local "WZP Tauri Android build dispatched (branch=$BRANCH, arch=$BUILD_ARCH, release=$BUILD_RELEASE)"
log "Triggering remote build (branch=$BRANCH, arch=$BUILD_ARCH)..."

# Run; last lines are APK_REMOTE_PATH=...  (one per arch)
REMOTE_OUTPUT=$(ssh_cmd "/tmp/wzp-tauri-build.sh '$BRANCH' '$DO_PULL' '$REBUILD_RUST' '$DO_INIT' '$BUILD_RELEASE' '$BUILD_ARCH'" || true)
echo "$REMOTE_OUTPUT" | tail -60

# Download all produced APKs
APK_REMOTES=$(echo "$REMOTE_OUTPUT" | grep '^APK_REMOTE_PATH=' | cut -d= -f2-)
if [ -z "$APK_REMOTES" ]; then
    log "No APK produced — see ntfy / remote log /tmp/wzp-tauri-build.log"
    exit 1
fi

DOWNLOADED=0
echo "$APK_REMOTES" | while IFS= read -r APK_REMOTE; do
    [ -z "$APK_REMOTE" ] && continue
    APK_NAME=$(basename "$APK_REMOTE")
    log "Downloading $APK_NAME..."
    scp $SSH_OPTS "$REMOTE_HOST:$APK_REMOTE" "$LOCAL_OUTPUT/$APK_NAME"
    echo "  $LOCAL_OUTPUT/$APK_NAME ($(du -h "$LOCAL_OUTPUT/$APK_NAME" | cut -f1))"
    DOWNLOADED=$((DOWNLOADED + 1))
done

log "Done! APKs in $LOCAL_OUTPUT/"
ls -lh "$LOCAL_OUTPUT"/wzp-tauri-*.apk 2>/dev/null || true
