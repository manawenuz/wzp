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
DROP_SHELL=0
for arg in "$@"; do
    case "$arg" in
        --rust)     REBUILD_RUST=1 ;;
        --pull)     DO_PULL=1 ;;
        --no-pull)  DO_PULL=0 ;;
        --init)     DO_INIT=1 ;;
        --release)  BUILD_RELEASE=1 ;;
        --shell)    DROP_SHELL=1 ;;    # interactive debug shell inside container
        -h|--help)
            sed -n '3,30p' "$0"
            exit 0
            ;;
    esac
done

# ── --shell: drop into an interactive container for fast manual iteration ──
# The container is NOT --rm'd so you can keep hacking between invocations,
# and has the same mounts / env as the build path above so `cargo tauri
# android build ...` just works.
if [ "$DROP_SHELL" = "1" ]; then
    log "Starting interactive shell in wzp-android-builder container..."
    log "  cd /build/source/desktop/src-tauri && cargo tauri android build --debug --target aarch64 --apk"
    log "  (exit the shell with ^D; container will be removed)"
    ssh -t -A $SSH_OPTS "$REMOTE_HOST" "
        set -euo pipefail
        BASE=$BASE_DIR
        # Make sure the source/cache is writable by uid 1000
        sudo chown -R 1000:1000 \$BASE/data/source \$BASE/data/cache 2>/dev/null || true
        docker run --rm -it \
            --user 1000:1000 \
            -v \$BASE/data/source:/build/source \
            -v \$BASE/data/cache/cargo-registry:/home/builder/.cargo/registry \
            -v \$BASE/data/cache/cargo-git:/home/builder/.cargo/git \
            -v \$BASE/data/cache/target:/build/source/target \
            -v \$BASE/data/cache/gradle:/home/builder/.gradle \
            -v \$BASE/data/cache/android-home:/home/builder/.android \
            -w /build/source/desktop/src-tauri \
            wzp-android-builder \
            bash
    "
    exit 0
fi

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
    -e CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER=/opt/android-sdk/ndk/26.1.10909125/toolchains/llvm/prebuilt/linux-x86_64/bin/aarch64-linux-android26-clang \
    -e CARGO_TARGET_ARMV7_LINUX_ANDROIDEABI_LINKER=/opt/android-sdk/ndk/26.1.10909125/toolchains/llvm/prebuilt/linux-x86_64/bin/armv7a-linux-androideabi26-clang \
    -e CARGO_TARGET_X86_64_LINUX_ANDROID_LINKER=/opt/android-sdk/ndk/26.1.10909125/toolchains/llvm/prebuilt/linux-x86_64/bin/x86_64-linux-android26-clang \
    -e CARGO_TARGET_I686_LINUX_ANDROID_LINKER=/opt/android-sdk/ndk/26.1.10909125/toolchains/llvm/prebuilt/linux-x86_64/bin/i686-linux-android26-clang \
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

# ── Post-init patches ────────────────────────────────────────────────────────

# Bump minSdk 24 -> 26. Tauri scaffolds with minSdk=24, which forces cargo to
# use the aarch64-linux-android24-clang linker. That linker pulls a broken
# compiler-rt stub for __init_tcb / pthread_create that SIGSEGVs on first
# thread::spawn inside a .so (static libc init never runs in dlopen-loaded
# libraries). API 26 has working runtime symbols. Oboe also requires API 26+.
BUILD_GRADLE=gen/android/app/build.gradle.kts
if grep -q "minSdk = 24" "$BUILD_GRADLE"; then
    echo ">>> bumping minSdk 24 -> 26 in build.gradle.kts"
    sed -i "s|minSdk = 24|minSdk = 26|" "$BUILD_GRADLE"
fi

MANIFEST=gen/android/app/src/main/AndroidManifest.xml
if ! grep -q "RECORD_AUDIO" "$MANIFEST"; then
    echo ">>> injecting RECORD_AUDIO + MODIFY_AUDIO_SETTINGS into AndroidManifest"
    sed -i "s|<uses-permission android:name=\"android.permission.INTERNET\" />|<uses-permission android:name=\"android.permission.INTERNET\" />\n    <uses-permission android:name=\"android.permission.RECORD_AUDIO\" />\n    <uses-permission android:name=\"android.permission.MODIFY_AUDIO_SETTINGS\" />|" "$MANIFEST"
fi

# Overwrite MainActivity to request the mic permission on launch. Idempotent —
# Tauri re-init would reset it, and we re-write it here on every build.
MAIN_ACTIVITY=gen/android/app/src/main/java/com/wzp/desktop/MainActivity.kt
cat > "$MAIN_ACTIVITY" <<KOTLIN_EOF
package com.wzp.desktop

import android.Manifest
import android.content.pm.PackageManager
import android.os.Bundle
import androidx.activity.enableEdgeToEdge
import androidx.core.app.ActivityCompat

class MainActivity : TauriActivity() {
  override fun onCreate(savedInstanceState: Bundle?) {
    enableEdgeToEdge()
    super.onCreate(savedInstanceState)

    // Auto-request RECORD_AUDIO + MODIFY_AUDIO_SETTINGS on first launch — Oboe
    // capture fails silently without them.
    val needed = arrayOf(
      Manifest.permission.RECORD_AUDIO,
      Manifest.permission.MODIFY_AUDIO_SETTINGS,
    ).filter {
      ActivityCompat.checkSelfPermission(this, it) != PackageManager.PERMISSION_GRANTED
    }.toTypedArray()
    if (needed.isNotEmpty()) {
      ActivityCompat.requestPermissions(this, needed, 1337)
    }
  }
}
KOTLIN_EOF

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
