# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

Zenoh TCP Bridge is a bidirectional bridge that connects TCP services to the Zenoh distributed data bus. It allows exposing TCP backends as Zenoh services (export mode) or making Zenoh services accessible via TCP listeners (import mode).

Key features:
- **Export Mode**: Expose TCP backend services over Zenoh
- **Import Mode**: Make Zenoh services accessible via TCP listeners
- **HTTP/HTTPS Routing**: DNS-based routing using Host header (HTTP) or SNI (HTTPS)
- **WebSocket Support**: Bridge WebSocket backends with `--ws-export` and `--ws-import`
- **Auto-Import**: Protocol auto-detection (TLS/HTTP/WebSocket/raw TCP) with `--auto-import`
- **HTTP Multiroute**: Per-request Host routing on persistent connections with `--http-multiroute-import`
- **TLS Termination**: Optional HTTPS termination with `--https-terminate` (feature: `tls-termination`)
- **Liveliness Detection**: Automatic client presence tracking via Zenoh liveliness tokens
- **Configurable Logging**: `--log-level` and `--log-format` (pretty/compact/json)

## Build Commands

```bash
# Build (release recommended)
cargo build --release

# Build with TLS termination feature
cargo build --release --features tls-termination

# Run the bridge
cargo run --release -- --export 'service/127.0.0.1:8003' --import 'service/127.0.0.1:8002'

# Run with HTTP routing
cargo run --release -- --http-export 'http-svc/api.example.com/127.0.0.1:8000' --http-import 'http-svc/0.0.0.0:8080'

# Run with WebSocket
cargo run --release -- --ws-export 'ws-svc/ws://127.0.0.1:9000' --ws-import 'ws-svc/0.0.0.0:8080'

# With logging options
cargo run --release -- --log-level debug --log-format json --export 'service/127.0.0.1:8003'
```

## Testing

```bash
# Run all tests (nextest recommended for isolation)
cargo nextest run

# Run unit tests only
cargo test --lib

# Run specific integration test suites
cargo nextest run --test http_routing_integration
cargo nextest run --test https_routing_integration
```

## Linting and Formatting

```bash
cargo fmt
cargo clippy
cargo deny check  # Check dependencies
```

## Architecture

### Crate Structure

Single crate with library and binary:
- `src/lib.rs` - Library entry point, re-exports modules
- `src/main.rs` - CLI entry point, spawns export/import tasks
- `src/args.rs` - Command-line argument parsing (clap)
- `src/config.rs` - Zenoh and bridge configuration (BridgeConfig)
- `src/error.rs` - Structured error types (BridgeError)
- `src/export.rs` - Export mode: TCP/WebSocket backend -> Zenoh
- `src/import.rs` - Import mode: Zenoh -> TCP/WebSocket listener
- `src/http_parser.rs` - HTTP request parsing, Host header extraction
- `src/http_response_parser.rs` - HTTP response body framing detection
- `src/tls_parser.rs` - TLS ClientHello parsing, SNI extraction
- `src/tls_config.rs` - TLS configuration loading (for `tls-termination` feature)
- `src/protocol_detect.rs` - Protocol auto-detection (TLS/HTTP/WebSocket/TCP)

### CLI Arguments

Core modes:
- `--export`, `--import` - Raw TCP bridging
- `--http-export`, `--http-import` - HTTP/HTTPS DNS-based routing
- `--ws-export`, `--ws-import` - WebSocket bridging
- `--auto-import` - Auto-detect protocol per connection
- `--http-multiroute-import` - Per-request HTTP/1.1 routing with keep-alive
- `--https-terminate` - HTTPS import with TLS termination (requires `--tls-cert` and `--tls-key`)

Configuration:
- `--buffer-size` (default: 65536), `--read-timeout` (default: 10), `--drain-timeout` (default: 5)
- `--mode`, `--connect`, `--listen` - Zenoh network config
- `--log-level`, `--log-format` - Logging

Feature flag: `tls-termination` - Enables `--https-terminate`, `--tls-cert`, `--tls-key`

### Data Flow

**Export Mode** (backend -> Zenoh):
1. Monitors liveliness tokens at `{service}/clients/*`
2. When client appears, connects to TCP backend
3. Subscribes to `{service}/tx/{client_id}` (client -> backend)
4. Publishes to `{service}/rx/{client_id}` (backend -> client)

**Import Mode** (Zenoh -> TCP listener):
1. Accepts TCP connections, assigns unique client_id
2. Declares liveliness token at `{service}/clients/{client_id}`
3. Publishes to `{service}/tx/{client_id}` (client -> backend)
4. Subscribes to `{service}/rx/{client_id}` (backend -> client)

**HTTP/HTTPS Mode** adds DNS routing:
- Key pattern becomes `{service}/{dns}/tx/{client_id}` etc.
- DNS extracted from HTTP Host header or TLS SNI
- Backends register availability at `{service}/{dns}/available`

### Key Zenoh Patterns

Uses `zenoh-ext` AdvancedPublisher/Subscriber for reliability:
- **AdvancedPublisher**: Cache + publisher detection + heartbeat
- **AdvancedSubscriber**: History + late publisher detection + recovery
- **Liveliness**: Client presence tracking, backend availability signals

## Dependencies

- `zenoh` 1.6.2 - Zenoh distributed data bus
- `zenoh-ext` - Extended pub/sub with reliability features
- `tokio` - Async runtime
- `tokio-util` - CancellationToken for graceful shutdown
- `clap` - CLI parsing
- `anyhow` / `thiserror` - Error handling
- `tracing` / `tracing-subscriber` - Structured logging with JSON support
- `httparse` - HTTP/1.x header parsing
- `tls-parser` - TLS ClientHello/SNI parsing
- `tokio-tungstenite` - WebSocket support
- `futures-util` - Async stream utilities
- `backon` - Retry with exponential backoff
- `uuid` - Unique client ID generation
