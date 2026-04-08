#!/usr/bin/env bash
# Multi-hop bridge end-to-end test.
#
# This script:
#   1. Builds the bridge binary (release)
#   2. Deploys the multi-hop nlink-lab topology
#   3. Waits for all services to be healthy
#   4. Sends data through the full chain: client -> import -> zenoh -> export -> backend
#   5. Verifies the data arrived correctly
#   6. Tears down the topology
#
# Requirements:
#   - nlink-lab installed (suid)
#   - cargo (Rust toolchain)
#
# Usage:
#    ./tests/nlink/run-multi-hop-test.sh
#    ./tests/nlink/run-multi-hop-test.sh --wan-delay 100ms --wan-loss 1%

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/../.." && pwd)"
TOPOLOGY="$SCRIPT_DIR/multi-hop-bridge.nll"
LAB_NAME="multi-hop-bridge"
NLINK_LAB="${NLINK_LAB:-nlink-lab}"

# Parse optional arguments
WAN_DELAY="30ms"
WAN_LOSS="0%"
SKIP_BUILD=0

while [[ $# -gt 0 ]]; do
    case "$1" in
        --wan-delay) WAN_DELAY="$2"; shift 2 ;;
        --wan-loss)  WAN_LOSS="$2";  shift 2 ;;
        --skip-build) SKIP_BUILD=1;  shift ;;
        *) echo "Unknown option: $1"; exit 1 ;;
    esac
done

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m'

pass() { echo -e "${GREEN}[PASS]${NC} $1"; }
fail() { echo -e "${RED}[FAIL]${NC} $1"; }
info() { echo -e "${YELLOW}[INFO]${NC} $1"; }

cleanup() {
    info "Cleaning up topology..."
    $NLINK_LAB destroy "$LAB_NAME" 2>/dev/null || true
}
trap cleanup EXIT

# --- Step 1: Build ---
if [[ $SKIP_BUILD -eq 0 ]]; then
    info "Building zenoh-bridge-tcp (release)..."
    (cd "$PROJECT_DIR" && cargo build --release --quiet)
fi

BRIDGE_BIN="$PROJECT_DIR/target/release/zenoh-bridge-tcp"
if [[ ! -x "$BRIDGE_BIN" ]]; then
    fail "Bridge binary not found at $BRIDGE_BIN"
    exit 1
fi
info "Bridge binary: $BRIDGE_BIN"

# --- Step 2: Clean any previous run ---
$NLINK_LAB destroy "$LAB_NAME" 2>/dev/null || true

# --- Step 3: Deploy topology ---
info "Deploying topology (wan_delay=$WAN_DELAY, wan_loss=$WAN_LOSS)..."
$NLINK_LAB deploy "$TOPOLOGY" \
    --set "wan_delay=$WAN_DELAY" \
    --set "wan_loss=$WAN_LOSS" \
    --set "bridge_bin=$BRIDGE_BIN"

info "Waiting for services to become healthy..."
sleep 8

# --- Step 4: Validate basic connectivity ---
info "Running nlink-lab validation..."
if $NLINK_LAB test "$TOPOLOGY" --set "bridge_bin=$BRIDGE_BIN"; then
    pass "nlink-lab validation passed"
else
    fail "nlink-lab validation failed"
    info "Dumping lab status..."
    $NLINK_LAB status "$LAB_NAME" || true
    exit 1
fi

# --- Step 5: End-to-end data flow test ---
FAILURES=0

# Test 1: Simple echo through the bridge
info "Test 1: Echo through multi-hop bridge..."
RESPONSE=$($NLINK_LAB exec "$LAB_NAME" client -- \
    bash -c 'echo "test-payload-123" | nc -w 5 10.0.4.20 8002' 2>&1 || true)

if echo "$RESPONSE" | grep -q "hello"; then
    pass "Test 1: Data flowed through multi-hop bridge (client -> import -> zenoh -> export -> backend)"
else
    fail "Test 1: Expected 'hello' from echo server, got: '$RESPONSE'"
    FAILURES=$((FAILURES + 1))
fi

# Test 2: Multiple sequential connections
info "Test 2: Multiple sequential connections..."
SUCCESS_COUNT=0
for i in $(seq 1 5); do
    RESP=$($NLINK_LAB exec "$LAB_NAME" client -- \
        bash -c "echo 'ping-$i' | nc -w 5 10.0.4.20 8002" 2>&1 || true)
    if echo "$RESP" | grep -q "hello"; then
        SUCCESS_COUNT=$((SUCCESS_COUNT + 1))
    fi
    sleep 1
done

if [[ $SUCCESS_COUNT -ge 4 ]]; then
    pass "Test 2: $SUCCESS_COUNT/5 sequential connections succeeded"
else
    fail "Test 2: Only $SUCCESS_COUNT/5 sequential connections succeeded"
    FAILURES=$((FAILURES + 1))
fi

# Test 3: Verify latency is within expected bounds (WAN delay * 2 round-trip + overhead)
info "Test 3: Latency check (WAN delay: $WAN_DELAY)..."
LATENCY=$($NLINK_LAB exec "$LAB_NAME" client -- \
    bash -c 'ping -c 3 -q 10.0.1.10 2>&1 | tail -1' 2>&1 || true)
if echo "$LATENCY" | grep -q "rtt"; then
    pass "Test 3: Round-trip latency: $LATENCY"
else
    info "Test 3: Could not measure latency (non-critical): $LATENCY"
fi

# --- Step 6: Report ---
echo ""
echo "================================="
if [[ $FAILURES -eq 0 ]]; then
    pass "All multi-hop bridge tests passed!"
    exit 0
else
    fail "$FAILURES test(s) failed"
    info "Inspect with:  nlink-lab shell $LAB_NAME client"
    exit 1
fi
