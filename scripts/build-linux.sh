#!/usr/bin/env bash
set -euo pipefail

# Build WarzonePhone Linux x86_64 release binaries using a Hetzner Cloud VPS.
# Prerequisites: hcloud CLI authenticated, SSH key "wz" registered.
#
# Usage: ./scripts/build-linux.sh
#
# Outputs: target/linux-x86_64/wzp-relay, wzp-client, wzp-bench

SSH_KEY_NAME="wz"
SSH_KEY_PATH="/Users/manwe/CascadeProjects/wzp"
SERVER_NAME="wzp-builder-$(date +%s)"
SERVER_TYPE="cx33"
IMAGE="ubuntu-24.04"
REMOTE_USER="root"
OUTPUT_DIR="target/linux-x86_64"

echo "=== WarzonePhone Linux Build ==="

# Ensure server gets deleted on any exit (success or failure)
cleanup() {
  if [ -n "${SERVER_NAME:-}" ]; then
    echo "       Cleaning up server $SERVER_NAME..."
    hcloud server delete "$SERVER_NAME" 2>/dev/null || true
  fi
  rm -f /tmp/wzp-src.tar.gz
}
trap cleanup EXIT

# 1. Create the build server
echo "[1/7] Creating Hetzner server..."
hcloud server create \
  --name "$SERVER_NAME" \
  --type "$SERVER_TYPE" \
  --image "$IMAGE" \
  --ssh-key "$SSH_KEY_NAME" \
  --location fsn1 \
  --quiet

SERVER_IP=$(hcloud server ip "$SERVER_NAME")
echo "       Server: $SERVER_NAME @ $SERVER_IP"

# SSH options: skip host key check, use our key
SSH="ssh -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o ConnectTimeout=10 -i $SSH_KEY_PATH $REMOTE_USER@$SERVER_IP"
SCP="scp -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -i $SSH_KEY_PATH"

# 2. Wait for SSH to come up
echo "[2/7] Waiting for SSH..."
for i in $(seq 1 30); do
  if $SSH "echo ok" &>/dev/null; then
    break
  fi
  sleep 2
done

# 3. Install build dependencies
echo "[3/7] Installing build dependencies..."
$SSH "apt-get update -qq && apt-get install -y -qq build-essential cmake pkg-config libasound2-dev curl git > /dev/null 2>&1"

# 4. Install Rust
echo "[4/7] Installing Rust..."
$SSH "curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable > /dev/null 2>&1"

# 5. Upload source code
echo "[5/7] Uploading source code..."
# Create a tarball excluding target/ and .git/
tar czf /tmp/wzp-src.tar.gz \
  --exclude='target' \
  --exclude='.git' \
  --exclude='.claude' \
  -C /Users/manwe/CascadeProjects/warzonePhone .

$SCP /tmp/wzp-src.tar.gz "$REMOTE_USER@$SERVER_IP:/root/wzp-src.tar.gz"
$SSH "mkdir -p /root/warzonePhone && tar xzf /root/wzp-src.tar.gz -C /root/warzonePhone"

# 6. Build release binaries (headless + audio variants)
echo "[6/8] Building all binaries..."
$SSH "source ~/.cargo/env && cd /root/warzonePhone && cargo build --release --bin wzp-relay --bin wzp-client --bin wzp-bench --bin wzp-web 2>&1" | tail -3

echo "[7/8] Building audio-enabled client..."
$SSH "source ~/.cargo/env && cd /root/warzonePhone && cargo build --release --bin wzp-client --features audio 2>&1" | tail -3
$SSH "cp /root/warzonePhone/target/release/wzp-client /root/warzonePhone/target/release/wzp-client-audio"
$SSH "source ~/.cargo/env && cd /root/warzonePhone && cargo build --release --bin wzp-client 2>&1" | tail -1

# 8. Download binaries + static files
echo "[8/8] Downloading binaries..."
mkdir -p "$OUTPUT_DIR/static"
$SCP "$REMOTE_USER@$SERVER_IP:/root/warzonePhone/target/release/wzp-relay" "$OUTPUT_DIR/wzp-relay"
$SCP "$REMOTE_USER@$SERVER_IP:/root/warzonePhone/target/release/wzp-client" "$OUTPUT_DIR/wzp-client"
$SCP "$REMOTE_USER@$SERVER_IP:/root/warzonePhone/target/release/wzp-client-audio" "$OUTPUT_DIR/wzp-client-audio"
$SCP "$REMOTE_USER@$SERVER_IP:/root/warzonePhone/target/release/wzp-bench" "$OUTPUT_DIR/wzp-bench"
$SCP "$REMOTE_USER@$SERVER_IP:/root/warzonePhone/target/release/wzp-web" "$OUTPUT_DIR/wzp-web"
$SCP "$REMOTE_USER@$SERVER_IP:/root/warzonePhone/crates/wzp-web/static/index.html" "$OUTPUT_DIR/static/index.html"

# Show results (server is deleted by EXIT trap)
echo ""
echo "=== Build Complete ==="
ls -lh "$OUTPUT_DIR"/wzp-*
echo ""
echo "Binaries:"
echo "  wzp-relay        — relay daemon"
echo "  wzp-client       — headless client (--send-tone, --record)"
echo "  wzp-client-audio — client with mic/speakers (needs libasound2)"
echo "  wzp-web          — web bridge (serve with static/ folder)"
echo "  wzp-bench        — benchmarks"
echo "  static/          — web UI files"
echo ""
echo "Deploy with:"
echo "  scp $OUTPUT_DIR/wzp-* user@server:~/wzp/"
