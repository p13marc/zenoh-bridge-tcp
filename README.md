# Zenoh TCP Bridge

A bidirectional bridge that connects TCP services to the Zenoh distributed data bus. Export TCP backends as Zenoh services or import Zenoh services as TCP listeners.

## Overview

This bridge enables:
- **Export Mode**: Expose TCP backend services (e.g., databases, APIs) over Zenoh
- **Import Mode**: Make Zenoh services accessible via TCP listeners
- **Multiple Services**: Run multiple exports and imports simultaneously in a single bridge instance
- **Automatic Connection Management**: Lazy connections with liveliness detection

## Features

### Core Functionality
- **Lazy Connections**: Backend connections are created only when clients connect (export mode)
- **Per-Client Isolation**: Each client gets dedicated Zenoh pub/sub channels and backend connection
- **Concurrent Services**: Handle multiple services in one bridge process
- **Flexible Configuration**: Command-line arguments or Zenoh config files

### Liveliness Detection
- Automatic client presence tracking using Zenoh liveliness tokens
- Clean disconnection handling and resource cleanup
- Backend connection lifecycle tied to client presence

### Multiple Zenoh Modes
- **Peer Mode** (default): Direct peer-to-peer communication
- **Client Mode**: Connect to existing Zenoh routers
- **Router Mode**: Act as a Zenoh router

## Architecture

### Export Mode
```
TCP Backend (e.g., HTTP server on :8003)
    ↕
Zenoh Bridge (Export)
    ↕
Zenoh Network
    ↕
Zenoh Bridge (Import)
    ↕
TCP Client connects to :8002
```

The exporter:
1. Monitors for client liveliness tokens on `{service}/clients/{client_id}`
2. Creates backend connection when client appears
3. Subscribes to `{service}/tx/{client_id}` (data from client)
4. Publishes to `{service}/rx/{client_id}` (data to client)
5. Cleans up when client disconnects

### Import Mode

The importer:
1. Listens for TCP connections on specified address
2. For each connection, creates unique client ID
3. Declares liveliness token at `{service}/clients/{client_id}`
4. Publishes to `{service}/tx/{client_id}` (data from TCP client)
5. Subscribes to `{service}/rx/{client_id}` (data to TCP client)
6. Undeclares liveliness on disconnection

## Quick Start

### Export a TCP Service

Make a local HTTP server accessible over Zenoh:

```bash
# Terminal 1: Start your HTTP server
python3 -m http.server 8003

# Terminal 2: Export it as "webserver" service
zenoh-bridge-tcp --export 'webserver/127.0.0.1:8003'
```

### Import a Zenoh Service

Make the Zenoh service accessible via TCP:

```bash
# Terminal 3: Import "webserver" and listen on :8002
zenoh-bridge-tcp --import 'webserver/127.0.0.1:8002'

# Terminal 4: Test with curl
curl http://127.0.0.1:8002
```

### Multiple Services

Run multiple exports/imports in one bridge:

```bash
zenoh-bridge-tcp \
  --export 'api/127.0.0.1:3000' \
  --export 'db/127.0.0.1:5432' \
  --import 'frontend/0.0.0.0:8080' \
  --mode peer
```

## Building

### Prerequisites
- Rust 1.70 or later
- Optional: `protoc` for gRPC integration tests

### Build
```bash
cargo build --release
```

The binary will be at `target/release/zenoh-bridge-tcp`.

### Run Tests
```bash
# Run all tests
cargo test

# Run specific test suite
cargo test --test export_import_integration -- --test-threads=1
cargo test --test bridge_integration
cargo test --test grpc_integration
```

## Usage

### Command Line Options

```
zenoh-bridge-tcp [OPTIONS]

Options:
  -c, --config <FILE>         Path to Zenoh configuration file (JSON5)
      --export <SPEC>         Export TCP backend as Zenoh service
                              Format: 'service_name/backend_addr'
                              Example: 'myapi/127.0.0.1:3000'
                              Can be specified multiple times
      --import <SPEC>         Import Zenoh service as TCP listener
                              Format: 'service_name/listen_addr'
                              Example: 'myapi/0.0.0.0:8080'
                              Can be specified multiple times
  -m, --mode <MODE>           Zenoh mode: peer, client, or router [default: peer]
  -e, --connect <ENDPOINT>    Zenoh connect endpoint (e.g., tcp/localhost:7447)
  -l, --listen <ENDPOINT>     Zenoh listen endpoint (e.g., tcp/0.0.0.0:7447)
  -h, --help                  Print help
  -V, --version               Print version
```

### Export Specification Format

```
service_name/backend_address:port
```

Examples:
- `webserver/127.0.0.1:8080` - Export local web server
- `api/backend-host:3000` - Export remote API
- `postgres/localhost:5432` - Export PostgreSQL database

### Import Specification Format

```
service_name/listen_address:port
```

Examples:
- `webserver/0.0.0.0:8080` - Listen on all interfaces
- `api/127.0.0.1:3000` - Listen only on localhost
- `service/[::]:8080` - Listen on IPv6

### Configuration File

Use a Zenoh configuration file for advanced settings:

```bash
zenoh-bridge-tcp \
  --config examples/zenoh-config.json5 \
  --export 'myservice/127.0.0.1:8003' \
  --import 'myservice/0.0.0.0:8002'
```

Example configuration files are in the `examples/` directory:
- `zenoh-config-minimal.json5` - Minimal peer configuration
- `zenoh-config-client.json5` - Client mode connecting to router
- `zenoh-config-router.json5` - Router mode configuration
- `zenoh-config.json5` - Full-featured example

## Examples

### Example 1: HTTP Service Bridge

```bash
# Terminal 1: Start HTTP server
python3 -m http.server 8003

# Terminal 2: Export side
zenoh-bridge-tcp --export 'http/127.0.0.1:8003'

# Terminal 3: Import side (can be on different machine)
zenoh-bridge-tcp --import 'http/127.0.0.1:8002' --connect tcp/exporter-host:7447

# Terminal 4: Test
curl http://127.0.0.1:8002
```

### Example 2: Netcat Echo Test

```bash
# Terminal 1: Start netcat server
nc -l 8003

# Terminal 2: Export
zenoh-bridge-tcp --export 'echo/127.0.0.1:8003'

# Terminal 3: Import
zenoh-bridge-tcp --import 'echo/127.0.0.1:8002'

# Terminal 4: Client
nc 127.0.0.1 8002
# Type messages - they go through Zenoh to the server
```

### Example 3: Multiple Services

```bash
zenoh-bridge-tcp \
  --export 'web/127.0.0.1:8080' \
  --export 'api/127.0.0.1:3000' \
  --import 'frontend/0.0.0.0:9001' \
  --import 'admin/0.0.0.0:9002' \
  --mode peer
```

### Example 4: With Zenoh Router

```bash
# Terminal 1: Start Zenoh router
zenohd

# Terminal 2: Export side (client mode)
zenoh-bridge-tcp \
  --export 'service/127.0.0.1:8003' \
  --mode client \
  --connect tcp/localhost:7447

# Terminal 3: Import side (client mode)
zenoh-bridge-tcp \
  --import 'service/0.0.0.0:8002' \
  --mode client \
  --connect tcp/localhost:7447
```

## Docker Deployment

Build and run with Docker:

```bash
# Build image
docker build -t zenoh-bridge-tcp .

# Run with docker-compose
docker-compose up -d

# Test multi-bridge setup
docker-compose --profile multi-bridge up -d
```

See [examples/DOCKER.md](examples/DOCKER.md) for detailed Docker deployment instructions.

## Testing

The project includes comprehensive integration tests:

- **`tests/export_import_integration.rs`** - Core export/import functionality
- **`tests/bridge_integration.rs`** - Basic bridge operations
- **`tests/grpc_integration.rs`** - gRPC service bridging (requires protoc)
- **`tests/http_integration.rs`** - HTTP/HTTPS service bridging
- **`tests/liveliness_integration.rs`** - Liveliness detection
- **`tests/multi_service_integration.rs`** - Multiple concurrent services

Run tests:

```bash
# All tests
cargo test

# Export/import tests (recommended: single-threaded)
cargo test --test export_import_integration -- --test-threads=1 --nocapture

# HTTP tests
cargo test --test http_integration -- --nocapture

# gRPC tests (requires protoc)
cargo test --test grpc_integration -- --nocapture
```

See [tests/README.md](tests/README.md) for detailed testing documentation.

## Zenoh Key Expression Design

The bridge uses a structured key expression pattern:

```
{service_name}/tx/{client_id}      # Client → Backend data
{service_name}/rx/{client_id}      # Backend → Client data
{service_name}/clients/{client_id} # Liveliness token
```

Each TCP connection gets a unique `client_id` (UUID), ensuring isolation between clients.

## Logging

Control log verbosity with `RUST_LOG`:

```bash
# Info level (default)
RUST_LOG=info zenoh-bridge-tcp --export 'service/127.0.0.1:8003'

# Debug level
RUST_LOG=debug zenoh-bridge-tcp --export 'service/127.0.0.1:8003'

# Trace level (very verbose)
RUST_LOG=trace zenoh-bridge-tcp --export 'service/127.0.0.1:8003'

# Module-specific
RUST_LOG=zenoh_bridge_tcp=debug,zenoh=info zenoh-bridge-tcp --export 'service/127.0.0.1:8003'
```

## Performance Considerations

- **Buffer Size**: 4KB default per connection
- **Concurrent Connections**: Limited by system resources (file descriptors, memory)
- **Latency**: Adds ~1-2ms overhead vs direct TCP (depends on Zenoh setup)
- **Throughput**: Tested with HTTP, gRPC, and raw TCP; handles typical workloads well

## Use Cases

- **Service Discovery**: Expose services without static IP addresses
- **Network Abstraction**: Abstract away network topology
- **Cloud-Edge Bridging**: Connect edge devices to cloud services via Zenoh
- **Legacy Integration**: Make TCP services Zenoh-native
- **Multi-Region Deployment**: Leverage Zenoh's peer-to-peer or routed mesh
- **Protocol Bridging**: Connect TCP clients to Zenoh-based backends

## Dependencies

Core dependencies:
- `zenoh` 1.6.2 - Zenoh distributed data bus
- `tokio` - Async runtime
- `clap` - Command-line parsing
- `anyhow` - Error handling
- `tracing` - Structured logging

Development/test dependencies include: `tonic`, `axum`, `hyper`, `rustls` for protocol testing.

## Version Information

- **Current Version**: 0.1.0
- **Zenoh Version**: 1.6.2
- **Rust Edition**: 2021
- **MSRV**: 1.70 (not officially declared, but known to work)

## Migration Notes

If migrating from an older Zenoh version:
- Zenoh 1.x uses different API than 0.x versions
- Key expressions remain compatible
- Session configuration has changed - see examples for current format

## Quality Tools

The project uses standard Rust quality tools:

```bash
# Format code
cargo fmt

# Lint
cargo clippy

# Check for issues
cargo deny check
```

Configuration files:
- `.cargo/config.toml` - Cargo settings
- `deny.toml` - Dependency auditing

## Contributing

Contributions welcome! Please:

1. Run `cargo fmt` before committing
2. Ensure `cargo clippy` passes
3. Add tests for new features
4. Update documentation as needed

## License

Licensed under either of:
- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or http://www.apache.org/licenses/LICENSE-2.0)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or http://opensource.org/licenses/MIT)

at your option.

## Additional Resources

- [Zenoh Documentation](https://zenoh.io/docs/)
- [Zenoh GitHub](https://github.com/eclipse-zenoh/zenoh)
- [Docker Deployment Guide](examples/DOCKER.md)
- [Test Documentation](tests/README.md)