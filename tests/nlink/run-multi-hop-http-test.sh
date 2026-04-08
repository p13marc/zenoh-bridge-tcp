#!/usr/bin/env bash
# Multi-hop HTTP routing bridge end-to-end test.
#
# Tests Host-header routing through a multi-hop zenoh bridge:
#   - Two backends (api.example.com, web.example.com) at site A
#   - One import listener at site B routes by Host header
#   - Client at site B sends HTTP requests with different Host headers
#
# Requirements:
#   - nlink-lab installed (suid)
#   - cargo (Rust toolchain)
#   - curl available in network namespaces
#
# Usage:
#    ./tests/nlink/run-multi-hop-http-test.sh
#    ./tests/nlink/run-multi-hop-http-test.sh --wan-delay 100ms

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/../.." && pwd)"
TOPOLOGY="$SCRIPT_DIR/multi-hop-http-bridge.nll"
LAB_NAME="multi-hop-http"
NLINK_LAB="${NLINK_LAB:-nlink-lab}"

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

# --- Build ---
if [[ $SKIP_BUILD -eq 0 ]]; then
    info "Building zenoh-bridge-tcp (release)..."
    (cd "$PROJECT_DIR" && cargo build --release --quiet)
fi

BRIDGE_BIN="$PROJECT_DIR/target/release/zenoh-bridge-tcp"
if [[ ! -x "$BRIDGE_BIN" ]]; then
    fail "Bridge binary not found at $BRIDGE_BIN"
    exit 1
fi

# --- Deploy ---
$NLINK_LAB destroy "$LAB_NAME" 2>/dev/null || true

info "Deploying HTTP topology (wan_delay=$WAN_DELAY, wan_loss=$WAN_LOSS)..."
$NLINK_LAB deploy "$TOPOLOGY" \
    --set "wan_delay=$WAN_DELAY" \
    --set "wan_loss=$WAN_LOSS" \
    --set "bridge_bin=$BRIDGE_BIN"

info "Waiting for services to become healthy..."
sleep 10

# --- Validate connectivity ---
info "Running nlink-lab validation..."
if $NLINK_LAB test "$TOPOLOGY" --set "bridge_bin=$BRIDGE_BIN"; then
    pass "nlink-lab validation passed"
else
    fail "nlink-lab validation failed"
    $NLINK_LAB status "$LAB_NAME" || true
    exit 1
fi

# --- HTTP routing tests ---
FAILURES=0
IMPORTER_IP="10.0.4.20"

# Test 1: Route to api.example.com backend (python http.server returns 200 with directory listing)
info "Test 1: HTTP request routed to api.example.com..."
HTTP_CODE=$($NLINK_LAB exec "$LAB_NAME" client -- \
    curl -s -o /dev/null -w "%{http_code}" -m 10 -H "Host: api.example.com" "http://$IMPORTER_IP:8080/" 2>&1 || true)

if [[ "$HTTP_CODE" == "200" ]]; then
    pass "Test 1: Host: api.example.com -> 200 OK (routed to api backend)"
else
    fail "Test 1: Expected HTTP 200, got: $HTTP_CODE"
    FAILURES=$((FAILURES + 1))
fi

# Test 2: Route to web.example.com backend
info "Test 2: HTTP request routed to web.example.com..."
HTTP_CODE=$($NLINK_LAB exec "$LAB_NAME" client -- \
    curl -s -o /dev/null -w "%{http_code}" -m 10 -H "Host: web.example.com" "http://$IMPORTER_IP:8080/" 2>&1 || true)

if [[ "$HTTP_CODE" == "200" ]]; then
    pass "Test 2: Host: web.example.com -> 200 OK (routed to web backend)"
else
    fail "Test 2: Expected HTTP 200, got: $HTTP_CODE"
    FAILURES=$((FAILURES + 1))
fi

# Test 3: Unknown Host header should get 502
info "Test 3: Unknown Host header returns 502..."
HTTP_CODE=$($NLINK_LAB exec "$LAB_NAME" client -- \
    curl -s -o /dev/null -w "%{http_code}" -m 10 -H "Host: unknown.example.com" "http://$IMPORTER_IP:8080/" 2>&1 || true)

if [[ "$HTTP_CODE" == "502" ]]; then
    pass "Test 3: Host: unknown.example.com -> 502 Bad Gateway"
else
    fail "Test 3: Expected HTTP 502, got: $HTTP_CODE"
    FAILURES=$((FAILURES + 1))
fi

# Test 4: Multiple requests to different backends (routing isolation)
info "Test 4: Alternating Host headers..."
CORRECT=0
for i in $(seq 1 6); do
    if (( i % 2 == 0 )); then
        HOST="api.example.com"
    else
        HOST="web.example.com"
    fi
    CODE=$($NLINK_LAB exec "$LAB_NAME" client -- \
        curl -s -o /dev/null -w "%{http_code}" -m 10 -H "Host: $HOST" "http://$IMPORTER_IP:8080/" 2>&1 || true)
    if [[ "$CODE" == "200" ]]; then
        CORRECT=$((CORRECT + 1))
    fi
done

if [[ $CORRECT -ge 5 ]]; then
    pass "Test 4: $CORRECT/6 alternating Host requests routed correctly"
else
    fail "Test 4: Only $CORRECT/6 alternating Host requests routed correctly"
    FAILURES=$((FAILURES + 1))
fi

# Test 5: Case-insensitive Host header
info "Test 5: Case-insensitive Host routing..."
HTTP_CODE=$($NLINK_LAB exec "$LAB_NAME" client -- \
    curl -s -o /dev/null -w "%{http_code}" -m 10 -H "Host: API.EXAMPLE.COM" "http://$IMPORTER_IP:8080/" 2>&1 || true)

if [[ "$HTTP_CODE" == "200" ]]; then
    pass "Test 5: Host: API.EXAMPLE.COM (uppercase) -> 200 OK"
else
    fail "Test 5: Case-insensitive routing failed, got HTTP $HTTP_CODE"
    FAILURES=$((FAILURES + 1))
fi

# --- Report ---
echo ""
echo "================================="
if [[ $FAILURES -eq 0 ]]; then
    pass "All multi-hop HTTP routing tests passed!"
    exit 0
else
    fail "$FAILURES test(s) failed"
    info "Inspect with:  nlink-lab shell $LAB_NAME client"
    exit 1
fi
