#!/usr/bin/env bash
set -euo pipefail

# =============================================================================
# WZ Phone — unified build script
#
# Builds Tauri Android APK and/or Linux x86_64 binaries via Docker on a
# remote build server. Uploads artifacts, notifies via ntfy.sh/wzp.
#
# Two servers:
#   PRIMARY (default)  SepehrHomeserverdk    paste.dk.manko.yoga   origin (gitea)
#   ALT (--alt)        manwe@172.16.81.175   paste.tbs.amn.gg      fj (forgejo)
#
# Usage:
#   ./scripts/build.sh                       Android APK (current branch, primary)
#   ./scripts/build.sh --alt                  Android APK on alt server
#   ./scripts/build.sh --linux               Linux binaries only
#   ./scripts/build.sh --all                 Android + Linux
#   ./scripts/build.sh --branch NAME         Override branch
#   ./scripts/build.sh --rust                Force Rust rebuild
#   ./scripts/build.sh --no-pull             Skip git pull
#   ./scripts/build.sh --init                First-time setup (clone + Docker image)
#   ./scripts/build.sh --install             Download APK + adb install locally
#   ./scripts/build.sh --release             Release APK (not debug)
# =============================================================================

NTFY_TOPIC="https://ntfy.sh/wzp"
LOCAL_OUTPUT="target/tauri-android-apk"
SSH_BASE_OPTS="-o ConnectTimeout=15 -o ServerAliveInterval=15 -o ServerAliveCountMax=4 -o LogLevel=ERROR"

# ── Server profiles ─────────────────────────────────────────────────────────
USE_ALT=0
REBUILD_RUST=0
DO_PULL=1
DO_INSTALL=0
DO_INIT=0
BUILD_ANDROID=1
BUILD_LINUX=0
BUILD_RELEASE=0
BRANCH=$(git -C "$(dirname "$0")/.." branch --show-current 2>/dev/null || echo "")

while [ $# -gt 0 ]; do
    case "$1" in
        --alt)      USE_ALT=1 ;;
        --rust)     REBUILD_RUST=1 ;;
        --pull)     DO_PULL=1 ;;
        --no-pull)  DO_PULL=0 ;;
        --install)  DO_INSTALL=1 ;;
        --init)     DO_INIT=1 ;;
        --android)  BUILD_ANDROID=1; BUILD_LINUX=0 ;;
        --linux)    BUILD_ANDROID=0; BUILD_LINUX=1 ;;
        --all)      BUILD_ANDROID=1; BUILD_LINUX=1 ;;
        --release)  BUILD_RELEASE=1 ;;
        --branch)   shift; BRANCH="$1" ;;
        --branch=*) BRANCH="${1#--branch=}" ;;
        -h|--help)  sed -n '3,22p' "$0"; exit 0 ;;
        *)          echo "Unknown arg: $1"; exit 1 ;;
    esac
    shift
done

if [ -z "$BRANCH" ]; then
    echo "ERROR: could not determine target branch (detached HEAD?). Pass --branch NAME."
    exit 1
fi

# ── Select server profile ───────────────────────────────────────────────────
if [ "$USE_ALT" = "1" ]; then
    SERVER_TAG="ALT"
    REMOTE_HOST="manwe@172.16.81.175"
    BASE_DIR="/home/manwe/wzp-builder"
    SSH_OPTS="$SSH_BASE_OPTS"
    GIT_ORIGIN="ssh://git@git.tbs.amn.gg:2222/manawenuz/wzp.git"
    # Alt server uploads directly (no .env file)
    UPLOAD_MODE="direct"
    PASTE_URL="http://paste.tbs.amn.gg"
    PASTE_AUTH="X2j6szIQaoJGaxZjLkpl3A8IX9/mTkDgdhhgyYFcpaU="
else
    SERVER_TAG="PRI"
    REMOTE_HOST="SepehrHomeserverdk"
    BASE_DIR="/mnt/storage/manBuilder"
    SSH_OPTS="-A $SSH_BASE_OPTS"
    GIT_ORIGIN=""  # uses existing origin on the remote
    # Primary server uses .env file for rustypaste credentials
    UPLOAD_MODE="envfile"
    PASTE_URL=""
    PASTE_AUTH=""
fi

TARGETS=""
[ "$BUILD_ANDROID" = 1 ] && TARGETS="Android"
[ "$BUILD_LINUX" = 1 ] && TARGETS="${TARGETS:+$TARGETS + }Linux"
echo "[$SERVER_TAG] branch: $BRANCH | targets: $TARGETS"

log() { echo -e "\033[1;36m>>> $*\033[0m"; }
ssh_cmd() { ssh $SSH_OPTS "$REMOTE_HOST" "$@"; }

# ── First-time setup (--init) ───────────────────────────────────────────────
if [ "$DO_INIT" = "1" ]; then
    log "[$SERVER_TAG] First-time setup..."
    ssh_cmd "mkdir -p $BASE_DIR/data/{source,cache/target,cache/cargo-registry,cache/cargo-git,cache/gradle,cache/android-home,cache-linux/target,cache-linux/cargo-registry,cache-linux/cargo-git}"

    if [ -n "$GIT_ORIGIN" ]; then
        log "Cloning from $GIT_ORIGIN..."
        ssh_cmd "if [ ! -d $BASE_DIR/data/source/.git ]; then git clone $GIT_ORIGIN $BASE_DIR/data/source; else echo 'Repo already cloned'; fi"
    fi

    log "Uploading Dockerfile..."
    cat scripts/Dockerfile.android-builder | ssh_cmd "cat > /tmp/Dockerfile.android-builder"
    log "Building Docker image (10-20 min on first run)..."
    ssh_cmd "cd /tmp && docker build -t wzp-android-builder -f Dockerfile.android-builder . 2>&1 | tail -20"

    log "[$SERVER_TAG] Init done! Run without --init to build."
    exit 0
fi

# ── Upload remote build script ──────────────────────────────────────────────
log "[$SERVER_TAG] Uploading build script..."
ssh_cmd "cat > /tmp/wzp-build.sh" <<REMOTE_SCRIPT
#!/usr/bin/env bash
set -euo pipefail

BASE_DIR="$BASE_DIR"
NTFY_TOPIC="$NTFY_TOPIC"
REBUILD_RUST="$REBUILD_RUST"
DO_PULL="$DO_PULL"
BRANCH="$BRANCH"
BUILD_ANDROID="$BUILD_ANDROID"
BUILD_LINUX="$BUILD_LINUX"
BUILD_RELEASE="$BUILD_RELEASE"
SERVER_TAG="$SERVER_TAG"
UPLOAD_MODE="$UPLOAD_MODE"
PASTE_URL="$PASTE_URL"
PASTE_AUTH="$PASTE_AUTH"

notify() { curl -s -d "\$1" "\$NTFY_TOPIC" > /dev/null 2>&1 || true; }

# Upload a file; print URL on stdout.
upload_file() {
    local file="\$1"
    if [ "\$UPLOAD_MODE" = "direct" ]; then
        curl -s -F "file=@\$file" -H "Authorization: \$PASTE_AUTH" "\$PASTE_URL" || echo ""
    else
        local env_file="\$BASE_DIR/.env"
        [ ! -f "\$env_file" ] && { echo ""; return; }
        source "\$env_file"
        if [ -n "\${rusty_address:-}" ] && [ -n "\${rusty_auth_token:-}" ]; then
            curl -s -F "file=@\$file" -H "Authorization: \$rusty_auth_token" "\$rusty_address" || echo ""
        else
            echo ""
        fi
    fi
}

trap 'notify "WZP [\$SERVER_TAG] build FAILED [\$BRANCH]! Check /tmp/wzp-build.log"' ERR

# ── Pull source ─────────────────────────────────────────────────────────
if [ "\$DO_PULL" = "1" ]; then
    echo ">>> Pulling branch '\$BRANCH' from origin..."
    cd "\$BASE_DIR/data/source"
    git reset --hard HEAD 2>/dev/null || true
    # NOTE: do NOT git clean -fd — it wipes tauri-generated scaffold
    git fetch origin "\$BRANCH" 2>&1 | tail -3
    git checkout "\$BRANCH" 2>/dev/null || git checkout -b "\$BRANCH" "origin/\$BRANCH"
    git reset --hard "origin/\$BRANCH"
    git submodule update --init || true
    echo ">>> HEAD: \$(git rev-parse --short HEAD) — \$(git log -1 --format=%s)"
fi

GIT_HASH=\$(cd "\$BASE_DIR/data/source" && git rev-parse --short HEAD 2>/dev/null || echo unknown)
GIT_MSG=\$(cd "\$BASE_DIR/data/source" && git log -1 --pretty=%s 2>/dev/null | head -c 60 || echo "?")

# ── Clean Rust if requested ─────────────────────────────────────────────
if [ "\$REBUILD_RUST" = "1" ]; then
    echo ">>> Cleaning Rust targets..."
    rm -rf "\$BASE_DIR/data/cache/target/aarch64-linux-android" \
           "\$BASE_DIR/data/cache/target/armv7-linux-androideabi" \
           "\$BASE_DIR/data/cache/target/i686-linux-android" \
           "\$BASE_DIR/data/cache/target/x86_64-linux-android"
    rm -rf "\$BASE_DIR/data/cache-linux/target/release"
fi

# ── Fix perms ───────────────────────────────────────────────────────────
find "\$BASE_DIR/data/source" "\$BASE_DIR/data/cache" \
    ! -user 1000 -o ! -group 1000 2>/dev/null | \
    xargs -r chown 1000:1000 2>/dev/null || true
if [ -d "\$BASE_DIR/data/cache-linux" ]; then
    find "\$BASE_DIR/data/cache-linux" \
        ! -user 1000 -o ! -group 1000 2>/dev/null | \
        xargs -r chown 1000:1000 2>/dev/null || true
fi

# ── Tauri Android APK ──────────────────────────────────────────────────
if [ "\$BUILD_ANDROID" = "1" ]; then
    notify "WZP [\$SERVER_TAG] Tauri Android build STARTED [\$BRANCH @ \$GIT_HASH] — \$GIT_MSG"
    echo ">>> Building Tauri Android APK..."

    PROFILE_FLAG="--debug"
    [ "\$BUILD_RELEASE" = "1" ] && PROFILE_FLAG=""

    mkdir -p "\$BASE_DIR/data/cache/android-home"
    chown 1000:1000 "\$BASE_DIR/data/cache/android-home" 2>/dev/null || true

    docker run --rm --user 1000:1000 \
        -e PROFILE_FLAG="\$PROFILE_FLAG" \
        -v "\$BASE_DIR/data/source:/build/source" \
        -v "\$BASE_DIR/data/cache/cargo-registry:/home/builder/.cargo/registry" \
        -v "\$BASE_DIR/data/cache/cargo-git:/home/builder/.cargo/git" \
        -v "\$BASE_DIR/data/cache/target:/build/source/target" \
        -v "\$BASE_DIR/data/cache/gradle:/home/builder/.gradle" \
        -v "\$BASE_DIR/data/cache/android-home:/home/builder/.android" \
        wzp-android-builder bash -c '
set -euo pipefail
cd /build/source/desktop

echo ">>> npm install"
npm install --silent 2>&1 | tail -5 || npm install 2>&1 | tail -20

cd src-tauri

if [ ! -x gen/android/gradlew ]; then
    echo ">>> cargo tauri android init"
    cargo tauri android init 2>&1 | tail -20
fi

echo ">>> cargo ndk build -p wzp-native --release"
JNI_ABI_DIR=gen/android/app/src/main/jniLibs/arm64-v8a
mkdir -p "\$JNI_ABI_DIR"
(
    cd /build/source
    cargo ndk -t arm64-v8a -o desktop/src-tauri/gen/android/app/src/main/jniLibs \
        build --release -p wzp-native 2>&1 | tail -10
)
[ -f "\$JNI_ABI_DIR/libwzp_native.so" ] && ls -lh "\$JNI_ABI_DIR/libwzp_native.so"

if [ ! -f "\$JNI_ABI_DIR/libc++_shared.so" ]; then
    echo ">>> libc++_shared.so missing, copying from NDK..."
    NDK_LIBCXX=\$(find "\$ANDROID_NDK_HOME" -name "libc++_shared.so" -path "*/aarch64-linux-android/*" | head -1)
    if [ -n "\$NDK_LIBCXX" ]; then
        cp "\$NDK_LIBCXX" "\$JNI_ABI_DIR/"
    else
        echo "ERROR: libc++_shared.so not found in NDK"; exit 1
    fi
fi

echo ">>> cargo tauri android build \${PROFILE_FLAG} --target aarch64 --apk"
cargo tauri android build \${PROFILE_FLAG} --target aarch64 --apk

echo ">>> Build artifacts:"
find gen/android -name "*.apk" -exec ls -lh {} \; 2>/dev/null
echo "APK_BUILT"
'

    echo ">>> Uploading APK..."
    APK=\$(find "\$BASE_DIR/data/source/desktop/src-tauri/gen/android" -name "*.apk" -type f 2>/dev/null | head -1)
    if [ -n "\$APK" ]; then
        APK_SIZE=\$(du -h "\$APK" | cut -f1)
        URL=\$(upload_file "\$APK")
        echo "APK_URL=\$URL"
        notify "WZP [\$SERVER_TAG] Tauri Android OK [\$BRANCH @ \$GIT_HASH] (\$APK_SIZE)
\$URL"
        echo ">>> APK: \$URL (\$APK_SIZE)"
    else
        notify "WZP [\$SERVER_TAG] Tauri Android FAILED [\$BRANCH @ \$GIT_HASH] - no APK"
        echo "ERROR: No APK found"; exit 1
    fi
fi

# ── Linux x86_64 binaries ───────────────────────────────────────────────
if [ "\$BUILD_LINUX" = "1" ]; then
    mkdir -p "\$BASE_DIR/data/cache-linux/target" \
             "\$BASE_DIR/data/cache-linux/cargo-registry" \
             "\$BASE_DIR/data/cache-linux/cargo-git"

    notify "WZP [\$SERVER_TAG] Linux x86_64 build STARTED [\$BRANCH @ \$GIT_HASH]..."
    echo ">>> Building Linux binaries..."

    docker run --rm --user 1000:1000 \
        -v "\$BASE_DIR/data/source:/build/source" \
        -v "\$BASE_DIR/data/cache-linux/cargo-registry:/home/builder/.cargo/registry" \
        -v "\$BASE_DIR/data/cache-linux/cargo-git:/home/builder/.cargo/git" \
        -v "\$BASE_DIR/data/cache-linux/target:/build/source/target" \
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

    echo ">>> Uploading Linux binaries..."
    docker run --rm \
        -v "\$BASE_DIR/data/cache-linux/target:/build/target" \
        wzp-android-builder bash -c \
        "cp /build/target/release/wzp-relay /build/target/release/wzp-client /build/target/release/wzp-client-audio /build/target/release/wzp-web /build/target/release/wzp-bench /tmp/ && tar czf /tmp/wzp-linux-x86_64.tar.gz -C /tmp wzp-relay wzp-client wzp-client-audio wzp-web wzp-bench && cat /tmp/wzp-linux-x86_64.tar.gz" \
        > /tmp/wzp-linux-x86_64.tar.gz

    URL=\$(upload_file /tmp/wzp-linux-x86_64.tar.gz)
    if [ -n "\$URL" ]; then
        echo "LINUX_URL=\$URL"
        notify "WZP [\$SERVER_TAG] Linux x86_64 OK [\$BRANCH @ \$GIT_HASH]
\$URL"
        echo ">>> Linux binaries: \$URL"
    else
        notify "WZP [\$SERVER_TAG] Linux build FAILED - upload error"
        echo "ERROR: Linux upload failed"; exit 1
    fi
fi

echo ">>> All builds complete!"
REMOTE_SCRIPT

ssh_cmd "chmod +x /tmp/wzp-build.sh"

# Run in tmux
log "[$SERVER_TAG] Starting build in tmux (branch: $BRANCH)..."
ssh_cmd "tmux kill-session -t wzp-build 2>/dev/null; true"
ssh_cmd "tmux new-session -d -s wzp-build '/tmp/wzp-build.sh 2>&1 | tee /tmp/wzp-build.log'"

log "[$SERVER_TAG] Build running! Notification on ntfy.sh/wzp when done."
echo ""
echo "  Monitor:  ssh $REMOTE_HOST 'tail -f /tmp/wzp-build.log'"
echo "  Status:   ssh $REMOTE_HOST 'tail -5 /tmp/wzp-build.log'"
echo ""

# Optionally wait and install locally
if [ "$DO_INSTALL" = "1" ]; then
    log "Waiting for build..."
    while true; do
        sleep 15
        if ssh_cmd "grep -q 'APK_URL\|LINUX_URL\|ERROR\|All builds complete' /tmp/wzp-build.log 2>/dev/null"; then
            break
        fi
    done

    URL=$(ssh_cmd "grep APK_URL /tmp/wzp-build.log | tail -1 | cut -d= -f2")
    if [ -n "$URL" ]; then
        log "Downloading APK..."
        mkdir -p "$LOCAL_OUTPUT"
        curl -s -o "$LOCAL_OUTPUT/wzp-tauri.apk" "$URL"
        log "Installing..."
        adb uninstall com.wzp.phone 2>/dev/null || true
        adb install "$LOCAL_OUTPUT/wzp-tauri.apk"
        log "Done!"
    else
        log "No APK URL found in log"
    fi
fi
