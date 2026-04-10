#!/usr/bin/env bash
set -euo pipefail

# =============================================================================
# WZ Phone — Windows x86_64 cross-compile (Docker on SepehrHomeserverdk)
#
# Cross-compiles the Tauri desktop binary for Windows via `cargo xwin`
# inside the wzp-windows-builder Docker image on SepehrHomeserverdk.
# Uploads the resulting .exe to rustypaste, fires ntfy.sh/wzp notifications
# at start + finish, and SCPs the .exe back locally.
#
# Same pattern as build-tauri-android.sh but for the Windows cross-compile
# pipeline:
#   - Source: desktop/src-tauri/
#   - Build:  cargo xwin build --release --target x86_64-pc-windows-msvc
#   - Output: target/x86_64-pc-windows-msvc/release/wzp-desktop.exe
#
# Usage:
#   ./scripts/build-windows-docker.sh                # full pipeline
#   ./scripts/build-windows-docker.sh --no-pull      # skip git fetch
#   ./scripts/build-windows-docker.sh --rust         # force-clean rust target
#   ./scripts/build-windows-docker.sh --image-build  # (re)build the docker image
#
# Environment:
#   WZP_BRANCH   Branch to build (default: feat/desktop-audio-rewrite)
# =============================================================================

REMOTE_HOST="SepehrHomeserverdk"
BASE_DIR="/mnt/storage/manBuilder"
NTFY_TOPIC="https://ntfy.sh/wzp"
LOCAL_OUTPUT="target/windows-exe"
BRANCH="${WZP_BRANCH:-feat/desktop-audio-rewrite}"
SSH_OPTS="-o ConnectTimeout=15 -o ServerAliveInterval=15 -o ServerAliveCountMax=4 -o LogLevel=ERROR"

REBUILD_RUST=0
DO_PULL=1
IMAGE_BUILD=0
for arg in "$@"; do
    case "$arg" in
        --rust)         REBUILD_RUST=1 ;;
        --pull)         DO_PULL=1 ;;
        --no-pull)      DO_PULL=0 ;;
        --image-build)  IMAGE_BUILD=1 ;;
        -h|--help)
            sed -n '3,27p' "$0"
            exit 0
            ;;
    esac
done

log() { echo -e "\033[1;36m>>> $*\033[0m"; }
ssh_cmd() { ssh -A $SSH_OPTS "$REMOTE_HOST" "$@"; }

notify_local() { curl -s -d "$1" "$NTFY_TOPIC" > /dev/null 2>&1 || true; }

mkdir -p "$LOCAL_OUTPUT"

# ─── Optional: (re)build the docker image on the remote ────────────────────
# Runs once, whenever the Dockerfile changes. Fire-and-forget so the local
# script doesn't wait for the ~15 minute image build.
if [ "$IMAGE_BUILD" = "1" ]; then
    log "Uploading Dockerfile.windows-builder to remote..."
    scp $SSH_OPTS "$(dirname "$0")/Dockerfile.windows-builder" \
        "$REMOTE_HOST:$BASE_DIR/Dockerfile.windows-builder"

    log "Triggering remote image build (fire-and-forget)..."
    ssh_cmd "cd $BASE_DIR && \
        nohup docker build --pull -f Dockerfile.windows-builder \
            -t wzp-windows-builder . \
            > /tmp/wzp-windows-image-build.log 2>&1 & \
        echo 'image build PID: '\$!"
    notify_local "WZP Windows image build dispatched (check /tmp/wzp-windows-image-build.log on remote)"
    log "Image build running in background on $REMOTE_HOST."
    log "Tail the log with:  ssh $REMOTE_HOST 'tail -f /tmp/wzp-windows-image-build.log'"
    exit 0
fi

# ─── Upload remote build runner script ─────────────────────────────────────
log "Uploading remote build script..."
ssh_cmd "cat > /tmp/wzp-windows-build.sh" <<'REMOTE_SCRIPT'
#!/usr/bin/env bash
set -euo pipefail

BASE_DIR="/mnt/storage/manBuilder"
NTFY_TOPIC="https://ntfy.sh/wzp"
BRANCH="${1:-feat/desktop-audio-rewrite}"
DO_PULL="${2:-1}"
REBUILD_RUST="${3:-0}"

LOG_FILE=/tmp/wzp-windows-build.log
GIT_HASH="unknown"
ENV_FILE="$BASE_DIR/.env"

notify() { curl -s -d "$1" "$NTFY_TOPIC" > /dev/null 2>&1 || true; }

# Upload to rustypaste; print URL on stdout (or empty on failure).
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

on_error() {
    local line="$1"
    local log_url
    log_url=$(upload_to_rustypaste "$LOG_FILE" || echo "")
    if [ -n "$log_url" ]; then
        notify "WZP Windows build FAILED [$GIT_HASH] (line $line)
log: $log_url"
    else
        notify "WZP Windows build FAILED [$GIT_HASH] (line $line) — log upload failed, see $LOG_FILE on remote"
    fi
}
trap 'on_error $LINENO' ERR

exec > >(tee "$LOG_FILE") 2>&1

# ── git fetch + reset the target branch ───────────────────────────────────
if [ "$DO_PULL" = "1" ]; then
    echo ">>> git fetch + reset $BRANCH"
    cd "$BASE_DIR/data/source"
    git reset --hard HEAD 2>/dev/null || true
    git gc --prune=now 2>/dev/null || true
    git fetch origin "$BRANCH" 2>&1 | tail -3
    git checkout "$BRANCH" 2>/dev/null || git checkout -b "$BRANCH" "origin/$BRANCH"
    git reset --hard "origin/$BRANCH"
    git submodule update --init --recursive || true
fi

GIT_HASH=$(cd "$BASE_DIR/data/source" && git rev-parse --short HEAD 2>/dev/null || echo unknown)
GIT_MSG=$(cd "$BASE_DIR/data/source" && git log -1 --pretty=%s 2>/dev/null | head -c 60 || echo "?")
notify "WZP Windows build STARTED [$GIT_HASH] — $GIT_MSG"

# Fix perms so builder uid 1000 can read/write the mounted source.
find "$BASE_DIR/data/source" "$BASE_DIR/data/cache" \
    ! -user 1000 -o ! -group 1000 2>/dev/null | \
    xargs -r chown 1000:1000 2>/dev/null || true

if [ "$REBUILD_RUST" = "1" ]; then
    echo ">>> Cleaning Rust windows target dir..."
    rm -rf "$BASE_DIR/data/cache/target-windows/x86_64-pc-windows-msvc" \
           "$BASE_DIR/data/cache/target-windows/release"
fi

# ── Docker run ─────────────────────────────────────────────────────────────
# Cached volumes:
#   - cargo-registry / cargo-git: shared with the android builder — both use
#     the same crates, so the download cache is worth sharing.
#   - target-windows: the Windows target tree. Kept separate from the android
#     target-cache so the two pipelines don't stomp on each other's build
#     artefacts (different triples, but the workspace root target dir has
#     shared subdirs like release/build/ that can get confused).
#   - cargo-xwin cache is BAKED into the docker image, no volume needed.

mkdir -p "$BASE_DIR/data/cache/cargo-registry" \
         "$BASE_DIR/data/cache/cargo-git" \
         "$BASE_DIR/data/cache/target-windows"
chown -R 1000:1000 "$BASE_DIR/data/cache/target-windows" 2>/dev/null || true

docker run --rm \
    --user 1000:1000 \
    -v "$BASE_DIR/data/source:/build/source" \
    -v "$BASE_DIR/data/cache/cargo-registry:/home/builder/.cargo/registry" \
    -v "$BASE_DIR/data/cache/cargo-git:/home/builder/.cargo/git" \
    -v "$BASE_DIR/data/cache/target-windows:/build/source/target" \
    wzp-windows-builder \
    bash -c '
set -euo pipefail

# libopus (audiopus_sys) ships per-file SSE4.1 / SSSE3 C sources under
# opus/silk/x86/ that assume the compiler sets `-msse4.1` / `-mssse3`
# as per-file CMake COMPILE_FLAGS. With clang-cl those bare -m flags
# are silently ignored (clang-cl expects /clang:-msse4.1), so the
# intrinsics functions in those files fail to compile with:
#   "error: always_inline function _mm_cvtepi16_epi32 requires target
#    feature sse4.1, but would be inlined into a function that is
#    compiled without support for sse4.1"
#
# env-var CFLAGS do NOT help here: cargo-xwin ships its own CMake
# toolchain file (~/.cache/cargo-xwin/cmake/clang-cl/x86_64-pc-windows-msvc-toolchain.cmake)
# which hardcodes COMPILE_FLAGS and FORCE-overrides CMAKE_C_FLAGS,
# so anything we export via CFLAGS_$TARGET gets dropped. The only
# place that actually reaches every C compilation inside the opus
# cmake build is the toolchain file itself. Patch it in place with
# a one-shot sed: append four /clang:-m* flags immediately before
# the closing paren of the COMPILE_FLAGS setter. Idempotent across
# runs — if the file already contains the sentinel line we skip.
TOOLCHAIN_FILE=/home/builder/.cache/cargo-xwin/cmake/clang-cl/x86_64-pc-windows-msvc-toolchain.cmake
echo ">>> Patching cargo-xwin toolchain for SSE4.1 / SSSE3 intrinsics"
if grep -q WZP_SSE_PATCH "$TOOLCHAIN_FILE"; then
    echo "    (already patched, skipping)"
else
    # The cleanest way to add flags is to append a pure-CMake block to
    # the end of the toolchain file. Both CMAKE_C_FLAGS and
    # CMAKE_CXX_FLAGS were already FORCE-set earlier in the toolchain;
    # we override them again with the same content plus our SSE flags.
    # This runs AFTER the original set(... FORCE) and the CMAKE_ARGS
    # from the cmake-rs invocation, so it wins.
    cat >> "$TOOLCHAIN_FILE" <<CMAKE_PATCH_EOF

# ── WZP_SSE_PATCH ─────────────────────────────────────────────────────────
# libopus (audiopus_sys) has per-file COMPILE_FLAGS "-msse4.1" on its
# sse4_1.c sources, which clang-cl silently drops (expects /clang:-m…).
# Force-add the flags globally so every C file in the opus cmake subbuild
# compiles with the intrinsics target features enabled. All x86_64
# Windows CPUs shipped since 2008 support these, so enabling them
# globally on this target is safe.
set(CMAKE_C_FLAGS   "\${CMAKE_C_FLAGS}   /clang:-msse4.1 /clang:-mssse3 /clang:-msse3 /clang:-msse2" CACHE STRING "" FORCE)
set(CMAKE_CXX_FLAGS "\${CMAKE_CXX_FLAGS} /clang:-msse4.1 /clang:-mssse3 /clang:-msse3 /clang:-msse2" CACHE STRING "" FORCE)
CMAKE_PATCH_EOF
fi
echo ">>> Toolchain tail after patch:"
tail -15 "$TOOLCHAIN_FILE"

cd /build/source/desktop

echo ">>> npm install"
npm install --silent 2>&1 | tail -5 || npm install 2>&1 | tail -20

echo ">>> npm run build"
npm run build 2>&1 | tail -5

echo ">>> cargo xwin build --release --target x86_64-pc-windows-msvc --bin wzp-desktop"
cd src-tauri
cargo xwin build --release --target x86_64-pc-windows-msvc --bin wzp-desktop 2>&1 | tail -50

echo ""
echo ">>> Build artifacts:"
ls -lh /build/source/target/x86_64-pc-windows-msvc/release/wzp-desktop.exe 2>/dev/null || echo "NO EXE"
'

# Locate the produced .exe
EXE="$BASE_DIR/data/cache/target-windows/x86_64-pc-windows-msvc/release/wzp-desktop.exe"
if [ ! -f "$EXE" ]; then
    LOG_URL=$(upload_to_rustypaste "$LOG_FILE" || echo "")
    if [ -n "$LOG_URL" ]; then
        notify "WZP Windows build [$GIT_HASH]: no .exe produced
log: $LOG_URL"
    else
        notify "WZP Windows build [$GIT_HASH]: no .exe produced — log upload failed"
    fi
    exit 1
fi

EXE_SIZE=$(du -h "$EXE" | cut -f1)

RUSTY_URL=$(upload_to_rustypaste "$EXE" || echo "")
if [ -n "$RUSTY_URL" ]; then
    notify "WZP Windows build OK [$GIT_HASH] ($EXE_SIZE)
$RUSTY_URL"
else
    notify "WZP Windows build OK [$GIT_HASH] ($EXE_SIZE) — rustypaste upload skipped"
fi

# Print path so the local script can scp it back
echo "EXE_REMOTE_PATH=$EXE"
REMOTE_SCRIPT

ssh_cmd "chmod +x /tmp/wzp-windows-build.sh"

notify_local "WZP Windows build dispatched (branch=$BRANCH)"
log "Triggering remote build (branch=$BRANCH)..."

# Run; last line is EXE_REMOTE_PATH=...
REMOTE_OUTPUT=$(ssh_cmd "/tmp/wzp-windows-build.sh '$BRANCH' '$DO_PULL' '$REBUILD_RUST'" || true)
echo "$REMOTE_OUTPUT" | tail -60

EXE_REMOTE=$(echo "$REMOTE_OUTPUT" | grep '^EXE_REMOTE_PATH=' | tail -1 | cut -d= -f2-)
if [ -n "$EXE_REMOTE" ]; then
    log "Downloading wzp-desktop.exe to $LOCAL_OUTPUT/..."
    scp $SSH_OPTS "$REMOTE_HOST:$EXE_REMOTE" "$LOCAL_OUTPUT/wzp-desktop.exe"
    echo "  $LOCAL_OUTPUT/wzp-desktop.exe ($(du -h "$LOCAL_OUTPUT/wzp-desktop.exe" | cut -f1))"
else
    log "No .exe produced — see ntfy / remote log /tmp/wzp-windows-build.log"
    exit 1
fi
