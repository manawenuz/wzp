#!/usr/bin/env bash
# =============================================================================
# WZ Phone — Android APK build script for Debian 12 (Bookworm)
#
# Sets up a complete build environment from scratch and produces a debug APK.
# Idempotent — safe to run multiple times (skips already-installed components).
#
# Tested on: Debian 12 x86_64, cross-compiling to aarch64-linux-android
#
# Why these specific versions:
#
#   cmake 3.25.1 (Debian 12 system package)
#     cmake 3.27+ rewrote Platform/Android-Determine.cmake with bugs:
#     can't find make during cross-compilation, armv7/aarch64 flag conflicts.
#     cmake 3.25 is the last version where Android cross-compilation works
#     without workarounds. Do NOT use pip cmake — it bundles its own modules
#     that have the same bugs.
#
#   NDK 26.1.10909125 (r26b)
#     NDK 27+ ships a newer libc++_shared.so with different scudo allocator
#     defaults. On Android 16 devices with MTE (Memory Tagging Extension)
#     enabled (e.g. Nothing A059), NDK 27's scudo crashes during malloc/calloc.
#     NDK 26.1 is the last stable version for these devices.
#     Matches build.gradle.kts: ndkVersion = "26.1.10909125"
#
#   JDK 17 (openjdk-17-jdk-headless)
#     Gradle 8.5 + AGP 8.2.0 officially support JDK 17.
#     JDK 21 works for compilation but has Gradle daemon compat issues.
#
#   Rust stable (currently 1.94.1)
#     Edition 2024, MSRV 1.85. Stable channel is fine.
#
#   ANDROID_NDK=$ANDROID_NDK_HOME (BOTH must be set)
#     cmake's Android platform module checks ANDROID_NDK (no _HOME suffix).
#     cargo-ndk sets ANDROID_NDK_HOME. Both must point to the same path.
#
# Usage:
#   chmod +x scripts/build-android.sh
#   ./scripts/build-android.sh                    # build from current tree
#   WZP_CLONE=1 ./scripts/build-android.sh        # clone fresh from git
#   WZP_COMMIT=2092245 ./scripts/build-android.sh  # pin to specific commit
#
# Environment variables (all optional):
#   WZP_CLONE       Set to 1 to clone from git instead of using current dir
#   WZP_REPO        Git clone URL        (default: ssh://git@git.manko.yoga:222/manawenuz/wz-phone)
#   WZP_BRANCH      Branch to checkout   (default: feat/android-voip-client)
#   WZP_COMMIT      Commit to pin to     (default: HEAD)
#   WZP_WORKDIR     Build directory       (default: /tmp/wzp-build)
#   ANDROID_API     SDK platform level    (default: 34)
#   NDK_VERSION     NDK version string    (default: 26.1.10909125)
# =============================================================================
set -euo pipefail

# ---------------------------------------------------------------------------
# Configuration
# ---------------------------------------------------------------------------
CLONE="${WZP_CLONE:-0}"
REPO="${WZP_REPO:-ssh://git@git.manko.yoga:222/manawenuz/wz-phone}"
BRANCH="${WZP_BRANCH:-feat/android-voip-client}"
COMMIT="${WZP_COMMIT:-}"
WORKDIR="${WZP_WORKDIR:-/tmp/wzp-build}"
ANDROID_API="${ANDROID_API:-34}"
NDK_VERSION="${NDK_VERSION:-26.1.10909125}"

ANDROID_HOME="${ANDROID_HOME:-$HOME/android-sdk}"
ANDROID_NDK_HOME="$ANDROID_HOME/ndk/$NDK_VERSION"
# cmake checks ANDROID_NDK (not _HOME) — both must be set
ANDROID_NDK="$ANDROID_NDK_HOME"
JAVA_HOME="/usr/lib/jvm/java-17-openjdk-$(dpkg --print-architecture)"
CMDLINE_TOOLS_URL="https://dl.google.com/android/repository/commandlinetools-linux-11076708_latest.zip"

export ANDROID_HOME ANDROID_NDK_HOME ANDROID_NDK JAVA_HOME
export PATH="$JAVA_HOME/bin:$ANDROID_HOME/cmdline-tools/latest/bin:$ANDROID_HOME/platform-tools:$HOME/.cargo/bin:$PATH"

log() { echo -e "\n\033[1;36m>>> $*\033[0m"; }
err() { echo -e "\033[1;31mERROR: $*\033[0m" >&2; exit 1; }

# ---------------------------------------------------------------------------
# Step 1: System packages (cmake 3.25, JDK 17, make, git, etc.)
# ---------------------------------------------------------------------------
log "Installing system packages"
export DEBIAN_FRONTEND=noninteractive
apt-get update -qq
apt-get install -y -qq \
    build-essential \
    cmake \
    curl \
    git \
    libssl-dev \
    pkg-config \
    unzip \
    wget \
    zip \
    openjdk-17-jdk-headless \
    2>/dev/null

# Verify critical versions
log "Verifying build environment"
echo "  cmake:  $(cmake --version | head -1)"
echo "  java:   $(java -version 2>&1 | head -1)"
echo "  make:   $(make --version | head -1)"

CMAKE_MAJOR=$(cmake --version | head -1 | grep -oP '\d+' | head -1)
CMAKE_MINOR=$(cmake --version | head -1 | grep -oP '\d+' | sed -n '2p')
if [ "$CMAKE_MAJOR" -gt 3 ] || { [ "$CMAKE_MAJOR" -eq 3 ] && [ "$CMAKE_MINOR" -gt 26 ]; }; then
    err "cmake $(cmake --version | head -1) is too new! Need cmake <= 3.26.x (Debian 12 ships 3.25.1). cmake 3.27+ has Android cross-compilation bugs."
fi

# ---------------------------------------------------------------------------
# Step 2: Rust toolchain
# ---------------------------------------------------------------------------
log "Setting up Rust toolchain"
if ! command -v rustup &>/dev/null; then
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable
    source "$HOME/.cargo/env"
fi
rustup default stable
rustup target add aarch64-linux-android
echo "  rustc:  $(rustc --version)"
echo "  cargo:  $(cargo --version)"

if ! command -v cargo-ndk &>/dev/null; then
    log "Installing cargo-ndk"
    cargo install cargo-ndk
fi
echo "  ndk:    $(cargo ndk --version)"

# ---------------------------------------------------------------------------
# Step 3: Android SDK + NDK 26.1
# ---------------------------------------------------------------------------
log "Setting up Android SDK + NDK $NDK_VERSION"
if [ ! -f "$ANDROID_HOME/cmdline-tools/latest/bin/sdkmanager" ]; then
    log "Downloading Android command-line tools"
    mkdir -p "$ANDROID_HOME/cmdline-tools"
    TMPZIP=$(mktemp /tmp/cmdline-tools-XXXXX.zip)
    wget -q -O "$TMPZIP" "$CMDLINE_TOOLS_URL"
    unzip -qo "$TMPZIP" -d "$ANDROID_HOME/cmdline-tools"
    mv "$ANDROID_HOME/cmdline-tools/cmdline-tools" "$ANDROID_HOME/cmdline-tools/latest" 2>/dev/null || true
    rm -f "$TMPZIP"
fi

yes | sdkmanager --licenses >/dev/null 2>&1 || true

if [ ! -d "$ANDROID_NDK_HOME" ]; then
    log "Installing NDK $NDK_VERSION (this takes a few minutes)"
    sdkmanager --install \
        "platforms;android-${ANDROID_API}" \
        "build-tools;${ANDROID_API}.0.0" \
        "ndk;${NDK_VERSION}" \
        "platform-tools" \
        2>&1 | grep -v "^\[" || true
fi

[ -d "$ANDROID_NDK_HOME" ] || err "NDK not found at $ANDROID_NDK_HOME"
echo "  NDK:    $ANDROID_NDK_HOME"
echo "  SDK:    $ANDROID_HOME"

# ---------------------------------------------------------------------------
# Step 4: Source code
# ---------------------------------------------------------------------------
if [ "$CLONE" = "1" ]; then
    log "Cloning $REPO (branch: $BRANCH)"
    if [ -d "$WORKDIR/.git" ]; then
        cd "$WORKDIR"
        git fetch origin
    else
        rm -rf "$WORKDIR"
        git clone --branch "$BRANCH" --recurse-submodules "$REPO" "$WORKDIR"
        cd "$WORKDIR"
    fi
    git checkout "$BRANCH"
    git pull origin "$BRANCH" || true
    git submodule update --init --recursive

    if [ -n "$COMMIT" ]; then
        log "Pinning to commit $COMMIT"
        git checkout "$COMMIT"
    fi
else
    # Use current directory (assume we're in the repo root)
    SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
    WORKDIR="$(cd "$SCRIPT_DIR/.." && pwd)"
    cd "$WORKDIR"
    [ -f "Cargo.toml" ] || err "Not in repo root. Run from repo root or set WZP_CLONE=1"
fi

echo "  HEAD:   $(git log --oneline -1)"

# ---------------------------------------------------------------------------
# Step 5: Build native Rust library (.so)
# ---------------------------------------------------------------------------
log "Building Rust native library (arm64-v8a, release)"
cargo ndk -t arm64-v8a \
    -o "$WORKDIR/android/app/src/main/jniLibs" \
    build --release -p wzp-android

SO="$WORKDIR/android/app/src/main/jniLibs/arm64-v8a/libwzp_android.so"
[ -f "$SO" ] || err ".so not found at $SO"
echo "  Built:  $SO ($(du -h "$SO" | cut -f1))"

# ---------------------------------------------------------------------------
# Step 6: Generate debug keystore (if missing)
# ---------------------------------------------------------------------------
KEYSTORE="$WORKDIR/android/keystore/wzp-debug.jks"
if [ ! -f "$KEYSTORE" ]; then
    log "Generating debug keystore"
    mkdir -p "$(dirname "$KEYSTORE")"
    keytool -genkey -v \
        -keystore "$KEYSTORE" \
        -keyalg RSA -keysize 2048 -validity 10000 \
        -alias wzp-debug \
        -storepass android -keypass android \
        -dname "CN=WZP Debug" 2>&1 | tail -1
fi

# ---------------------------------------------------------------------------
# Step 7: Build Android APK
# ---------------------------------------------------------------------------
log "Building APK (debug)"
cd "$WORKDIR/android"
chmod +x ./gradlew
./gradlew assembleDebug --no-daemon --warning-mode=none

APK=$(find . -name "app-debug*.apk" -path "*/outputs/apk/*" | head -1)
[ -n "$APK" ] || err "APK not found"
APK_ABS="$(cd "$(dirname "$APK")" && pwd)/$(basename "$APK")"

# ---------------------------------------------------------------------------
# Done
# ---------------------------------------------------------------------------
log "Build complete!"
echo ""
echo "  ┌──────────────────────────────────────────────────────────┐"
echo "  │ APK:    $APK_ABS"
echo "  │ Size:   $(du -h "$APK_ABS" | cut -f1)"
echo "  │ SHA256: $(sha256sum "$APK_ABS" | cut -d' ' -f1)"
echo "  └──────────────────────────────────────────────────────────┘"
echo ""
echo "  Install:  adb install -r $APK_ABS"
echo ""
