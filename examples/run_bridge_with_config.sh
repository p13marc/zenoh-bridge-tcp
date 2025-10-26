#!/bin/bash
# Example script demonstrating how to use zenoh-bridge-tcp with configuration files
# This script shows different ways to configure the bridge using JSON5 config files

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"

# Build the project if needed
echo "Building zenoh-bridge-tcp..."
cd "$PROJECT_DIR"
cargo build --release 2>/dev/null || cargo build
BRIDGE="$PROJECT_DIR/target/debug/zenoh-bridge-tcp"
if [ -f "$PROJECT_DIR/target/release/zenoh-bridge-tcp" ]; then
    BRIDGE="$PROJECT_DIR/target/release/zenoh-bridge-tcp"
fi

echo "Using bridge binary: $BRIDGE"
echo ""

# Function to show usage
show_usage() {
    echo "Usage: $0 [peer|router|client|minimal]"
    echo ""
    echo "Examples:"
    echo "  $0 peer     - Run bridge as peer with full config"
    echo "  $0 router   - Run bridge as router on port 7447"
    echo "  $0 client   - Run bridge as client connecting to localhost:7447"
    echo "  $0 minimal  - Run bridge with minimal config"
    echo ""
    exit 1
}

# Get mode from command line or default to peer
MODE="${1:-peer}"

case "$MODE" in
    peer)
        CONFIG_FILE="$SCRIPT_DIR/zenoh-config.json5"
        echo "=== Running bridge as PEER with standard configuration ==="
        echo "Config file: $CONFIG_FILE"
        echo ""
        echo "This configuration:"
        echo "  - Runs in peer mode"
        echo "  - Enables multicast scouting for peer discovery"
        echo "  - Auto-connects to discovered routers and peers"
        echo "  - Listens on a random available port"
        echo ""
        ;;

    router)
        CONFIG_FILE="$SCRIPT_DIR/zenoh-config-router.json5"
        echo "=== Running bridge as ROUTER ==="
        echo "Config file: $CONFIG_FILE"
        echo ""
        echo "This configuration:"
        echo "  - Runs in router mode"
        echo "  - Listens on standard Zenoh port 7447"
        echo "  - Acts as a message broker for peers and clients"
        echo "  - Enables peer failover brokering"
        echo ""
        ;;

    client)
        CONFIG_FILE="$SCRIPT_DIR/zenoh-config-client.json5"
        echo "=== Running bridge as CLIENT ==="
        echo "Config file: $CONFIG_FILE"
        echo ""
        echo "This configuration:"
        echo "  - Runs in client mode"
        echo "  - Connects to router at localhost:7447"
        echo "  - Requires a router to be running first"
        echo ""
        echo "NOTE: Make sure a Zenoh router is running on localhost:7447"
        echo "      You can start one with: $0 router"
        echo ""
        ;;

    minimal)
        CONFIG_FILE="$SCRIPT_DIR/zenoh-config-minimal.json5"
        echo "=== Running bridge with MINIMAL configuration ==="
        echo "Config file: $CONFIG_FILE"
        echo ""
        echo "This configuration:"
        echo "  - Only sets the mode to 'peer'"
        echo "  - Uses default values for everything else"
        echo ""
        ;;

    *)
        echo "Error: Unknown mode '$MODE'"
        echo ""
        show_usage
        ;;
esac

# Check if config file exists
if [ ! -f "$CONFIG_FILE" ]; then
    echo "Error: Config file not found: $CONFIG_FILE"
    exit 1
fi

# Show the config file content
echo "Configuration file contents:"
echo "----------------------------"
cat "$CONFIG_FILE"
echo "----------------------------"
echo ""

# Export a test service
SERVICE_NAME="testservice"
BACKEND_ADDR="127.0.0.1:9999"

echo "Starting bridge with:"
echo "  - Service export: $SERVICE_NAME -> $BACKEND_ADDR"
echo "  - Configuration: $CONFIG_FILE"
echo ""
echo "The bridge will:"
echo "  1. Load Zenoh configuration from the JSON5 file"
echo "  2. Export TCP service '$SERVICE_NAME' backed by $BACKEND_ADDR"
echo "  3. Wait for client connections over Zenoh"
echo ""
echo "To test this, start an import in another terminal:"
echo "  $BRIDGE --mode peer --import '$SERVICE_NAME/127.0.0.1:8888'"
echo ""
echo "Then connect to the imported service:"
echo "  nc localhost 8888"
echo ""
echo "Press Ctrl+C to stop the bridge"
echo ""

# Run the bridge
exec "$BRIDGE" \
    --config "$CONFIG_FILE" \
    --export "$SERVICE_NAME/$BACKEND_ADDR"
