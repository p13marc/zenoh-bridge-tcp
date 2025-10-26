#!/bin/bash
#
# Example script to run the Zenoh TCP Bridge with common configurations
#
# Usage:
#   ./run_bridge.sh [config_name]
#
# Available configs:
#   default   - Basic peer mode on localhost:7447
#   public    - Listen on all interfaces
#   client    - Connect to remote router
#   custom    - Custom topic names
#   debug     - Enable debug logging
#

set -e

# Configuration presets
config_default() {
    echo "Running bridge with default configuration..."
    cargo run --release -- \
        --tcp-listen 127.0.0.1:7447 \
        --pub-key tcp/data/tx \
        --sub-key tcp/data/rx \
        --mode peer
}

config_public() {
    echo "Running bridge listening on all interfaces..."
    cargo run --release -- \
        --tcp-listen 0.0.0.0:7447 \
        --pub-key tcp/data/tx \
        --sub-key tcp/data/rx \
        --mode peer
}

config_client() {
    echo "Running bridge in client mode..."

    # You need to specify your router address
    ROUTER_ADDRESS="${ZENOH_ROUTER:-tcp/localhost:7447}"

    echo "Connecting to router: $ROUTER_ADDRESS"
    cargo run --release -- \
        --tcp-listen 127.0.0.1:7447 \
        --pub-key tcp/data/tx \
        --sub-key tcp/data/rx \
        --mode client \
        --connect "$ROUTER_ADDRESS"
}

config_custom() {
    echo "Running bridge with custom topic names..."
    cargo run --release -- \
        --tcp-listen 127.0.0.1:9000 \
        --pub-key sensors/temperature/raw \
        --sub-key actuators/hvac/commands \
        --mode peer
}

config_debug() {
    echo "Running bridge with debug logging enabled..."
    RUST_LOG=debug cargo run --release -- \
        --tcp-listen 127.0.0.1:7447 \
        --pub-key tcp/data/tx \
        --sub-key tcp/data/rx \
        --mode peer
}

config_router() {
    echo "Running bridge with router mode (requires zenoh router capabilities)..."
    cargo run --release -- \
        --tcp-listen 0.0.0.0:7447 \
        --pub-key tcp/data/tx \
        --sub-key tcp/data/rx \
        --mode router \
        --listen tcp/0.0.0.0:7448
}

config_multi_port() {
    echo "This configuration requires running multiple instances in separate terminals:"
    echo ""
    echo "Terminal 1 (Port 8001):"
    echo "  cargo run --release -- --tcp-listen 127.0.0.1:8001 --pub-key service/a/tx --sub-key service/b/tx"
    echo ""
    echo "Terminal 2 (Port 8002):"
    echo "  cargo run --release -- --tcp-listen 127.0.0.1:8002 --pub-key service/b/tx --sub-key service/a/tx"
    echo ""
    echo "Connect to 8001 and 8002 with separate clients - they can communicate through Zenoh!"
}

# Display usage information
show_usage() {
    echo "Usage: $0 [config_name]"
    echo ""
    echo "Available configurations:"
    echo "  default    - Basic peer mode on localhost:7447 (default)"
    echo "  public     - Listen on all interfaces (0.0.0.0:7447)"
    echo "  client     - Connect to remote Zenoh router"
    echo "  custom     - Custom topic names example"
    echo "  debug      - Enable debug logging"
    echo "  router     - Run as Zenoh router"
    echo "  multi-port - Instructions for multi-port setup"
    echo ""
    echo "Environment variables:"
    echo "  ZENOH_ROUTER - Router address for client mode (default: tcp/localhost:7447)"
    echo ""
    echo "Examples:"
    echo "  $0                    # Run with default config"
    echo "  $0 public             # Listen on all interfaces"
    echo "  $0 debug              # Run with debug logging"
    echo "  ZENOH_ROUTER=tcp/192.168.1.100:7447 $0 client"
    echo ""
}

# Main script logic
CONFIG="${1:-default}"

case "$CONFIG" in
    default)
        config_default
        ;;
    public)
        config_public
        ;;
    client)
        config_client
        ;;
    custom)
        config_custom
        ;;
    debug)
        config_debug
        ;;
    router)
        config_router
        ;;
    multi-port|multiport)
        config_multi_port
        ;;
    -h|--help|help)
        show_usage
        exit 0
        ;;
    *)
        echo "Error: Unknown configuration '$CONFIG'"
        echo ""
        show_usage
        exit 1
        ;;
esac
