# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

Zenoh TCP Bridge is a bidirectional bridge that connects TCP services to the Zenoh distributed data bus. It allows exposing TCP backends as Zenoh services (export mode) or making Zenoh services accessible via TCP listeners (import mode).

Key features:
- **Export Mode**: Expose TCP backend services over Zenoh
- **Import Mode**: Make Zenoh services accessible via TCP listeners
- **HTTP/HTTPS Routing**: DNS-based routing using Host header (HTTP) or SNI (HTTPS)
- **Liveliness Detection**: Automatic client presence tracking via Zenoh liveliness tokens

## Build Commands

```bash
# Build (release recommended)
cargo build --release

# Run the bridge
cargo run --release -- --export 'service/127.0.0.1:8003' --import 'service/127.0.0.1:8002'

# Run with HTTP routing
cargo run --release -- --http-export 'http-svc/api.example.com/127.0.0.1:8000' --http-import 'http-svc/0.0.0.0:8080'
```

## Testing

```bash
# Run all tests (nextest recommended for isolation)
cargo nextest run

# Run with cargo test (some tests need single-threaded execution)
cargo test --test export_import_integration -- --test-threads=1
cargo test --test http_edge_cases -- --test-threads=1

# Run unit tests only
cargo test --lib

# Run specific integration test suites
cargo test --test http_routing_integration
cargo test --test https_routing_integration
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
- `src/config.rs` - Zenoh configuration handling
- `src/export.rs` - Export mode: TCP backend -> Zenoh
- `src/import.rs` - Import mode: Zenoh -> TCP listener
- `src/http_parser.rs` - HTTP request parsing, Host header extraction
- `src/tls_parser.rs` - TLS ClientHello parsing, SNI extraction

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
- `clap` - CLI parsing
- `httparse` - HTTP/1.x header parsing
- `tls-parser` - TLS ClientHello/SNI parsing
