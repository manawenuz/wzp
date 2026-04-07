#!/usr/bin/env bash
set -euo pipefail

# =============================================================================
# WZ Phone — Android APK build via Docker on remote host
#
# Replaces Hetzner Cloud VMs with a Docker container on SepehrHomeserverdk.
# Persistent storage at /mnt/storage/manBuilder/data/{source,cache,keystore}.
# Uploads APKs to rustypaste, then SCPs them back locally.
#
# Prerequisites:
#   - SSH config has "SepehrHomeserverdk" host entry
#   - SSH agent running with keys for both remote host and git.manko.yoga
#   - Docker installed on remote host
#   - /mnt/storage/manBuilder/.env with rusty_address and rusty_auth_token
#
# Usage:
#   ./scripts/build-android-docker.sh              Full: prepare+pull+build+upload+transfer
#   ./scripts/build-android-docker.sh --prepare    Build Docker image + sync keystores
#   ./scripts/build-android-docker.sh --pull       Clone/update source from Gitea
#   ./scripts/build-android-docker.sh --build      Build debug APK inside Docker
#   ./scripts/build-android-docker.sh --upload     Upload APKs to rustypaste
#   ./scripts/build-android-docker.sh --transfer   SCP APKs back to local machine
#   ./scripts/build-android-docker.sh --all        pull+build+upload+transfer (image ready)
#
#   Add --release to also build release APK:
#   ./scripts/build-android-docker.sh --build --release
#   ./scripts/build-android-docker.sh --all --release
#   ./scripts/build-android-docker.sh --release          (full pipeline, debug+release)
#
# Environment variables (all optional):
#   WZP_BRANCH   Branch to build (default: feat/android-voip-client)
# =============================================================================

REMOTE_HOST="SepehrHomeserverdk"
BASE_DIR="/mnt/storage/manBuilder"
REPO_URL="ssh://git@git.manko.yoga:222/manawenuz/wz-phone.git"
BRANCH="${WZP_BRANCH:-feat/android-voip-client}"
DOCKER_IMAGE="wzp-android-builder"
LOCAL_OUTPUT_DIR="target/android-apk"
PROJECT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
LOCAL_KEYSTORE_DIR="$PROJECT_DIR/android/keystore"

SSH_OPTS="-o ConnectTimeout=10 -o LogLevel=ERROR -o ServerAliveInterval=15 -o ServerAliveCountMax=4"

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------
log()  { echo -e "\n\033[1;36m>>> $*\033[0m"; }
err()  { echo -e "\033[1;31mERROR: $*\033[0m" >&2; }

ssh_cmd() {
    ssh -A $SSH_OPTS "$REMOTE_HOST" "$@"
}

push_reminder() {
    echo ""
    echo "  ┌──────────────────────────────────────────────────────────────────┐"
    echo "  │  IMPORTANT: Push your changes to origin (Gitea) before build!   │"
    echo "  │                                                                  │"
    echo "  │  The build fetches from:                                         │"
    echo "  │    ssh://git@git.manko.yoga:222/manawenuz/wz-phone.git          │"
    echo "  │                                                                  │"
    echo "  │  Run:  git push origin $BRANCH"
    echo "  └──────────────────────────────────────────────────────────────────┘"
    echo ""
    read -r -p "Press Enter to continue (Ctrl-C to abort)... "
}

# ---------------------------------------------------------------------------
# --prepare: Create remote dirs, build Docker image, sync keystores
# ---------------------------------------------------------------------------
do_prepare() {
    log "Preparing remote environment..."
    ssh_cmd "mkdir -p $BASE_DIR/data/{source,cache/cargo-registry,cache/cargo-git,cache/target,cache/gradle,keystore}"

    # Sync keystores (gitignored — won't exist after clone)
    REMOTE_HAS_KEYSTORE=$(ssh_cmd "[ -f $BASE_DIR/data/keystore/wzp-debug.jks ] && echo yes || echo no")
    if [ "$REMOTE_HAS_KEYSTORE" = "no" ]; then
        if [ -f "$LOCAL_KEYSTORE_DIR/wzp-debug.jks" ]; then
            log "Uploading keystores to remote persistent storage..."
            scp $SSH_OPTS \
                "$LOCAL_KEYSTORE_DIR/wzp-debug.jks" \
                "$LOCAL_KEYSTORE_DIR/wzp-release.jks" \
                "$REMOTE_HOST:$BASE_DIR/data/keystore/"
            echo "  Keystores uploaded to $BASE_DIR/data/keystore/"
        else
            err "No keystores found locally at $LOCAL_KEYSTORE_DIR/"
            err "Build will generate a temporary debug keystore instead."
        fi
    else
        echo "  Keystores already on remote."
    fi

    # Upload Dockerfile from local (always use local version — no git dependency)
    log "Uploading Dockerfile to remote..."
    ssh_cmd "mkdir -p $BASE_DIR/data/source/scripts"
    scp $SSH_OPTS \
        "$PROJECT_DIR/scripts/Dockerfile.android-builder" \
        "$REMOTE_HOST:$BASE_DIR/data/source/scripts/Dockerfile.android-builder"

    # Build Docker image
    log "Building Docker image (Debian 12 + Rust + Android SDK/NDK)..."
    ssh_cmd bash <<IMAGE_EOF
set -euo pipefail
docker build -t "$DOCKER_IMAGE" - < "$BASE_DIR/data/source/scripts/Dockerfile.android-builder"
echo "  Docker image '$DOCKER_IMAGE' ready."
IMAGE_EOF
}

# ---------------------------------------------------------------------------
# --pull: Clone or update source from Gitea
# ---------------------------------------------------------------------------
do_pull() {
    push_reminder

    log "Updating source (branch: $BRANCH)..."
    ssh_cmd bash <<PULL_EOF
set -euo pipefail
mkdir -p "$BASE_DIR/data/source" \
         "$BASE_DIR/data/cache/cargo-registry" \
         "$BASE_DIR/data/cache/cargo-git" \
         "$BASE_DIR/data/cache/target" \
         "$BASE_DIR/data/cache/gradle" \
         "$BASE_DIR/data/keystore"
cd "$BASE_DIR/data/source"
if [ -d .git ]; then
    echo "  Fetching origin..."
    git fetch origin
    git checkout "$BRANCH" 2>/dev/null || git checkout -b "$BRANCH" "origin/$BRANCH"
    git reset --hard "origin/$BRANCH"
else
    echo "  Cloning repo..."
    cd "$BASE_DIR/data"
    rm -rf source
    git clone --branch "$BRANCH" "$REPO_URL" source
    cd source
fi
git submodule update --init || true
echo "  HEAD:   \$(git log --oneline -1)"
echo "  Branch: \$(git branch --show-current)"
PULL_EOF

    # Inject keystores into source tree
    log "Injecting keystores into source tree..."
    ssh_cmd bash <<KS_EOF
set -euo pipefail
mkdir -p "$BASE_DIR/data/source/android/keystore"
if [ -f "$BASE_DIR/data/keystore/wzp-debug.jks" ]; then
    cp "$BASE_DIR/data/keystore/wzp-debug.jks"   "$BASE_DIR/data/source/android/keystore/"
    cp "$BASE_DIR/data/keystore/wzp-release.jks"  "$BASE_DIR/data/source/android/keystore/"
    echo "  Keystores ready (wzp-debug.jks + wzp-release.jks)"
else
    echo "  WARNING: No keystores in persistent storage — build will generate temporary ones"
fi
KS_EOF
}

# ---------------------------------------------------------------------------
# --build: Build APK inside Docker container
#   $1 = "1" to also build release APK (default: debug only)
# ---------------------------------------------------------------------------
do_build() {
    local build_release="${1:-0}"

    if [ "$build_release" = "1" ]; then
        log "Building debug + release APKs inside Docker container..."
    else
        log "Building debug APK inside Docker container..."
    fi

    ssh_cmd bash <<BUILD_EOF
set -euo pipefail

# Ensure uid 1000 can write to mounted volumes
# Use find to only chown files not already 1000:1000, ignore errors on stubborn files
find "$BASE_DIR/data/source" "$BASE_DIR/data/cache" \
    ! -user 1000 -o ! -group 1000 2>/dev/null | \
    xargs -r chown 1000:1000 2>/dev/null || true

docker run --rm \
    --user 1000:1000 \
    -e BUILD_RELEASE="$build_release" \
    -v "$BASE_DIR/data/source:/build/source" \
    -v "$BASE_DIR/data/cache/cargo-registry:/home/builder/.cargo/registry" \
    -v "$BASE_DIR/data/cache/cargo-git:/home/builder/.cargo/git" \
    -v "$BASE_DIR/data/cache/target:/build/source/target" \
    -v "$BASE_DIR/data/cache/gradle:/home/builder/.gradle" \
    "$DOCKER_IMAGE" \
    bash -c '
set -euo pipefail
cd /build/source

echo ">>> Building Rust native library (arm64-v8a, release)..."

# Clean stale jniLibs so cargo-ndk re-copies libc++_shared.so
rm -rf android/app/src/main/jniLibs/arm64-v8a

cargo ndk -t arm64-v8a \
    -o android/app/src/main/jniLibs \
    build --release -p wzp-android 2>&1 | tail -10

[ -f android/app/src/main/jniLibs/arm64-v8a/libwzp_android.so ] || {
    echo "ERROR: libwzp_android.so not found after build"; exit 1;
}
echo "  .so size: \$(du -h android/app/src/main/jniLibs/arm64-v8a/libwzp_android.so | cut -f1)"

# Verify keystores exist (should have been injected by --pull)
if [ -f android/keystore/wzp-debug.jks ] && [ -f android/keystore/wzp-release.jks ]; then
    echo "  Keystores: wzp-debug.jks + wzp-release.jks (from persistent storage)"
else
    echo "WARNING: Keystores missing — generating temporary debug keystore..."
    mkdir -p android/keystore
    keytool -genkey -v \
        -keystore android/keystore/wzp-debug.jks \
        -keyalg RSA -keysize 2048 -validity 10000 \
        -alias wzp-debug -storepass android -keypass android \
        -dname "CN=WZP Debug" 2>&1 | tail -1
    cp android/keystore/wzp-debug.jks android/keystore/wzp-release.jks
fi

cd android
chmod +x ./gradlew

echo ">>> Building debug APK..."
./gradlew assembleDebug --no-daemon --warning-mode=none 2>&1 | tail -5

if [ "\${BUILD_RELEASE}" = "1" ]; then
    echo ">>> Building release APK..."
    ./gradlew assembleRelease --no-daemon --warning-mode=none 2>&1 | tail -5 || \
        echo "  (release build failed — debug APK still available)"
fi

echo ""
echo ">>> Build artifacts:"
find . -name "*.apk" -path "*/outputs/apk/*" -exec ls -lh {} \;
'
BUILD_EOF
}

# ---------------------------------------------------------------------------
# --upload: Upload APKs to rustypaste
# ---------------------------------------------------------------------------
do_upload() {
    log "Uploading APKs to rustypaste..."

    UPLOAD_RESULT=$(ssh_cmd bash <<'UPLOAD_EOF'
set -euo pipefail

BASE_DIR="/mnt/storage/manBuilder"
ENV_FILE="$BASE_DIR/.env"

if [ ! -f "$ENV_FILE" ]; then
    echo "ERROR: $ENV_FILE not found — create it with rusty_address and rusty_auth_token" >&2
    exit 1
fi

source "$ENV_FILE"

if [ -z "${rusty_address:-}" ] || [ -z "${rusty_auth_token:-}" ]; then
    echo "ERROR: rusty_address or rusty_auth_token not set in $ENV_FILE" >&2
    exit 1
fi

upload_apk() {
    local apk="$1" label="$2"
    if [ -f "$apk" ]; then
        local url
        url=$(curl -s -F "file=@$apk" -H "Authorization: $rusty_auth_token" "$rusty_address")
        echo "$label: $url"
    fi
}

DEBUG_APK=$(find "$BASE_DIR/data/source/android" -name "app-debug*.apk" -path "*/outputs/apk/*" 2>/dev/null | head -1)
RELEASE_APK=$(find "$BASE_DIR/data/source/android" -name "app-release*.apk" -path "*/outputs/apk/*" 2>/dev/null | head -1)

upload_apk "${DEBUG_APK:-}" "debug"
upload_apk "${RELEASE_APK:-}" "release"
UPLOAD_EOF
    )

    echo "$UPLOAD_RESULT"
}

# ---------------------------------------------------------------------------
# --transfer: SCP APKs back to local machine
# ---------------------------------------------------------------------------
do_transfer() {
    log "Downloading APKs to local machine..."

    mkdir -p "$LOCAL_OUTPUT_DIR"

    # Debug APK
    DEBUG_REMOTE=$(ssh_cmd "find $BASE_DIR/data/source/android -name 'app-debug*.apk' -path '*/outputs/apk/*' 2>/dev/null | head -1" || true)
    if [ -n "$DEBUG_REMOTE" ]; then
        scp $SSH_OPTS "$REMOTE_HOST:$DEBUG_REMOTE" "$LOCAL_OUTPUT_DIR/wzp-debug.apk"
        echo "  debug:   $LOCAL_OUTPUT_DIR/wzp-debug.apk ($(du -h "$LOCAL_OUTPUT_DIR/wzp-debug.apk" | cut -f1))"
    fi

    # Release APK
    RELEASE_REMOTE=$(ssh_cmd "find $BASE_DIR/data/source/android -name 'app-release*.apk' -path '*/outputs/apk/*' 2>/dev/null | head -1" || true)
    if [ -n "$RELEASE_REMOTE" ]; then
        scp $SSH_OPTS "$REMOTE_HOST:$RELEASE_REMOTE" "$LOCAL_OUTPUT_DIR/wzp-release.apk"
        echo "  release: $LOCAL_OUTPUT_DIR/wzp-release.apk ($(du -h "$LOCAL_OUTPUT_DIR/wzp-release.apk" | cut -f1))"
    fi

    # Also grab the .so
    scp $SSH_OPTS "$REMOTE_HOST:$BASE_DIR/data/source/android/app/src/main/jniLibs/arm64-v8a/libwzp_android.so" \
        "$LOCAL_OUTPUT_DIR/libwzp_android.so" 2>/dev/null \
        && echo "  .so:     $LOCAL_OUTPUT_DIR/libwzp_android.so" || true
}

# ---------------------------------------------------------------------------
# Summary banner
# ---------------------------------------------------------------------------
show_summary() {
    log "All done!"
    echo ""
    echo "  ┌──────────────────────────────────────────────────────────────┐"
    [ -f "$LOCAL_OUTPUT_DIR/wzp-debug.apk" ] && \
    echo "  │ Debug APK:   $LOCAL_OUTPUT_DIR/wzp-debug.apk"
    [ -f "$LOCAL_OUTPUT_DIR/wzp-release.apk" ] && \
    echo "  │ Release APK: $LOCAL_OUTPUT_DIR/wzp-release.apk"
    echo "  │"
    if [ -n "${UPLOAD_RESULT:-}" ]; then
    echo "  │ Rustypaste:"
    echo "$UPLOAD_RESULT" | while read -r line; do
    echo "  │   $line"
    done
    echo "  │"
    fi
    echo "  │ Install: adb install -r $LOCAL_OUTPUT_DIR/wzp-debug.apk"
    echo "  └──────────────────────────────────────────────────────────────┘"
}

# ---------------------------------------------------------------------------
# Parse arguments
# ---------------------------------------------------------------------------
ACTION=""
BUILD_RELEASE=0

for arg in "$@"; do
    case "$arg" in
        --release)  BUILD_RELEASE=1 ;;
        --prepare|--pull|--build|--upload|--transfer|--all)
            if [ -n "$ACTION" ]; then
                err "Multiple actions specified: $ACTION and $arg"
                exit 1
            fi
            ACTION="$arg"
            ;;
        *)
            echo "Usage: $0 [--prepare|--pull|--build|--upload|--transfer|--all] [--release]"
            echo ""
            echo "Actions:"
            echo "  (no action)  Full pipeline: pull → prepare → build → upload → transfer"
            echo "  --prepare    Build Docker image + sync keystores to remote"
            echo "  --pull       Clone/update source from Gitea + inject keystores"
            echo "  --build      Build debug APK inside Docker container"
            echo "  --upload     Upload APKs to rustypaste"
            echo "  --transfer   SCP APKs + .so back to local machine"
            echo "  --all        pull → build → upload → transfer (Docker image ready)"
            echo ""
            echo "Flags:"
            echo "  --release    Also build release APK (default: debug only)"
            echo ""
            echo "Examples:"
            echo "  $0                       # full pipeline, debug only"
            echo "  $0 --release             # full pipeline, debug + release"
            echo "  $0 --build               # debug APK only"
            echo "  $0 --build --release     # debug + release APKs"
            echo "  $0 --all                 # iterate: pull+build+upload+transfer (debug)"
            echo "  $0 --all --release       # iterate with release too"
            echo ""
            echo "Environment:"
            echo "  WZP_BRANCH=$BRANCH"
            exit 1
            ;;
    esac
done

# ---------------------------------------------------------------------------
# Dispatch
# ---------------------------------------------------------------------------
case "${ACTION:-}" in
    --prepare)
        do_prepare
        ;;
    --pull)
        do_pull
        ;;
    --build)
        do_build "$BUILD_RELEASE"
        ;;
    --upload)
        do_upload
        ;;
    --transfer)
        do_transfer
        ;;
    --all)
        do_pull
        do_build "$BUILD_RELEASE"
        do_upload
        do_transfer
        show_summary
        ;;
    "")
        do_pull
        do_prepare
        do_build "$BUILD_RELEASE"
        do_upload
        do_transfer
        show_summary
        ;;
esac
