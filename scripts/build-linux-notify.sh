#!/usr/bin/env bash
set -euo pipefail

# Build WarzonePhone Linux x86_64 binaries via Hetzner Cloud VPS.
# Fire and forget — notifies via ntfy.sh/wzp with rustypaste URL.
#
# Usage:
#   ./scripts/build-linux-notify.sh              Full: create VM → build → upload → notify → destroy
#   ./scripts/build-linux-notify.sh --keep       Keep VM after build
#   ./scripts/build-linux-notify.sh --pull       Git pull (for existing VM)

SSH_KEY_NAME="wz"
SSH_KEY_PATH="/Users/manwe/CascadeProjects/wzp"
SERVER_TYPE="cx33"
IMAGE="debian-12"
SERVER_NAME="wzp-linux-builder"
NTFY_TOPIC="https://ntfy.sh/wzp"
LOCAL_OUTPUT="target/linux-x86_64"
PROJECT_DIR="$(cd "$(dirname "$0")/.." && pwd)"

SSH_OPTS="-o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o ConnectTimeout=15 -o ServerAliveInterval=15 -o LogLevel=ERROR"

KEEP_VM=0
DO_PULL=0
for arg in "$@"; do
    case "$arg" in
        --keep) KEEP_VM=1 ;;
        --pull) DO_PULL=1 ;;
    esac
done

log() { echo -e "\033[1;36m>>> $*\033[0m"; }
err() { echo -e "\033[1;31mERROR: $*\033[0m" >&2; }

get_vm_ip() {
    hcloud server list -o columns=name,ipv4 -o noheader 2>/dev/null | grep "$SERVER_NAME" | awk '{print $2}' | tr -d ' '
}

ssh_cmd() {
    local ip=$(get_vm_ip)
    [ -n "$ip" ] || { err "No VM found"; exit 1; }
    ssh $SSH_OPTS -i "$SSH_KEY_PATH" "root@$ip" "$@"
}

notify() { curl -s -d "$1" "$NTFY_TOPIC" > /dev/null 2>&1 || true; }

# --- Create VM if needed ---
existing=$(hcloud server list -o columns=name -o noheader 2>/dev/null | grep "$SERVER_NAME" | tr -d ' ' || true)
if [ -z "$existing" ]; then
    log "Creating Hetzner VM ($SERVER_TYPE, $IMAGE)..."
    hcloud server create --name "$SERVER_NAME" --type "$SERVER_TYPE" --image "$IMAGE" --ssh-key "$SSH_KEY_NAME" --location fsn1 --quiet

    log "Waiting for SSH..."
    ip=$(get_vm_ip)
    for i in $(seq 1 30); do
        ssh $SSH_OPTS -i "$SSH_KEY_PATH" "root@$ip" "echo ok" &>/dev/null && break
        sleep 2
    done

    log "Installing deps..."
    ssh_cmd "apt-get update -qq && apt-get install -y -qq build-essential cmake pkg-config libasound2-dev libssl-dev curl git > /dev/null 2>&1"
    ssh_cmd "curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable > /dev/null 2>&1"
fi

# --- Upload source ---
log "Uploading source..."
ip=$(get_vm_ip)
rsync -az --delete \
    --exclude='target' --exclude='.git' --exclude='.claude' \
    --exclude='node_modules' --exclude='dist' --exclude='android/app/build' \
    -e "ssh $SSH_OPTS -i $SSH_KEY_PATH" \
    "$PROJECT_DIR/" "root@$ip:/root/wzp-build/"

# --- Build ---
log "Building all binaries..."
notify "WZP Linux build started..."

ssh_cmd "source ~/.cargo/env && cd /root/wzp-build && \
    cargo build --release --bin wzp-relay --bin wzp-client --bin wzp-web --bin wzp-bench 2>&1 | tail -5 && \
    echo '--- audio client ---' && \
    cargo build --release --bin wzp-client --features audio 2>&1 | tail -3 && \
    cp target/release/wzp-client target/release/wzp-client-audio && \
    cargo build --release --bin wzp-client 2>&1 | tail -3 && \
    echo 'BUILD_DONE' && \
    ls -lh target/release/wzp-relay target/release/wzp-client target/release/wzp-client-audio target/release/wzp-web target/release/wzp-bench"

# --- Package + upload to rustypaste ---
log "Packaging and uploading..."
UPLOAD_URL=$(ssh_cmd "cd /root/wzp-build && \
    tar czf /tmp/wzp-linux-x86_64.tar.gz \
        -C target/release wzp-relay wzp-client wzp-client-audio wzp-web wzp-bench \
        -C /root/wzp-build/crates/wzp-web/static index.html audio-processor.js 2>/dev/null && \
    curl -s -F 'file=@/tmp/wzp-linux-x86_64.tar.gz' \
        -H 'Authorization: DAxAAGghkn1WKv1+RpPKkg==' \
        https://paste.dk.manko.yoga")

if [ -n "$UPLOAD_URL" ]; then
    notify "WZP Linux binaries ready! $UPLOAD_URL"
    log "Uploaded: $UPLOAD_URL"
else
    notify "WZP Linux build FAILED"
    err "Upload failed"
fi

# --- Transfer locally ---
log "Downloading binaries..."
mkdir -p "$LOCAL_OUTPUT"
for bin in wzp-relay wzp-client wzp-client-audio wzp-web wzp-bench; do
    scp $SSH_OPTS -i "$SSH_KEY_PATH" "root@$ip:/root/wzp-build/target/release/$bin" "$LOCAL_OUTPUT/$bin" 2>/dev/null
done
ls -lh "$LOCAL_OUTPUT"/wzp-*

# --- Cleanup ---
if [ "$KEEP_VM" = "1" ]; then
    log "VM kept alive. Destroy: hcloud server delete $SERVER_NAME"
else
    log "Destroying VM..."
    hcloud server delete "$SERVER_NAME"
fi

log "Done!"
echo "  Deploy: scp $LOCAL_OUTPUT/wzp-relay user@server:~/wzp/"
