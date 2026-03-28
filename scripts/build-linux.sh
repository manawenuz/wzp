#!/usr/bin/env bash
set -euo pipefail

# Build WarzonePhone Linux x86_64 release binaries using a Hetzner Cloud VPS.
# Prerequisites: hcloud CLI authenticated, SSH key "wz" registered.
#
# Usage:
#   ./scripts/build-linux.sh --prepare    Create VM, install deps, upload source
#   ./scripts/build-linux.sh --build      Build release binaries on the VM
#   ./scripts/build-linux.sh --transfer   Download binaries from VM to local
#   ./scripts/build-linux.sh --destroy    Delete the VM
#   ./scripts/build-linux.sh --all        Run prepare + build + transfer (no destroy)
#
# The VM persists between steps so you can iterate on build errors.

SSH_KEY_NAME="wz"
SSH_KEY_PATH="/Users/manwe/CascadeProjects/wzp"
SERVER_TYPE="cx33"
IMAGE="debian-12"
REMOTE_USER="root"
OUTPUT_DIR="target/linux-x86_64"
PROJECT_DIR="/Users/manwe/CascadeProjects/warzonePhone"

SSH_OPTS="-o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o ConnectTimeout=10"

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

get_vm_ip() {
  local ip
  ip=$(hcloud server list -o columns=ipv4 -o noheader 2>/dev/null | tail -1 | tr -d ' ')
  if [ -z "$ip" ]; then
    echo "ERROR: No Hetzner VM found. Run --prepare first." >&2
    exit 1
  fi
  echo "$ip"
}

ssh_cmd() {
  local ip
  ip=$(get_vm_ip)
  ssh $SSH_OPTS -i "$SSH_KEY_PATH" "$REMOTE_USER@$ip" "$@"
}

scp_cmd() {
  local ip
  ip=$(get_vm_ip)
  scp $SSH_OPTS -i "$SSH_KEY_PATH" "$@"
}

get_vm_name() {
  hcloud server list -o columns=name -o noheader 2>/dev/null | tail -1 | tr -d ' '
}

# ---------------------------------------------------------------------------
# --prepare: Create VM, install deps, upload source
# ---------------------------------------------------------------------------

do_prepare() {
  local server_name="wzp-builder"

  # Check if VM already exists
  local existing
  existing=$(hcloud server list -o columns=name -o noheader 2>/dev/null | grep wzp-builder || true)
  if [ -n "$existing" ]; then
    echo "VM already exists: $existing"
    echo "Reusing it. Uploading fresh source..."
    do_upload
    return
  fi

  echo "[1/5] Creating Hetzner VM..."
  hcloud server create \
    --name "$server_name" \
    --type "$SERVER_TYPE" \
    --image "$IMAGE" \
    --ssh-key "$SSH_KEY_NAME" \
    --location fsn1 \
    --quiet

  local ip
  ip=$(get_vm_ip)
  echo "       VM: $server_name @ $ip"

  # Wait for SSH
  echo "[2/5] Waiting for SSH..."
  for i in $(seq 1 30); do
    if ssh $SSH_OPTS -i "$SSH_KEY_PATH" "$REMOTE_USER@$ip" "echo ok" &>/dev/null; then
      break
    fi
    sleep 2
  done

  # Install build dependencies
  echo "[3/5] Installing build dependencies..."
  ssh_cmd "apt-get update -qq && apt-get install -y -qq build-essential cmake pkg-config libasound2-dev libssl-dev curl git libstdc++-12-dev > /dev/null 2>&1"

  # Install Rust
  echo "[4/5] Installing Rust..."
  ssh_cmd "curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable > /dev/null 2>&1"

  # Upload source
  echo "[5/5] Uploading source code..."
  do_upload

  echo ""
  echo "=== VM Ready ==="
  echo "IP: $ip"
  echo "SSH: ssh -i $SSH_KEY_PATH root@$ip"
  echo ""
  echo "Next: ./scripts/build-linux.sh --build"
}

do_upload() {
  echo "       Creating source tarball..."
  tar czf /tmp/wzp-src.tar.gz \
    --exclude='target' \
    --exclude='.git' \
    --exclude='.claude' \
    --exclude='notes' \
    -C "$PROJECT_DIR" . 2>/dev/null

  local ip
  ip=$(get_vm_ip)
  echo "       Uploading to VM..."
  scp $SSH_OPTS -i "$SSH_KEY_PATH" /tmp/wzp-src.tar.gz "$REMOTE_USER@$ip:/root/wzp-src.tar.gz" 2>/dev/null
  ssh_cmd "rm -rf /root/warzonePhone && mkdir -p /root/warzonePhone && tar xzf /root/wzp-src.tar.gz -C /root/warzonePhone" 2>/dev/null
  rm -f /tmp/wzp-src.tar.gz
  echo "       Source uploaded."
}

# ---------------------------------------------------------------------------
# --build: Build release binaries on the VM
# ---------------------------------------------------------------------------

do_build() {
  local ip
  ip=$(get_vm_ip)
  echo "=== Building on $ip ==="

  echo "[1/3] Building relay + client + web..."
  ssh_cmd "source ~/.cargo/env && cd /root/warzonePhone && cargo build --release --bin wzp-relay --bin wzp-client --bin wzp-bench --bin wzp-web 2>&1"

  echo ""
  echo "[2/3] Building audio-enabled client..."
  ssh_cmd "source ~/.cargo/env && cd /root/warzonePhone && cargo build --release --bin wzp-client --features audio 2>&1" | tail -5
  ssh_cmd "cp /root/warzonePhone/target/release/wzp-client /root/warzonePhone/target/release/wzp-client-audio"
  ssh_cmd "source ~/.cargo/env && cd /root/warzonePhone && cargo build --release --bin wzp-client 2>&1" | tail -3

  echo ""
  echo "[3/3] Verifying binaries..."
  ssh_cmd "ls -lh /root/warzonePhone/target/release/wzp-relay /root/warzonePhone/target/release/wzp-client /root/warzonePhone/target/release/wzp-web /root/warzonePhone/target/release/wzp-bench /root/warzonePhone/target/release/wzp-client-audio"

  echo ""
  echo "=== Build Complete ==="
  echo "Next: ./scripts/build-linux.sh --transfer"
}

# ---------------------------------------------------------------------------
# --transfer: Download binaries from VM to local
# ---------------------------------------------------------------------------

do_transfer() {
  local ip
  ip=$(get_vm_ip)
  echo "=== Downloading binaries from $ip ==="

  mkdir -p "$OUTPUT_DIR/static"

  for bin in wzp-relay wzp-client wzp-client-audio wzp-bench wzp-web; do
    echo "  $bin..."
    scp $SSH_OPTS -i "$SSH_KEY_PATH" "$REMOTE_USER@$ip:/root/warzonePhone/target/release/$bin" "$OUTPUT_DIR/$bin" 2>/dev/null
  done

  # Static files for web bridge
  scp $SSH_OPTS -i "$SSH_KEY_PATH" "$REMOTE_USER@$ip:/root/warzonePhone/crates/wzp-web/static/index.html" "$OUTPUT_DIR/static/index.html" 2>/dev/null
  scp $SSH_OPTS -i "$SSH_KEY_PATH" "$REMOTE_USER@$ip:/root/warzonePhone/crates/wzp-web/static/audio-processor.js" "$OUTPUT_DIR/static/audio-processor.js" 2>/dev/null

  echo ""
  echo "=== Transfer Complete ==="
  ls -lh "$OUTPUT_DIR"/wzp-*
  echo ""
  echo "Deploy with:"
  echo "  scp $OUTPUT_DIR/wzp-relay $OUTPUT_DIR/wzp-client user@server:~/wzp/"
}

# ---------------------------------------------------------------------------
# --destroy: Delete the VM
# ---------------------------------------------------------------------------

do_destroy() {
  local name
  name=$(get_vm_name)
  if [ -z "$name" ]; then
    echo "No VM to destroy."
    return
  fi
  echo "Deleting VM: $name"
  hcloud server delete "$name"
  echo "Done."
}

# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------

case "${1:-}" in
  --prepare)
    do_prepare
    ;;
  --build)
    do_build
    ;;
  --transfer)
    do_transfer
    ;;
  --destroy)
    do_destroy
    ;;
  --all)
    do_prepare
    do_build
    do_transfer
    echo ""
    echo "VM is still running. Destroy with: ./scripts/build-linux.sh --destroy"
    ;;
  --upload)
    do_upload
    ;;
  *)
    echo "Usage: $0 {--prepare|--build|--transfer|--destroy|--all|--upload}"
    echo ""
    echo "Steps:"
    echo "  --prepare    Create VM, install deps, upload source"
    echo "  --build      Build release binaries (shows full output)"
    echo "  --transfer   Download binaries to target/linux-x86_64/"
    echo "  --destroy    Delete the VM"
    echo "  --all        prepare + build + transfer (VM persists)"
    echo "  --upload     Re-upload source to existing VM"
    exit 1
    ;;
esac
