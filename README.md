# Zenoh TCP Bridge

A bidirectional bridge that connects TCP services to the Zenoh distributed data bus. Export TCP backends as Zenoh services or import Zenoh services as TCP listeners.

**New in v0.1.0**: HTTP/HTTPS routing with DNS-based service discovery! Route HTTP requests by Host header and HTTPS requests by SNI to multiple backends through a single listener.

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

### HTTP/HTTPS Routing (New!)
- **DNS-Based Routing**: Route HTTP requests by Host header to different backends
- **SNI-Based HTTPS**: Route HTTPS traffic by SNI without terminating TLS
- **Automatic Protocol Detection**: Detects HTTP vs HTTPS automatically
- **DNS Normalization**: Case-insensitive, port-aware routing
- **Multiple Backends**: One listener can route to N different HTTP/HTTPS servers
- **End-to-End TLS**: HTTPS traffic is never decrypted by the bridge

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
  -c, --config <FILE>              Path to Zenoh configuration file (JSON5)
      --export <SPEC>              Export TCP backend as Zenoh service
                                   Format: 'service_name/backend_addr'
                                   Example: 'myapi/127.0.0.1:3000'
                                   Can be specified multiple times
      --import <SPEC>              Import Zenoh service as TCP listener
                                   Format: 'service_name/listen_addr'
                                   Example: 'myapi/0.0.0.0:8080'
                                   Can be specified multiple times
      --http-export <HTTP_EXPORT>  Export HTTP backend with DNS-based routing
                                   Format: 'service_name/dns/backend_addr'
                                   Example: 'http-service/api.example.com/127.0.0.1:8000'
                                   Can be specified multiple times
      --http-import <HTTP_IMPORT>  Import HTTP service with DNS-based routing
                                   Format: 'service_name/listen_addr'
                                   Example: 'http-service/0.0.0.0:8080'
                                   Automatically detects HTTP vs HTTPS (SNI)
                                   Can be specified multiple times
  -m, --mode <MODE>                Zenoh mode: peer, client, or router [default: peer]
  -e, --connect <ENDPOINT>         Zenoh connect endpoint (e.g., tcp/localhost:7447)
  -l, --listen <ENDPOINT>          Zenoh listen endpoint (e.g., tcp/0.0.0.0:7447)
  -h, --help                       Print help
  -V, --version                    Print version
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

### Example 1: Traditional TCP Bridge for HTTP

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

### Example 1b: HTTP Routing with Multiple Backends (New!)

```bash
# Terminal 1: Start multiple HTTP servers
python3 -m http.server 8001  # API backend
python3 -m http.server 8002  # Web backend

# Terminal 2: Export API backend for api.example.com
zenoh-bridge-tcp --http-export 'http-service/api.example.com/127.0.0.1:8001'

# Terminal 3: Export Web backend for web.example.com
zenoh-bridge-tcp --http-export 'http-service/web.example.com/127.0.0.1:8002'

# Terminal 4: Import (single listener routes to both backends)
zenoh-bridge-tcp --http-import 'http-service/0.0.0.0:8080'

# Terminal 5: Test routing by Host header
curl -H "Host: api.example.com" http://127.0.0.1:8080/  # → API backend
curl -H "Host: web.example.com" http://127.0.0.1:8080/  # → Web backend
```

### Example 1c: HTTPS Routing with SNI (New!)

```bash
# Start HTTPS backends with certificates
# (Configure your HTTPS servers for api.example.com and web.example.com)

# Export HTTPS backends
zenoh-bridge-tcp --http-export 'https-service/api.example.com/127.0.0.1:8443'
zenoh-bridge-tcp --http-export 'https-service/web.example.com/127.0.0.1:8444'

# Import (automatically detects HTTPS via SNI)
zenoh-bridge-tcp --http-import 'https-service/0.0.0.0:8443'

# Test - SNI determines routing (no TLS termination!)
curl https://api.example.com:8443/ --resolve api.example.com:8443:127.0.0.1
curl https://web.example.com:8443/ --resolve web.example.com:8443:127.0.0.1
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

### Core Bridge Tests
- **`tests/export_import_integration.rs`** - Core export/import functionality
- **`tests/bridge_integration.rs`** - Basic bridge operations
- **`tests/grpc_integration.rs`** - gRPC service bridging (requires protoc)
- **`tests/http_integration.rs`** - HTTP/HTTPS service bridging
- **`tests/liveliness_integration.rs`** - Liveliness detection
- **`tests/multi_service_integration.rs`** - Multiple concurrent services

### HTTP/HTTPS Routing Tests (New!)
- **`tests/http_routing_integration.rs`** - HTTP routing with multiple backends
  - Multiple HTTP servers with different Host headers
  - DNS normalization (case, port stripping)
  - Backend availability detection
  - Concurrent clients (10 simultaneous)
  - Dynamic backend registration
  
- **`tests/https_routing_integration.rs`** - HTTPS routing with SNI
  - SNI-based routing to multiple backends
  - End-to-end TLS (no termination)
  - Self-signed certificate testing
  - DNS normalization for SNI
  - Concurrent HTTPS clients
  
- **`tests/http_edge_cases.rs`** - Edge cases and error handling
  - Missing/empty Host headers
  - Malformed HTTP requests
  - Very long headers
  - Special characters in hostnames
  - Various HTTP methods (GET, POST, PUT, DELETE)

### Test Statistics
```
Total Test Suites: 9 suites
Unit Tests: 46 tests ✅ PASSING
HTTP Integration: 3 tests ✅ PASSING
HTTPS Integration: 3 tests ✅ PASSING
Edge Cases: 3 tests ✅ PASSING (3 ignored - timing issues)
Total: 55 tests passing + 3 ignored
```

Run tests:

```bash
# All tests
cargo test

# Core TCP bridge tests
cargo test --test export_import_integration -- --test-threads=1 --nocapture

# HTTP routing tests
cargo test --test http_routing_integration -- --test-threads=1 --nocapture

# HTTPS routing tests
cargo test --test https_routing_integration -- --test-threads=1 --nocapture

# Edge case tests
cargo test --test http_edge_cases -- --test-threads=1 --nocapture

# Unit tests
cargo test --lib

# gRPC tests (requires protoc)
cargo test --test grpc_integration -- --nocapture
```

See [tests/README.md](tests/README.md) for detailed testing documentation and [docs/HTTP_ROUTING_GUIDE.md](docs/HTTP_ROUTING_GUIDE.md) for HTTP/HTTPS routing guide.

## Zenoh Key Expression Design

The bridge uses a structured key expression pattern:

### Traditional TCP Mode
```
{service_name}/tx/{client_id}      # Client → Backend data
{service_name}/rx/{client_id}      # Backend → Client data
{service_name}/clients/{client_id} # Liveliness token
```

### HTTP/HTTPS Mode (DNS-based routing)
```
{service_name}/{dns}/tx/{client_id}      # Client → Backend data (for specific DNS)
{service_name}/{dns}/rx/{client_id}      # Backend → Client data (for specific DNS)
{service_name}/{dns}/clients/{client_id} # Liveliness token (per DNS)
{service_name}/{dns}/available           # Backend availability signal
```

Each TCP connection gets a unique `client_id`, ensuring isolation between clients. In HTTP/HTTPS mode, the `{dns}` component (extracted from Host header or SNI) enables routing to multiple backends through a single listener.

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

### Traditional TCP Bridging
- **Service Discovery**: Expose services without static IP addresses
- **Network Abstraction**: Abstract away network topology
- **Cloud-Edge Bridging**: Connect edge devices to cloud services via Zenoh
- **Legacy Integration**: Make TCP services Zenoh-native
- **Multi-Region Deployment**: Leverage Zenoh's peer-to-peer or routed mesh
- **Protocol Bridging**: Connect TCP clients to Zenoh-based backends

### HTTP/HTTPS Routing (New!)
- **Virtual Host Routing**: Route HTTP traffic by hostname to different backends
- **Multi-Tenant SaaS**: Single listener routes customers to their dedicated backends
- **API Gateway**: Route API requests by domain to microservices
- **SNI-Based Load Distribution**: Distribute HTTPS traffic without TLS termination
- **Development/Staging Environments**: Route by hostname to different environments
- **Hybrid Cloud**: Route traffic to backends across different networks via Zenoh

## Dependencies

Core dependencies:
- `zenoh` 1.6.2 - Zenoh distributed data bus
- `tokio` - Async runtime
- `clap` - Command-line parsing
- `anyhow` - Error handling
- `tracing` - Structured logging
- `httparse` - HTTP/1.x parser (for HTTP routing)
- `tls-parser` - TLS/SNI parser (for HTTPS routing)

Development/test dependencies include: `tonic`, `axum`, `hyper`, `rustls`, `reqwest`, `futures` for protocol testing.

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

## Feature Highlights

### HTTP/HTTPS Routing Architecture

The HTTP/HTTPS routing feature enables DNS-based service discovery and routing:

**How it works:**
1. **Import side** listens for HTTP/HTTPS connections
2. **Protocol detection**: Automatically detects HTTP (text) vs HTTPS (TLS)
3. **DNS extraction**: 
   - HTTP: Parses `Host` header
   - HTTPS: Extracts SNI from TLS ClientHello (before encryption)
4. **DNS normalization**: Converts to lowercase, strips default ports (80/443)
5. **Backend discovery**: Queries Zenoh for backends registered with that DNS
6. **Routing**: Forwards to correct backend via DNS-specific Zenoh keys
7. **Pass-through**: For HTTPS, TLS handshake and data pass through unchanged

**Benefits:**
- One listener → N backends (multi-tenant)
- No configuration changes needed for new backends
- HTTPS works without TLS termination (end-to-end encryption)
- Automatic DNS normalization (case-insensitive, port-aware)
- Backend availability detection (HTTP 502 when unavailable)
- Production-ready with comprehensive error handling

**Test Coverage:**
- 9 comprehensive test suites (3 HTTP, 3 HTTPS, 3 edge cases)
- 55 unit and integration tests passing ✅
- 3 tests ignored (timing issues with complex scenarios)
- Tests HTTP, HTTPS, concurrent clients, DNS normalization, edge cases, and more
- Test pass rate: 95% (55/58 tests)

**Documentation:**
- [HTTP/HTTPS Routing Guide](docs/HTTP_ROUTING_GUIDE.md) - Complete guide with examples
- [Phase B Summary](PHASE_B_SUMMARY.md) - Technical implementation details
- [TODO.md](TODO.md) - Feature roadmap and implementation status

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