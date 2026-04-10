#!/usr/bin/env bash
# =============================================================================
# Prepare a Linux Mint / Debian / Ubuntu x86_64 host as a full WarzonePhone
# Android build environment. Installs everything the docker wzp-android-builder
# image has, but directly on the host — so we can iterate locally without
# docker layer caching, see real linker output, run gdbserver, etc.
#
# Target host: root@172.16.81.192 (Linux Mint on the LAN)
#
# Usage (from the macOS workstation):
#   scp scripts/prep-linux-mint.sh root@172.16.81.192:/tmp/
#   ssh root@172.16.81.192 'nohup bash /tmp/prep-linux-mint.sh > /var/log/wzp-prep.log 2>&1 &'
#
# The script is idempotent: safe to re-run if a step fails. Each stage tests
# for its target before doing work. Progress + completion is pinged to
# ntfy.sh/wzp so we can track it from the phone.
#
# On success the host has:
#   - JDK 17
#   - Android SDK (cmdline-tools + platforms 34/36, build-tools 34/35, NDK 26.1)
#   - Node.js 20 LTS + npm
#   - Rust stable + aarch64/armv7/i686/x86_64 android targets
#   - cargo-ndk + cargo tauri-cli 2.x
#   - /opt/wzp/warzonePhone  (cloned workspace checkout on feat/desktop-audio-rewrite)
#
# Everything lives under /opt/android-sdk and /opt/wzp so nothing leaks into $HOME.
# =============================================================================
set -euo pipefail

NTFY_TOPIC="https://ntfy.sh/wzp"
NDK_VERSION="26.1.10909125"
ANDROID_API=34
ANDROID_API_TAURI=36
BUILD_TOOLS_TAURI="35.0.0"
ANDROID_HOME=/opt/android-sdk
WZP_DIR=/opt/wzp
GIT_REPO="ssh://git@git.manko.yoga:222/manawenuz/wz-phone.git"
GIT_BRANCH="feat/desktop-audio-rewrite"

export DEBIAN_FRONTEND=noninteractive
export ANDROID_HOME ANDROID_NDK_HOME="$ANDROID_HOME/ndk/$NDK_VERSION"
export NDK_HOME="$ANDROID_NDK_HOME"
export PATH="$ANDROID_HOME/cmdline-tools/latest/bin:$ANDROID_HOME/platform-tools:/root/.cargo/bin:$PATH"

notify() { curl -s -d "$1" "$NTFY_TOPIC" > /dev/null 2>&1 || true; }
log()    { echo -e "\n\033[1;36m[prep-linux-mint]\033[0m $*"; }
die()    { notify "wzp prep-linux-mint FAILED: $1"; echo "FATAL: $1" >&2; exit 1; }

trap 'die "line $LINENO"' ERR

notify "wzp prep-linux-mint STARTED on $(hostname) ($(whoami))"

# ─── 1. Base packages ────────────────────────────────────────────────────────
log "Installing base packages..."
apt-get update -qq
apt-get install -y --no-install-recommends \
    build-essential \
    ca-certificates \
    cmake \
    curl \
    file \
    git \
    libasound2-dev \
    libc6-dev \
    libssl-dev \
    openjdk-17-jdk-headless \
    pkg-config \
    unzip \
    wget \
    xz-utils \
    zip

# ─── 2. Android SDK + NDK ────────────────────────────────────────────────────
if [ ! -x "$ANDROID_HOME/cmdline-tools/latest/bin/sdkmanager" ]; then
    log "Installing Android cmdline-tools..."
    mkdir -p "$ANDROID_HOME/cmdline-tools"
    cd /tmp
    wget -q https://dl.google.com/android/repository/commandlinetools-linux-11076708_latest.zip -O cmdtools.zip
    unzip -qo cmdtools.zip -d "$ANDROID_HOME/cmdline-tools"
    mv "$ANDROID_HOME/cmdline-tools/cmdline-tools" "$ANDROID_HOME/cmdline-tools/latest"
    rm cmdtools.zip
else
    log "cmdline-tools already installed"
fi

if [ ! -d "$ANDROID_HOME/ndk/$NDK_VERSION" ] || \
   [ ! -d "$ANDROID_HOME/platforms/android-$ANDROID_API" ] || \
   [ ! -d "$ANDROID_HOME/platforms/android-$ANDROID_API_TAURI" ]; then
    log "Installing Android platforms + NDK $NDK_VERSION..."
    yes | "$ANDROID_HOME/cmdline-tools/latest/bin/sdkmanager" --licenses > /dev/null 2>&1 || true
    "$ANDROID_HOME/cmdline-tools/latest/bin/sdkmanager" --install \
        "platforms;android-$ANDROID_API" \
        "build-tools;$ANDROID_API.0.0" \
        "platforms;android-$ANDROID_API_TAURI" \
        "build-tools;$BUILD_TOOLS_TAURI" \
        "ndk;$NDK_VERSION" \
        "platform-tools" 2>&1 | grep -v '^\[' || true
else
    log "Android SDK components already installed"
fi

# ─── 3. Node.js 20 LTS ───────────────────────────────────────────────────────
if ! command -v node >/dev/null 2>&1 || ! node --version | grep -q "^v20"; then
    log "Installing Node.js 20 LTS..."
    curl -fsSL https://deb.nodesource.com/setup_20.x | bash -
    apt-get install -y --no-install-recommends nodejs
else
    log "Node.js already at $(node --version)"
fi

# ─── 4. Rust + Android targets ───────────────────────────────────────────────
if ! command -v rustup >/dev/null 2>&1; then
    log "Installing rustup..."
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable
fi
. /root/.cargo/env

log "Ensuring Rust android targets + cargo-ndk + cargo-tauri..."
rustup target add \
    aarch64-linux-android \
    armv7-linux-androideabi \
    i686-linux-android \
    x86_64-linux-android
command -v cargo-ndk  >/dev/null 2>&1 || cargo install cargo-ndk
command -v cargo-tauri >/dev/null 2>&1 || cargo install tauri-cli --version "^2.0" --locked

# ─── 5. Clone the workspace ──────────────────────────────────────────────────
mkdir -p "$WZP_DIR"
cd "$WZP_DIR"
if [ -d warzonePhone/.git ]; then
    log "Pulling latest on $GIT_BRANCH..."
    cd warzonePhone
    git fetch origin || true
    git checkout "$GIT_BRANCH" 2>/dev/null || git checkout -b "$GIT_BRANCH" "origin/$GIT_BRANCH"
    git reset --hard "origin/$GIT_BRANCH" || true
else
    log "Cloning warzonePhone from $GIT_REPO..."
    # The public repo URL needs ssh keys; if unavailable, skip and let the user sort it later
    if git clone --branch "$GIT_BRANCH" "$GIT_REPO" warzonePhone 2>/dev/null; then
        log "  cloned ok"
    else
        log "  clone failed (no SSH keys for $GIT_REPO — skipping, user will rsync)"
    fi
fi

# ─── 6. Persistent env for the user ──────────────────────────────────────────
cat > /etc/profile.d/wzp-android.sh <<ENVEOF
export ANDROID_HOME=$ANDROID_HOME
export ANDROID_NDK_HOME=$ANDROID_HOME/ndk/$NDK_VERSION
export NDK_HOME=\$ANDROID_NDK_HOME
export PATH=\$ANDROID_HOME/cmdline-tools/latest/bin:\$ANDROID_HOME/platform-tools:/root/.cargo/bin:\$PATH
ENVEOF
chmod 644 /etc/profile.d/wzp-android.sh

# ─── 7. Sanity summary ───────────────────────────────────────────────────────
log "Sanity checks:"
echo "  java:       $(java -version 2>&1 | head -1)"
echo "  node:       $(node --version)"
echo "  npm:        $(npm --version)"
echo "  rustc:      $(rustc --version)"
echo "  cargo-ndk:  $(cargo ndk --version 2>&1 | head -1)"
echo "  cargo-tauri:$(cargo tauri --version 2>&1 | head -1)"
echo "  NDK dir:    $ANDROID_NDK_HOME"
echo "  WZP dir:    $WZP_DIR/warzonePhone"

notify "wzp prep-linux-mint DONE on $(hostname) — ready at /opt/wzp/warzonePhone"
log "All done."
