#!/usr/bin/env bash
set -euo pipefail

# Build WarzonePhone desktop .exe for Windows x86_64 using a temporary
# Hetzner Cloud VPS. Cross-compiles from Linux via `cargo xwin`, which
# auto-downloads the Windows SDK + MSVC CRT the first time it runs.
#
# No Windows machine needed for the build itself — the produced .exe
# still has to be copied to a real Windows host to run (we can only
# verify compile + link here, not runtime).
#
# Prerequisites:
#   - hcloud CLI authenticated
#   - SSH key "wz" registered in Hetzner
#   - Local ssh-agent loaded with an SSH key that can read the
#     git.manko.yoga repo (the script forwards the agent so the VM's
#     git clone uses your identity). Run `ssh-add /Users/manwe/CascadeProjects/wzp`
#     once before invoking this script if you haven't already.
#
# Usage:
#   ./scripts/build-windows-cloud.sh              Full build (create → build → download → destroy)
#   ./scripts/build-windows-cloud.sh --prepare    Create VM and install deps only
#   ./scripts/build-windows-cloud.sh --build      Build on existing VM
#   ./scripts/build-windows-cloud.sh --transfer   Download .exe from VM
#   ./scripts/build-windows-cloud.sh --destroy    Delete the VM
#   ./scripts/build-windows-cloud.sh --all        prepare + build + transfer (VM persists)
#   ./scripts/build-windows-cloud.sh --upload     Re-upload source to existing VM
#
# Environment variables (all optional):
#   WZP_BRANCH       Branch to build      (default: feat/desktop-audio-rewrite)
#   WZP_SERVER_TYPE  Hetzner server type  (default: cx23 — small, cheap, x86)
#   WZP_KEEP_VM      Set to 1 to skip destroy on full build

SSH_KEY_NAME="wz"
SSH_KEY_PATH="/Users/manwe/CascadeProjects/wzp"
SERVER_TYPE="${WZP_SERVER_TYPE:-cx33}"   # cx23 (4GB RAM) OOMs on tauri+rustls cross-compile — bump to cx33 (8GB, 8 vCPU)
IMAGE="ubuntu-24.04"
SERVER_NAME="wzp-windows-builder"
REMOTE_USER="root"
OUTPUT_DIR="target/windows-exe"
PROJECT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
BRANCH="${WZP_BRANCH:-feat/desktop-audio-rewrite}"
KEEP_VM="${WZP_KEEP_VM:-0}"

SSH_OPTS="-o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o ConnectTimeout=10 -o LogLevel=ERROR"

RUST_TARGET="x86_64-pc-windows-msvc"

NTFY_TOPIC="https://ntfy.sh/wzp"
RUSTY_ENV_FILE="$HOME/.wzp/rustypaste.env"

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

log()  { echo -e "\n\033[1;36m>>> $*\033[0m"; }
err()  { echo -e "\033[1;31mERROR: $*\033[0m" >&2; }
die()  {
  err "$@"
  notify "WZP Windows build FAILED — $*"
  # If the user wants to keep the VM alive for debugging (WZP_KEEP_VM=1),
  # don't tear it down on failure — they might want to ssh in and poke at
  # the build state. Only auto-destroy when KEEP_VM is explicitly off.
  if [ "${KEEP_VM:-0}" != "1" ]; then
    do_destroy_quiet
  else
    err "VM kept alive for debugging (WZP_KEEP_VM=1). Destroy with $0 --destroy"
  fi
  exit 1
}

notify() {
  # Fire-and-forget ntfy. Silently ignored if there's no network.
  curl -sf -m 5 -d "$1" "$NTFY_TOPIC" > /dev/null 2>&1 || true
}

# Upload a file to the online rustypaste (paste.dk.manko.yoga), return
# the public URL on stdout. Requires $RUSTY_ENV_FILE to contain
# rusty_address + rusty_auth_token (synced from SepehrHomeserverdk's
# /mnt/storage/manBuilder/.env once; see README).
rustypaste_upload() {
  local file="$1"
  [ -f "$file" ] || { echo ""; return; }
  [ -f "$RUSTY_ENV_FILE" ] || { echo ""; return; }
  # shellcheck disable=SC1090
  source "$RUSTY_ENV_FILE"
  if [ -n "${rusty_address:-}" ] && [ -n "${rusty_auth_token:-}" ]; then
    curl -s -F "file=@$file" -H "Authorization: $rusty_auth_token" "$rusty_address" || echo ""
  else
    echo ""
  fi
}

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
  local existing
  existing=$(hcloud server list -o columns=name -o noheader 2>/dev/null | grep "$SERVER_NAME" | tr -d ' ' || true)
  if [ -n "$existing" ]; then
    log "VM already exists: $existing — reusing"
    do_upload
    return
  fi

  notify "WZP Windows build STARTED ($BRANCH) — spinning up $SERVER_TYPE"
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

  # System packages — cargo-xwin needs llvm/lld; ring needs nasm on
  # Windows; audiopus_sys (libopus) uses cmake + ninja to build for the
  # Windows target; tauri's build.rs needs the frontend dist which needs
  # node+npm.
  log "Installing system packages (llvm, lld, clang, nasm, ninja, node)..."
  ssh_cmd "export DEBIAN_FRONTEND=noninteractive && \
    apt-get update -qq && \
    apt-get install -y -qq \
      build-essential cmake ninja-build curl git pkg-config \
      llvm clang lld nasm \
      libssl-dev ca-certificates \
      unzip wget \
      > /dev/null 2>&1" \
    || die "Failed to install system packages"

  # Node.js 20 via NodeSource
  ssh_cmd "curl -fsSL https://deb.nodesource.com/setup_20.x | bash - > /dev/null 2>&1 && \
    apt-get install -y -qq nodejs > /dev/null 2>&1" \
    || die "Failed to install Node.js"

  echo "  clang: $(ssh_cmd "clang --version | head -1")"
  echo "  node:  $(ssh_cmd "node --version")"
  echo "  npm:   $(ssh_cmd "npm --version")"

  # Rust
  log "Installing Rust toolchain + target $RUST_TARGET..."
  ssh_cmd "curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable > /dev/null 2>&1" \
    || die "Failed to install Rust"
  ssh_cmd "source \$HOME/.cargo/env && rustup target add $RUST_TARGET > /dev/null 2>&1" \
    || die "Failed to add Windows target"
  echo "  rust:  $(ssh_cmd "source \$HOME/.cargo/env && rustc --version")"

  # cargo-xwin — the cross compiler glue that fetches Windows SDK + CRT
  # on demand and shims cc/lld to produce PE/COFF output. The Microsoft
  # license is auto-accepted via XWIN_ACCEPT_LICENSE=1 below (current
  # cargo-xwin removed the --accept-license CLI flag in favour of the
  # env var; --dry-run just prints what it would do).
  log "Installing cargo-xwin..."
  ssh_cmd "source \$HOME/.cargo/env && cargo install cargo-xwin > /dev/null 2>&1" \
    || die "Failed to install cargo-xwin"
  echo "  cargo-xwin: $(ssh_cmd "source \$HOME/.cargo/env && cargo xwin --version 2>&1 | head -1")"

  # Make the license-accept env var persist across later ssh_cmd calls so
  # `cargo xwin build` in do_build() doesn't prompt interactively.
  ssh_cmd "echo 'export XWIN_ACCEPT_LICENSE=1' >> \$HOME/.bashrc"

  # Do the source upload + git clone (agent-forwarded) here.
  do_upload

  log "VM ready!"
  echo "  IP:  $ip"
  echo "  SSH: ssh -A -i $SSH_KEY_PATH root@$ip"
}

# ---------------------------------------------------------------------------
# --upload: Clone the repo on the VM (not rsync — the branch we want
# lives in a separate worktree, and cloning from git is simpler + reuses
# whatever SSH identity the calling shell has loaded in its agent).
# ---------------------------------------------------------------------------

GIT_REPO="ssh://git@git.manko.yoga:222/manawenuz/wz-phone.git"

do_upload() {
  log "Cloning wz-phone on VM (branch $BRANCH, agent-forwarded)..."
  local ip
  ip=$(get_vm_ip)
  [ -n "$ip" ] || die "No VM found."

  # Accept the git host key once so `git clone` doesn't hang asking.
  ssh_cmd "mkdir -p \$HOME/.ssh && \
    ssh-keyscan -p 222 -t rsa,ecdsa,ed25519 git.manko.yoga >> \$HOME/.ssh/known_hosts 2>/dev/null"

  # Fresh clone each run — cheap on a short-lived builder VM, avoids
  # stale state if the branch was force-pushed. --recurse-submodules so
  # deps/featherchat (which has the warzone-protocol workspace member)
  # comes along for the ride.
  ssh_cmd "rm -rf /root/wzp-build && \
    git clone --depth 1 --branch $BRANCH --recurse-submodules --shallow-submodules $GIT_REPO /root/wzp-build 2>&1 | tail -5" \
    || die "git clone failed — is your ssh-agent loaded with a key that can read git.manko.yoga?"

  echo "  Cloned $BRANCH into /root/wzp-build (with submodules)"
}

# ---------------------------------------------------------------------------
# --build: Build frontend + cross-compile wzp-desktop.exe
# ---------------------------------------------------------------------------

do_build() {
  log "Building frontend (vite)..."
  ssh_cmd "cd /root/wzp-build/desktop && \
    npm install --silent 2>&1 | tail -3 && \
    npm run build 2>&1 | tail -5" \
    || die "Frontend build failed"

  log "Cross-compiling wzp-desktop.exe ($RUST_TARGET) via cargo-xwin..."
  # XWIN_ACCEPT_LICENSE=1 is required by recent cargo-xwin for headless
  # runs; --cross-compiler clang-cl picks the system clang shipped by the
  # apt install step in do_prepare.
  ssh_cmd "source \$HOME/.cargo/env && \
    export XWIN_ACCEPT_LICENSE=1 && \
    cd /root/wzp-build/desktop/src-tauri && \
    cargo xwin build --release --target $RUST_TARGET --bin wzp-desktop 2>&1 | tail -30" \
    || die "Windows cross-compile failed"

  ssh_cmd "[ -f /root/wzp-build/target/$RUST_TARGET/release/wzp-desktop.exe ]" \
    || die "wzp-desktop.exe not found after build"

  local exe_size
  exe_size=$(ssh_cmd "du -h /root/wzp-build/target/$RUST_TARGET/release/wzp-desktop.exe | cut -f1")
  echo "  .exe: $exe_size"

  local git_hash
  git_hash=$(ssh_cmd "cd /root/wzp-build && git rev-parse --short HEAD")
  notify "WZP Windows build OK [$git_hash] ($exe_size)"
  export WZP_BUILD_GIT_HASH="$git_hash"
  export WZP_BUILD_SIZE="$exe_size"
}

# ---------------------------------------------------------------------------
# --transfer: Download the .exe to local machine
# ---------------------------------------------------------------------------

do_transfer() {
  log "Downloading wzp-desktop.exe..."
  mkdir -p "$OUTPUT_DIR"

  scp_down "/root/wzp-build/target/$RUST_TARGET/release/wzp-desktop.exe" "$OUTPUT_DIR/wzp-desktop.exe"
  local local_size
  local_size=$(du -h "$OUTPUT_DIR/wzp-desktop.exe" | cut -f1)
  echo "  $OUTPUT_DIR/wzp-desktop.exe ($local_size)"

  # Upload to online rustypaste and notify with the URL.
  log "Uploading to rustypaste..."
  local url
  url=$(rustypaste_upload "$OUTPUT_DIR/wzp-desktop.exe" || echo "")
  if [ -n "$url" ]; then
    echo "  $url"
    local hash="${WZP_BUILD_GIT_HASH:-?}"
    notify "WZP Windows build ready [$hash] ($local_size)
$url"
  else
    echo "  (rustypaste upload skipped — no creds in $RUSTY_ENV_FILE)"
    notify "WZP Windows build transferred ($local_size) — rustypaste upload skipped"
  fi

  log "Transfer complete!"
  echo ""
  echo "  Copy to a real Windows x86_64 host and double-click to run."
  echo "  WebView2 runtime is required on Windows 10 (ships with Win 11)."
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
  trap 'err "Build failed!"; [ "${KEEP_VM:-0}" = "1" ] || do_destroy_quiet; exit 1' ERR

  do_prepare
  do_build
  do_transfer

  if [ "$KEEP_VM" = "1" ]; then
    log "VM kept alive (WZP_KEEP_VM=1). Destroy with: $0 --destroy"
  else
    do_destroy
  fi

  log "All done!"
  echo ""
  echo "  ┌────────────────────────────────────────────────┐"
  echo "  │ Windows .exe: $OUTPUT_DIR/wzp-desktop.exe"
  echo "  │"
  echo "  │ Transfer to a Windows x86_64 machine and run."
  echo "  └────────────────────────────────────────────────┘"
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
    echo "  --transfer   Download .exe from VM"
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
