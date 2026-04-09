#!/usr/bin/env bash
set -euo pipefail

# Build WarzonePhone Android APK using a temporary Hetzner Cloud VPS.
# Creates a VM, builds both debug and release APKs, downloads them, destroys the VM.
#
# Prerequisites: hcloud CLI authenticated, SSH key "wz" registered.
#
# Usage:
#   ./scripts/build-android-cloud.sh              Full build (create → build → download → destroy)
#   ./scripts/build-android-cloud.sh --prepare     Create VM and install deps only
#   ./scripts/build-android-cloud.sh --build       Build on existing VM
#   ./scripts/build-android-cloud.sh --transfer    Download APKs from VM
#   ./scripts/build-android-cloud.sh --destroy     Delete the VM
#   ./scripts/build-android-cloud.sh --all         prepare + build + transfer (VM persists)
#   ./scripts/build-android-cloud.sh --upload      Re-upload source to existing VM
#
# Environment variables (all optional):
#   WZP_BRANCH      Branch to build      (default: feat/android-voip-client)
#   WZP_SERVER_TYPE  Hetzner server type  (default: cx32 — 4 vCPU, 8GB RAM)
#   WZP_KEEP_VM     Set to 1 to skip destroy on full build

SSH_KEY_NAME="wz"
SSH_KEY_PATH="/Users/manwe/CascadeProjects/wzp"
SERVER_TYPE="${WZP_SERVER_TYPE:-cx33}"
IMAGE="ubuntu-24.04"
SERVER_NAME="wzp-android-builder"
REMOTE_USER="root"
OUTPUT_DIR="target/android-apk"
PROJECT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
BRANCH="${WZP_BRANCH:-feat/android-voip-client}"
KEEP_VM="${WZP_KEEP_VM:-0}"

SSH_OPTS="-o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o ConnectTimeout=10 -o LogLevel=ERROR"

# NDK 26.1 — NDK 27 crashes scudo on Android 16 MTE devices
NDK_VERSION="26.1.10909125"
ANDROID_API="34"

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

log()  { echo -e "\n\033[1;36m>>> $*\033[0m"; }
err()  { echo -e "\033[1;31mERROR: $*\033[0m" >&2; }
die()  { err "$@"; do_destroy_quiet; exit 1; }

get_vm_ip() {
  hcloud server list -o columns=name,ipv4 -o noheader 2>/dev/null | grep "$SERVER_NAME" | awk '{print $2}' | tr -d ' '
}

ssh_cmd() {
  local ip
  ip=$(get_vm_ip)
  [ -n "$ip" ] || die "No VM found. Run --prepare first."
  ssh $SSH_OPTS -A -i "$SSH_KEY_PATH" "$REMOTE_USER@$ip" "$@"
}

scp_down() {
  local ip
  ip=$(get_vm_ip)
  [ -n "$ip" ] || die "No VM found."
  scp $SSH_OPTS -i "$SSH_KEY_PATH" "$REMOTE_USER@$ip:$1" "$2"
}

do_destroy_quiet() {
  local name
  name=$(hcloud server list -o columns=name -o noheader 2>/dev/null | grep "$SERVER_NAME" | tr -d ' ' || true)
  if [ -n "$name" ]; then
    echo ""
    err "Cleaning up — destroying VM $name"
    hcloud server delete "$name" 2>/dev/null || true
  fi
}

# ---------------------------------------------------------------------------
# --prepare: Create VM, install all build dependencies
# ---------------------------------------------------------------------------

do_prepare() {
  # Check if VM already exists
  local existing
  existing=$(hcloud server list -o columns=name -o noheader 2>/dev/null | grep "$SERVER_NAME" | tr -d ' ' || true)
  if [ -n "$existing" ]; then
    log "VM already exists: $existing — reusing"
    do_upload
    return
  fi

  log "Creating Hetzner VM ($SERVER_TYPE, $IMAGE)..."
  hcloud server create \
    --name "$SERVER_NAME" \
    --type "$SERVER_TYPE" \
    --image "$IMAGE" \
    --ssh-key "$SSH_KEY_NAME" \
    --location fsn1 \
    --quiet \
    || die "Failed to create VM"

  local ip
  ip=$(get_vm_ip)
  [ -n "$ip" ] || die "VM created but no IP found"
  echo "  VM: $SERVER_NAME @ $ip"

  # Wait for SSH
  log "Waiting for SSH..."
  local ok=0
  for i in $(seq 1 30); do
    if ssh $SSH_OPTS -i "$SSH_KEY_PATH" "$REMOTE_USER@$ip" "echo ok" &>/dev/null; then
      ok=1
      break
    fi
    sleep 2
  done
  [ "$ok" -eq 1 ] || die "SSH timeout after 60s"

  # System packages
  log "Installing system packages (cmake, JDK 17, build tools)..."
  ssh_cmd "export DEBIAN_FRONTEND=noninteractive && \
    apt-get update -qq && \
    apt-get install -y -qq \
      build-essential cmake curl git libssl-dev pkg-config \
      unzip wget zip openjdk-17-jdk-headless \
      > /dev/null 2>&1" \
    || die "Failed to install system packages"

  # Verify cmake version (must be <= 3.30)
  local cmake_ver
  cmake_ver=$(ssh_cmd "cmake --version | head -1")
  echo "  cmake: $cmake_ver"
  echo "  java:  $(ssh_cmd "java -version 2>&1 | head -1")"

  # Rust
  log "Installing Rust toolchain..."
  ssh_cmd "curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable > /dev/null 2>&1" \
    || die "Failed to install Rust"
  ssh_cmd "source \$HOME/.cargo/env && rustup target add aarch64-linux-android > /dev/null 2>&1"
  ssh_cmd "source \$HOME/.cargo/env && cargo install cargo-ndk > /dev/null 2>&1" \
    || die "Failed to install cargo-ndk"
  echo "  rust:  $(ssh_cmd "source \$HOME/.cargo/env && rustc --version")"

  # Android SDK + NDK
  log "Installing Android SDK + NDK $NDK_VERSION..."
  ssh_cmd "export JAVA_HOME=/usr/lib/jvm/java-17-openjdk-amd64 && \
    mkdir -p \$HOME/android-sdk/cmdline-tools && \
    cd /tmp && \
    wget -q https://dl.google.com/android/repository/commandlinetools-linux-11076708_latest.zip -O cmdtools.zip && \
    unzip -qo cmdtools.zip -d \$HOME/android-sdk/cmdline-tools && \
    mv \$HOME/android-sdk/cmdline-tools/cmdline-tools \$HOME/android-sdk/cmdline-tools/latest 2>/dev/null; \
    yes | \$HOME/android-sdk/cmdline-tools/latest/bin/sdkmanager --licenses > /dev/null 2>&1; \
    \$HOME/android-sdk/cmdline-tools/latest/bin/sdkmanager --install \
      'platforms;android-${ANDROID_API}' \
      'build-tools;${ANDROID_API}.0.0' \
      'ndk;${NDK_VERSION}' \
      'platform-tools' \
      2>&1 | grep -v '^\[' > /dev/null" \
    || die "Failed to install Android SDK/NDK"

  ssh_cmd "[ -d \$HOME/android-sdk/ndk/$NDK_VERSION ]" \
    || die "NDK not found after install"
  echo "  NDK:   $NDK_VERSION"

  # Upload source
  do_upload

  log "VM ready!"
  echo "  IP:  $ip"
  echo "  SSH: ssh -A -i $SSH_KEY_PATH root@$ip"
}

# ---------------------------------------------------------------------------
# --upload: Upload source code to VM
# ---------------------------------------------------------------------------

do_upload() {
  log "Uploading source code (rsync)..."
  local ip
  ip=$(get_vm_ip)
  [ -n "$ip" ] || die "No VM found."
  rsync -az --delete \
    --exclude='target' \
    --exclude='.git' \
    --exclude='.claude' \
    --exclude='node_modules' \
    --exclude='dist' \
    --exclude='desktop/src-tauri/gen' \
    -e "ssh $SSH_OPTS -i $SSH_KEY_PATH" \
    "$PROJECT_DIR/" "$REMOTE_USER@$ip:/root/wzp-build/"
  echo "  Source uploaded."
}

# ---------------------------------------------------------------------------
# --build: Build native .so + debug & release APKs
# ---------------------------------------------------------------------------

do_build() {
  log "Building Rust native library (arm64-v8a, release)..."

  # Clean Rust release target to force full rebuild.
  # cargo-ndk only copies libc++_shared.so when it actually links — a partial
  # clean that skips relinking leaves libc++_shared.so missing from jniLibs.
  ssh_cmd "rm -rf /root/wzp-build/target/aarch64-linux-android/release \
    /root/wzp-build/android/app/src/main/jniLibs/arm64-v8a"

  # ANDROID_NDK must be set (not just ANDROID_NDK_HOME) — cmake checks it
  ssh_cmd "source \$HOME/.cargo/env && \
    export ANDROID_HOME=\$HOME/android-sdk && \
    export ANDROID_NDK_HOME=\$ANDROID_HOME/ndk/$NDK_VERSION && \
    export ANDROID_NDK=\$ANDROID_NDK_HOME && \
    cd /root/wzp-build && \
    cargo ndk -t arm64-v8a \
      -o android/app/src/main/jniLibs \
      build --release -p wzp-android 2>&1" | tail -5 \
    || die "Rust native build failed"

  ssh_cmd "[ -f /root/wzp-build/android/app/src/main/jniLibs/arm64-v8a/libwzp_android.so ]" \
    || die "libwzp_android.so not found after build"

  local so_size
  so_size=$(ssh_cmd "du -h /root/wzp-build/android/app/src/main/jniLibs/arm64-v8a/libwzp_android.so | cut -f1")
  echo "  .so: $so_size"

  # Generate debug keystore if missing
  ssh_cmd "[ -f /root/wzp-build/android/keystore/wzp-debug.jks ] || \
    (mkdir -p /root/wzp-build/android/keystore && \
     keytool -genkey -v \
       -keystore /root/wzp-build/android/keystore/wzp-debug.jks \
       -keyalg RSA -keysize 2048 -validity 10000 \
       -alias wzp-debug -storepass android -keypass android \
       -dname 'CN=WZP Debug' > /dev/null 2>&1)"

  # Build debug APK
  log "Building debug APK..."
  ssh_cmd "export JAVA_HOME=/usr/lib/jvm/java-17-openjdk-amd64 && \
    export ANDROID_HOME=\$HOME/android-sdk && \
    cd /root/wzp-build/android && \
    chmod +x ./gradlew && \
    ./gradlew assembleDebug --no-daemon --warning-mode=none 2>&1" | tail -3 \
    || die "Debug APK build failed"

  # Build release APK (uses debug keystore for now)
  log "Building release APK..."
  # Copy debug keystore as release keystore (same password in build.gradle)
  ssh_cmd "cp /root/wzp-build/android/keystore/wzp-debug.jks /root/wzp-build/android/keystore/wzp-release.jks 2>/dev/null; true"
  ssh_cmd "export JAVA_HOME=/usr/lib/jvm/java-17-openjdk-amd64 && \
    export ANDROID_HOME=\$HOME/android-sdk && \
    cd /root/wzp-build/android && \
    ./gradlew assembleRelease --no-daemon --warning-mode=none 2>&1" | tail -3 \
    || echo "  (release APK failed — debug APK still available)"

  log "Build complete!"
  ssh_cmd "find /root/wzp-build/android -name '*.apk' -path '*/outputs/apk/*' -exec ls -lh {} \;"
}

# ---------------------------------------------------------------------------
# --transfer: Download APKs to local machine
# ---------------------------------------------------------------------------

do_transfer() {
  log "Downloading APKs..."
  mkdir -p "$OUTPUT_DIR"

  local ip
  ip=$(get_vm_ip)

  # Debug APK
  local debug_apk
  debug_apk=$(ssh_cmd "find /root/wzp-build/android -name 'app-debug*.apk' -path '*/outputs/apk/*' | head -1")
  if [ -n "$debug_apk" ]; then
    scp_down "$debug_apk" "$OUTPUT_DIR/wzp-debug.apk"
    echo "  debug:   $OUTPUT_DIR/wzp-debug.apk ($(du -h "$OUTPUT_DIR/wzp-debug.apk" | cut -f1))"
  fi

  # Release APK
  local release_apk
  release_apk=$(ssh_cmd "find /root/wzp-build/android -name 'app-release*.apk' -path '*/outputs/apk/*' | head -1" || true)
  if [ -n "$release_apk" ]; then
    scp_down "$release_apk" "$OUTPUT_DIR/wzp-release.apk"
    echo "  release: $OUTPUT_DIR/wzp-release.apk ($(du -h "$OUTPUT_DIR/wzp-release.apk" | cut -f1))"
  fi

  # Also copy the .so for inspection
  scp_down "/root/wzp-build/android/app/src/main/jniLibs/arm64-v8a/libwzp_android.so" "$OUTPUT_DIR/libwzp_android.so"
  echo "  .so:     $OUTPUT_DIR/libwzp_android.so"

  log "Transfer complete!"
  echo ""
  echo "  Install debug:   adb install -r $OUTPUT_DIR/wzp-debug.apk"
  [ -f "$OUTPUT_DIR/wzp-release.apk" ] && echo "  Install release: adb install -r $OUTPUT_DIR/wzp-release.apk"
}

# ---------------------------------------------------------------------------
# --destroy: Delete the VM
# ---------------------------------------------------------------------------

do_destroy() {
  local name
  name=$(hcloud server list -o columns=name -o noheader 2>/dev/null | grep "$SERVER_NAME" | tr -d ' ' || true)
  if [ -z "$name" ]; then
    echo "No VM to destroy."
    return
  fi
  log "Deleting VM: $name"
  hcloud server delete "$name"
  echo "  Done."
}

# ---------------------------------------------------------------------------
# Full build: create → build → transfer → destroy
# ---------------------------------------------------------------------------

do_full() {
  trap 'err "Build failed!"; do_destroy_quiet; exit 1' ERR

  do_prepare

  # Disable trap during build — release APK failure is non-fatal
  trap - ERR
  do_build
  do_transfer
  trap 'err "Build failed!"; do_destroy_quiet; exit 1' ERR

  if [ "$KEEP_VM" = "1" ]; then
    log "VM kept alive (WZP_KEEP_VM=1). Destroy with: $0 --destroy"
  else
    do_destroy
  fi

  log "All done!"
  echo ""
  echo "  ┌──────────────────────────────────────────────────┐"
  echo "  │ Debug APK:   $OUTPUT_DIR/wzp-debug.apk"
  [ -f "$OUTPUT_DIR/wzp-release.apk" ] && \
  echo "  │ Release APK: $OUTPUT_DIR/wzp-release.apk"
  echo "  │"
  echo "  │ Install: adb install -r $OUTPUT_DIR/wzp-debug.apk"
  echo "  └──────────────────────────────────────────────────┘"
}

# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------

case "${1:-}" in
  --prepare)  do_prepare ;;
  --build)    do_build ;;
  --transfer) do_transfer ;;
  --destroy)  do_destroy ;;
  --upload)   do_upload ;;
  --all)
    do_prepare
    do_build
    do_transfer
    log "VM still running. Destroy with: $0 --destroy"
    ;;
  "")
    do_full
    ;;
  *)
    echo "Usage: $0 [--prepare|--build|--transfer|--destroy|--all|--upload]"
    echo ""
    echo "  (no args)    Full build: create VM → build → download → destroy VM"
    echo "  --prepare    Create VM and install deps"
    echo "  --build      Build on existing VM"
    echo "  --transfer   Download APKs from VM"
    echo "  --destroy    Delete the VM"
    echo "  --all        prepare + build + transfer (VM persists)"
    echo "  --upload     Re-upload source to existing VM"
    echo ""
    echo "Environment:"
    echo "  WZP_BRANCH=$BRANCH"
    echo "  WZP_SERVER_TYPE=$SERVER_TYPE"
    echo "  WZP_KEEP_VM=$KEEP_VM (set to 1 to skip auto-destroy)"
    exit 1
    ;;
esac
