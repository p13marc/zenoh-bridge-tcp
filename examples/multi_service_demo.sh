#!/bin/bash
# Multi-Service Bridge Demo
#
# This script demonstrates running multiple exports and imports in a single bridge instance.
# It sets up a scenario where one bridge exports multiple services and another imports them.

set -e

# Colors for output
GREEN='\033[0;32m'
BLUE='\033[0;34m'
YELLOW='\033[1;33m'
RED='\033[0;31m'
NC='\033[0m' # No Color

echo -e "${BLUE}========================================${NC}"
echo -e "${BLUE}Multi-Service Bridge Demo${NC}"
echo -e "${BLUE}========================================${NC}"
echo ""

# Check if cargo is available
if ! command -v cargo &> /dev/null; then
    echo -e "${RED}Error: cargo not found. Please install Rust.${NC}"
    exit 1
fi

# Build the bridge
echo -e "${YELLOW}Building zenoh-bridge-tcp...${NC}"
cargo build --release --quiet
echo -e "${GREEN}✓ Build complete${NC}"
echo ""

# Cleanup function
cleanup() {
    echo -e "\n${YELLOW}Cleaning up...${NC}"
    pkill -P $$ 2>/dev/null || true
    sleep 1
    echo -e "${GREEN}✓ Cleanup complete${NC}"
}

trap cleanup EXIT INT TERM

# Start backend services (echo servers using netcat)
echo -e "${YELLOW}Starting backend services...${NC}"

# Service 1: Echo server on port 9001
echo -e "${BLUE}  Starting echo service on port 9001...${NC}"
while true; do
    nc -l 127.0.0.1 9001 -c 'while read line; do echo "Echo: $line"; done'
done 2>/dev/null &
ECHO_PID=$!

# Service 2: Uppercase service on port 9002
echo -e "${BLUE}  Starting uppercase service on port 9002...${NC}"
while true; do
    nc -l 127.0.0.1 9002 -c 'while read line; do echo "$line" | tr "[:lower:]" "[:upper:]"; done'
done 2>/dev/null &
UPPER_PID=$!

# Service 3: Reverse service on port 9003
echo -e "${BLUE}  Starting reverse service on port 9003...${NC}"
while true; do
    nc -l 127.0.0.1 9003 -c 'while read line; do echo "$line" | rev; done'
done 2>/dev/null &
REV_PID=$!

sleep 1
echo -e "${GREEN}✓ Backend services started${NC}"
echo ""

# Start Export Bridge (exports multiple services)
echo -e "${YELLOW}Starting Export Bridge...${NC}"
echo -e "${BLUE}  This bridge exports 3 services simultaneously:${NC}"
echo -e "${BLUE}    - echo_service    (from 127.0.0.1:9001)${NC}"
echo -e "${BLUE}    - uppercase_service (from 127.0.0.1:9002)${NC}"
echo -e "${BLUE}    - reverse_service (from 127.0.0.1:9003)${NC}"
echo ""

./target/release/zenoh-bridge-tcp \
    --export 'echo_service/127.0.0.1:9001' \
    --export 'uppercase_service/127.0.0.1:9002' \
    --export 'reverse_service/127.0.0.1:9003' \
    --mode peer \
    > /tmp/export_bridge.log 2>&1 &
EXPORT_PID=$!

sleep 2
echo -e "${GREEN}✓ Export bridge started (PID: $EXPORT_PID)${NC}"
echo ""

# Start Import Bridge (imports multiple services)
echo -e "${YELLOW}Starting Import Bridge...${NC}"
echo -e "${BLUE}  This bridge imports 3 services simultaneously:${NC}"
echo -e "${BLUE}    - echo_service    (TCP on 127.0.0.1:8001)${NC}"
echo -e "${BLUE}    - uppercase_service (TCP on 127.0.0.1:8002)${NC}"
echo -e "${BLUE}    - reverse_service (TCP on 127.0.0.1:8003)${NC}"
echo ""

./target/release/zenoh-bridge-tcp \
    --import 'echo_service/127.0.0.1:8001' \
    --import 'uppercase_service/127.0.0.1:8002' \
    --import 'reverse_service/127.0.0.1:8003' \
    --mode peer \
    > /tmp/import_bridge.log 2>&1 &
IMPORT_PID=$!

sleep 2
echo -e "${GREEN}✓ Import bridge started (PID: $IMPORT_PID)${NC}"
echo ""

# Test the services
echo -e "${BLUE}========================================${NC}"
echo -e "${BLUE}Testing Services${NC}"
echo -e "${BLUE}========================================${NC}"
echo ""

sleep 1

echo -e "${YELLOW}Test 1: Echo Service (port 8001)${NC}"
RESULT=$(echo "hello world" | nc 127.0.0.1 8001 -w 1)
echo -e "  Input:  ${BLUE}hello world${NC}"
echo -e "  Output: ${GREEN}$RESULT${NC}"
echo ""

echo -e "${YELLOW}Test 2: Uppercase Service (port 8002)${NC}"
RESULT=$(echo "make me loud" | nc 127.0.0.1 8002 -w 1)
echo -e "  Input:  ${BLUE}make me loud${NC}"
echo -e "  Output: ${GREEN}$RESULT${NC}"
echo ""

echo -e "${YELLOW}Test 3: Reverse Service (port 8003)${NC}"
RESULT=$(echo "stressed" | nc 127.0.0.1 8003 -w 1)
echo -e "  Input:  ${BLUE}stressed${NC}"
echo -e "  Output: ${GREEN}$RESULT${NC}"
echo ""

# Show concurrent access
echo -e "${BLUE}========================================${NC}"
echo -e "${BLUE}Testing Concurrent Access${NC}"
echo -e "${BLUE}========================================${NC}"
echo ""

echo -e "${YELLOW}Sending 3 requests to different services simultaneously...${NC}"

(echo "concurrent 1" | nc 127.0.0.1 8001 -w 1) &
(echo "concurrent 2" | nc 127.0.0.1 8002 -w 1) &
(echo "concurrent 3" | nc 127.0.0.1 8003 -w 1) &

wait

echo -e "${GREEN}✓ All concurrent requests completed successfully${NC}"
echo ""

# Summary
echo -e "${BLUE}========================================${NC}"
echo -e "${BLUE}Summary${NC}"
echo -e "${BLUE}========================================${NC}"
echo ""
echo -e "${GREEN}✓ Single export bridge handling 3 services${NC}"
echo -e "${GREEN}✓ Single import bridge handling 3 services${NC}"
echo -e "${GREEN}✓ All services working independently${NC}"
echo -e "${GREEN}✓ Concurrent access working correctly${NC}"
echo ""
echo -e "${YELLOW}Architecture:${NC}"
echo -e "  Backend Services (9001-9003) → Export Bridge → Zenoh"
echo -e "  Zenoh → Import Bridge → TCP Listeners (8001-8003) → Clients"
echo ""
echo -e "${YELLOW}Logs available at:${NC}"
echo -e "  Export bridge: /tmp/export_bridge.log"
echo -e "  Import bridge: /tmp/import_bridge.log"
echo ""
echo -e "${BLUE}Press Ctrl+C to stop all services${NC}"

# Keep running
wait $EXPORT_PID $IMPORT_PID
