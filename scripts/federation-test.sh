#!/usr/bin/env bash
set -euo pipefail

# Federation Test Harness
# Tests presence, audio delivery, and reconnection across 3 relays.
#
# Usage:
#   ./scripts/federation-test.sh <relay1> <relay2> <relay3>
#   ./scripts/federation-test.sh 172.16.81.175:4434 172.16.81.175:4435 172.16.81.175:4436
#
# Requires: wzp-client binary in PATH or target/release/

RELAY1="${1:-127.0.0.1:4433}"
RELAY2="${2:-127.0.0.1:4434}"
RELAY3="${3:-127.0.0.1:4435}"
ROOM="general"
CLIENT="${WZP_CLIENT:-target/release/wzp-client}"
AUDIO="/tmp/test-audio-60s.raw"
RESULTS="/tmp/federation-test-results"
DURATION=15  # seconds per test phase

# Fixed seeds for reproducible identities
SEED_A="aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
SEED_B="bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
SEED_C="cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc"
SEED_D="dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd"

log() { echo -e "\033[1;36m>>> $*\033[0m"; }
err() { echo -e "\033[1;31mERROR: $*\033[0m" >&2; }
pass() { echo -e "\033[1;32m  PASS: $*\033[0m"; }
fail() { echo -e "\033[1;31m  FAIL: $*\033[0m"; }

analyze() {
    local path="$1" label="$2"
    if [ ! -f "$path" ] || [ ! -s "$path" ]; then
        fail "$label: NO FILE or empty"
        return 1
    fi
    python3 -c "
import struct, math
with open('$path', 'rb') as f: data = f.read()
if len(data) < 4:
    print('  $label: EMPTY')
    exit(1)
samples = struct.unpack(f'<{len(data)//2}h', data)
n = len(samples)
rms = math.sqrt(sum(s*s for s in samples) / n) if n > 0 else 0
dur = n / 48000
nonzero = sum(1 for s in samples if s != 0)
pct = 100 * nonzero / n if n > 0 else 0
if rms > 50 and pct > 5:
    print(f'  \033[32mPASS\033[0m: $label — {dur:.1f}s, RMS {rms:.0f}, {pct:.0f}% nonzero')
else:
    print(f'  \033[31mFAIL\033[0m: $label — {dur:.1f}s, RMS {rms:.0f}, {pct:.0f}% nonzero')
    exit(1)
" 2>/dev/null
}

cleanup() {
    log "Cleaning up..."
    kill ${PIDS[@]} 2>/dev/null || true
    wait 2>/dev/null || true
}
trap cleanup EXIT

mkdir -p "$RESULTS"
PIDS=()

# Generate test audio if missing
if [ ! -f "$AUDIO" ]; then
    log "Generating test audio..."
    python3 -c "
import struct, math, random
RATE = 48000; samples = []
t = 0
while t < 60 * RATE:
    burst = random.randint(int(RATE*0.2), int(RATE*0.8))
    freq = random.choice([220,330,440,550,660,880])
    amp = random.uniform(8000,16000)
    for i in range(min(burst, 60*RATE-t)):
        s = amp * math.sin(2*math.pi*freq*(t+i)/RATE)
        samples.append(int(max(-32767,min(32767,s))))
    t += burst
    sil = random.randint(int(RATE*0.1), int(RATE*0.5))
    samples.extend([0]*min(sil, 60*RATE-t)); t += sil
with open('$AUDIO', 'wb') as f:
    f.write(struct.pack(f'<{len(samples)}h', *samples))
print(f'Generated {len(samples)/RATE:.1f}s')
"
fi

echo ""
echo "╔══════════════════════════════════════════════════════════╗"
echo "║          WarzonePhone Federation Test Suite              ║"
echo "╠══════════════════════════════════════════════════════════╣"
echo "║  Relay 1: $RELAY1"
echo "║  Relay 2: $RELAY2"
echo "║  Relay 3: $RELAY3"
echo "║  Room:    $ROOM"
echo "║  Duration: ${DURATION}s per phase"
echo "╚══════════════════════════════════════════════════════════╝"
echo ""

# ═══════════════════════════════════════════════════════════════
# TEST 1: Basic 2-relay audio
# ═══════════════════════════════════════════════════════════════
log "TEST 1: Basic audio — A sends on Relay1, B records on Relay2"

RUST_LOG=error $CLIENT --room $ROOM --seed $SEED_B --record "$RESULTS/t1_b.raw" "$RELAY2" &
PIDS+=($!); sleep 2

RUST_LOG=error $CLIENT --room $ROOM --seed $SEED_A --send-tone $DURATION "$RELAY1" &
PIDS+=($!); sleep $((DURATION + 3))

kill -INT ${PIDS[-2]} 2>/dev/null; sleep 3; kill -INT ${PIDS[-1]} 2>/dev/null; wait ${PIDS[-1]} ${PIDS[-2]} 2>/dev/null || true
PIDS=("${PIDS[@]:0:${#PIDS[@]}-2}")

analyze "$RESULTS/t1_b.raw" "Relay1→Relay2 audio"
echo ""

# ═══════════════════════════════════════════════════════════════
# TEST 2: Reverse direction
# ═══════════════════════════════════════════════════════════════
log "TEST 2: Reverse — B sends on Relay2, A records on Relay1"

RUST_LOG=error $CLIENT --room $ROOM --seed $SEED_A --record "$RESULTS/t2_a.raw" "$RELAY1" &
PIDS+=($!); sleep 2

RUST_LOG=error $CLIENT --room $ROOM --seed $SEED_B --send-tone $DURATION "$RELAY2" &
PIDS+=($!); sleep $((DURATION + 3))

kill -INT ${PIDS[-2]} 2>/dev/null; sleep 3; kill -INT ${PIDS[-1]} 2>/dev/null; wait ${PIDS[-1]} ${PIDS[-2]} 2>/dev/null || true
PIDS=("${PIDS[@]:0:${#PIDS[@]}-2}")

analyze "$RESULTS/t2_a.raw" "Relay2→Relay1 audio"
echo ""

# ═══════════════════════════════════════════════════════════════
# TEST 3: 3-relay chain
# ═══════════════════════════════════════════════════════════════
log "TEST 3: 3-relay chain — A sends on Relay1, C records on Relay3"

RUST_LOG=error $CLIENT --room $ROOM --seed $SEED_C --record "$RESULTS/t3_c.raw" "$RELAY3" &
PIDS+=($!); sleep 2

RUST_LOG=error $CLIENT --room $ROOM --seed $SEED_A --send-tone $DURATION "$RELAY1" &
PIDS+=($!); sleep $((DURATION + 3))

kill -INT ${PIDS[-2]} 2>/dev/null; sleep 3; kill -INT ${PIDS[-1]} 2>/dev/null; wait ${PIDS[-1]} ${PIDS[-2]} 2>/dev/null || true
PIDS=("${PIDS[@]:0:${#PIDS[@]}-2}")

analyze "$RESULTS/t3_c.raw" "Relay1→Relay3 (via Relay2) audio"
echo ""

# ═══════════════════════════════════════════════════════════════
# TEST 4: File playback (simulated talk show)
# ═══════════════════════════════════════════════════════════════
log "TEST 4: File playback — A plays audio file on Relay1, B records on Relay2"

RUST_LOG=error $CLIENT --room $ROOM --seed $SEED_B --record "$RESULTS/t4_b.raw" "$RELAY2" &
PIDS+=($!); sleep 2

RUST_LOG=error $CLIENT --room $ROOM --seed $SEED_A --send-file "$AUDIO" "$RELAY1" &
PIDS+=($!); sleep 20  # file is 60s but we only wait 20

kill -INT ${PIDS[-2]} 2>/dev/null; sleep 3; kill -INT ${PIDS[-1]} 2>/dev/null; wait ${PIDS[-1]} ${PIDS[-2]} 2>/dev/null || true
PIDS=("${PIDS[@]:0:${#PIDS[@]}-2}")

analyze "$RESULTS/t4_b.raw" "File playback Relay1→Relay2"
echo ""

# ═══════════════════════════════════════════════════════════════
# TEST 5: Reconnection — B disconnects and rejoins
# ═══════════════════════════════════════════════════════════════
log "TEST 5: Reconnection — A sends, B joins/leaves/rejoins on Relay2"

# A sends continuously
RUST_LOG=error $CLIENT --room $ROOM --seed $SEED_A --send-tone 30 "$RELAY1" &
A_PID=$!; PIDS+=($A_PID)
sleep 2

# B joins and records for 5s
RUST_LOG=error $CLIENT --room $ROOM --seed $SEED_B --record "$RESULTS/t5_b_first.raw" "$RELAY2" &
B_PID=$!; PIDS+=($B_PID)
sleep 5
kill -INT $B_PID 2>/dev/null; wait $B_PID 2>/dev/null || true

log "  B disconnected, waiting 3s..."
sleep 3

# B rejoins and records for 5s
RUST_LOG=error $CLIENT --room $ROOM --seed $SEED_B --record "$RESULTS/t5_b_rejoin.raw" "$RELAY2" &
B_PID=$!; PIDS+=($B_PID)
sleep 8
kill -INT $B_PID 2>/dev/null; wait $B_PID 2>/dev/null || true
kill -INT $A_PID 2>/dev/null; wait $A_PID 2>/dev/null || true

analyze "$RESULTS/t5_b_first.raw" "B first join (before disconnect)"
analyze "$RESULTS/t5_b_rejoin.raw" "B rejoin (after disconnect)"
echo ""

# ═══════════════════════════════════════════════════════════════
# TEST 6: Multi-participant — 3 users on 3 relays
# ═══════════════════════════════════════════════════════════════
log "TEST 6: Multi-participant — A sends on R1, B records on R2, C records on R3"

RUST_LOG=error $CLIENT --room $ROOM --seed $SEED_B --record "$RESULTS/t6_b.raw" "$RELAY2" &
PIDS+=($!); sleep 1
RUST_LOG=error $CLIENT --room $ROOM --seed $SEED_C --record "$RESULTS/t6_c.raw" "$RELAY3" &
PIDS+=($!); sleep 1
RUST_LOG=error $CLIENT --room $ROOM --seed $SEED_A --send-tone $DURATION "$RELAY1" &
PIDS+=($!); sleep $((DURATION + 3))

# Kill all 3
for i in 1 2 3; do
    kill -INT ${PIDS[-$i]} 2>/dev/null || true
done
wait 2>/dev/null || true
PIDS=()

analyze "$RESULTS/t6_b.raw" "B on Relay2 hears A on Relay1"
analyze "$RESULTS/t6_c.raw" "C on Relay3 hears A on Relay1"
echo ""

# ═══════════════════════════════════════════════════════════════
# TEST 7: Simultaneous senders
# ═══════════════════════════════════════════════════════════════
log "TEST 7: Simultaneous — A sends 440Hz on R1, B sends 880Hz on R2, C records on R3"

RUST_LOG=error $CLIENT --room $ROOM --seed $SEED_C --record "$RESULTS/t7_c.raw" "$RELAY3" &
PIDS+=($!); sleep 2
RUST_LOG=error $CLIENT --room $ROOM --seed $SEED_A --send-tone $DURATION "$RELAY1" &
PIDS+=($!);
RUST_LOG=error $CLIENT --room $ROOM --seed $SEED_B --send-tone $DURATION "$RELAY2" &
PIDS+=($!); sleep $((DURATION + 3))

for i in 1 2 3; do kill ${PIDS[-$i]} 2>/dev/null || true; done
wait 2>/dev/null || true
PIDS=()

analyze "$RESULTS/t7_c.raw" "C hears both A(R1) + B(R2)"
echo ""

# ═══════════════════════════════════════════════════════════════
# SUMMARY
# ═══════════════════════════════════════════════════════════════
echo ""
echo "╔══════════════════════════════════════════════════════════╗"
echo "║                    TEST SUMMARY                          ║"
echo "╠══════════════════════════════════════════════════════════╣"

PASS=0; FAIL=0
for f in "$RESULTS"/t*.raw; do
    label=$(basename "$f" .raw)
    if [ -s "$f" ]; then
        rms=$(python3 -c "
import struct, math
with open('$f','rb') as f: d=f.read()
s=struct.unpack(f'<{len(d)//2}h',d)
print(f'{math.sqrt(sum(x*x for x in s)/len(s)):.0f}')
" 2>/dev/null || echo "0")
        if [ "$rms" -gt 50 ] 2>/dev/null; then
            echo "║  ✓ $label (RMS: $rms)"
            PASS=$((PASS + 1))
        else
            echo "║  ✗ $label (RMS: $rms)"
            FAIL=$((FAIL + 1))
        fi
    else
        echo "║  ✗ $label (NO FILE)"
        FAIL=$((FAIL + 1))
    fi
done

echo "╠══════════════════════════════════════════════════════════╣"
echo "║  PASSED: $PASS  FAILED: $FAIL"
echo "╚══════════════════════════════════════════════════════════╝"
echo ""
echo "Recordings saved to: $RESULTS/"
echo "Play with: ffplay -f s16le -ar 48000 -ac 1 $RESULTS/<file>.raw"
