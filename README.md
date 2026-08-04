# Zenoh TCP Bridge

A bidirectional bridge that connects TCP services to the Zenoh distributed data bus. Export TCP backends as Zenoh services or import Zenoh services as TCP listeners.

## Overview

This bridge enables:
- **`--backend`**: expose local TCP/WebSocket services onto the Zenoh bus
- **`--listen`**: accept clients on a local port and bridge them to services on the bus
- **Multiple Services**: run many listeners and backends in a single bridge instance
- **Automatic Connection Management**: lazy connections with liveliness detection

## How routing works

A listener needs no protocol configuration. It peeks the first bytes of each
connection, classifies the protocol, and **mints a Zenoh key** from what it
finds: the TLS **SNI** (passthrough — the bridge never decrypts), the HTTP/1
**Host**, the HTTP/2 **`:authority`** (plaintext h2c, or decrypted when the
listener holds a cert), a WebSocket upgrade's Host, or nothing at all for
opaque traffic. Backends **self-announce** on the bus
(`{service}/{host}/available` liveliness tokens), so the route table is never
written anywhere — N listener bridges and M backend bridges form a
many-to-many mesh with zero per-route configuration. HTTP/1.1, HTTP/2, and
gRPC all ride through the same door: the codec is detected, never configured.

The only decisions left to the operator are the ones the wire cannot answer
(see [Listener options](#listener-options)): `proto=raw` for
server-speaks-first protocols, `cert=`/`key=` when the bridge should be the
TLS endpoint, and `route=request` for per-request HTTP/1.1 routing. The full
design rationale lives in
[docs/ROUTING-SIMPLIFICATION.md](docs/ROUTING-SIMPLIFICATION.md).

## Features

### Core Functionality
- **Lazy Connections**: Backend connections are created only when clients connect (export mode)
- **Per-Client Isolation**: Each client gets dedicated Zenoh pub/sub channels and backend connection
- **Concurrent Services**: Handle multiple services in one bridge process
- **Flexible Configuration**: Command-line arguments or Zenoh config files
- **WebSocket Support**: Bridge WebSocket backends alongside TCP services
- **Graceful Shutdown**: Clean shutdown via CancellationToken with per-task tracking and connection draining
- **Backend Reconnection**: Exponential backoff retry when backends become unavailable
- **Input Validation**: Early CLI validation (buffer size, timeouts, spec formats, log options) with clear error messages
- **Strict TLS Parsing**: RFC 6066/1035-compliant SNI validation with size bounds checking

### HTTP/HTTPS Routing
- **DNS-Based Routing**: Route HTTP requests by Host header to different backends
- **SNI-Based HTTPS**: Route HTTPS traffic by SNI without terminating TLS
- **Automatic Protocol Detection**: Detects HTTP vs HTTPS automatically
- **DNS Normalization**: Case-insensitive, port-aware routing
- **Multiple Backends**: One listener can route to N different HTTP/HTTPS servers
- **End-to-End TLS**: HTTPS traffic is never decrypted by the bridge
- **Per-Request Multiroute**: HTTP/1.1 keep-alive connections can route to different backends per request
- **Optional TLS Termination**: decrypt at the bridge with `--listen …,cert=,key=` (requires `tls-termination` feature)

### Protocol Auto-Detection
- **Auto-Detection**: a single listener detects TLS/HTTPS, HTTP, h2c, WebSocket, or raw TCP from the first bytes
- **Zero Configuration**: No need to pre-declare protocol per listener

### Liveliness Detection
- Automatic client presence tracking using Zenoh liveliness tokens
- Clean disconnection handling and resource cleanup
- Backend connection lifecycle tied to client presence

### Multiple Zenoh Modes
- **Peer Mode** (default): Direct peer-to-peer communication
- **Client Mode**: Connect to existing Zenoh routers
- **Router Mode**: Act as a Zenoh router

## Architecture

### Backend side (`--backend`)
```
TCP Backend (e.g., HTTP server on :8003)
    ↕
Zenoh Bridge (--backend)
    ↕
Zenoh Network
    ↕
Zenoh Bridge (--listen)
    ↕
TCP Client connects to :8002
```

The backend bridge:
1. Monitors for client liveliness tokens on `{service}/clients/{client_id}`
2. Creates backend connection when client appears
3. Subscribes to `{service}/tx/{client_id}` (data from client)
4. Publishes to `{service}/rx/{client_id}` (data to client)
5. Cleans up when client disconnects

### Listener side (`--listen`)

The listener bridge:
1. Listens for TCP connections on specified address
2. For each connection, creates unique client ID
3. Declares liveliness token at `{service}/clients/{client_id}`
4. Publishes to `{service}/tx/{client_id}` (data from TCP client)
5. Subscribes to `{service}/rx/{client_id}` (data to TCP client)
6. Undeclares liveliness on disconnection

## Quick Start

### Expose a service onto the bus

```bash
# Terminal 1: Start your HTTP server
python3 -m http.server 8003

# Terminal 2: Expose it as the "webserver" service
zenoh-bridge-tcp --backend 'webserver/127.0.0.1:8003'
```

### Listen for clients

```bash
# Terminal 3: Accept clients for "webserver" on :8002
zenoh-bridge-tcp --listen 'webserver/127.0.0.1:8002'

# Terminal 4: Test with curl
curl http://127.0.0.1:8002
```

### Multiple Services

```bash
zenoh-bridge-tcp \
  --backend 'api/127.0.0.1:3000' \
  --backend 'db/127.0.0.1:5432' \
  --listen 'frontend/0.0.0.0:8080'
```

## Building

### Prerequisites
- Rust 1.97 or later (edition 2024; pinned as `rust-version`)

### Build
```bash
cargo build --release
```

The binary will be at `target/release/zenoh-bridge-tcp`.

### Build with TLS Termination
```bash
cargo build --release --features tls-termination
```

### Run Tests
```bash
# Run all tests (recommended: use nextest for better test isolation)
cargo nextest run

# Run unit tests only
cargo test --lib

# Run a specific integration test suite
cargo nextest run --test http_routing_integration
```

## Usage

### Command Line Options

```
zenoh-bridge-tcp [OPTIONS]

Options:
      --listen <SPEC>          Accept clients on a local port and bridge them over Zenoh
                               Format: '<service>/<addr>[,proto=raw][,cert=PATH,key=PATH][,route=request]'
                               Default: auto-detect — TLS routes by SNI (passthrough),
                               HTTP/1 by Host, WebSocket upgrades transparently,
                               anything else as an opaque tunnel
                               Example: 'web/0.0.0.0:8080'
      --backend <SPEC>         Expose a local service onto the Zenoh bus
                               Format: '<service>[@<host>]/<target>'
                               Target 'host:port' is TCP; a 'ws://'/'wss://' URL is WebSocket
                               Example: 'web@api.example.com/127.0.0.1:8003'
      --zenoh-config <FILE>    Path to Zenoh configuration file (JSON5)
      --zenoh-mode <MODE>      Zenoh mode: peer, client, or router [default: peer]
      --zenoh-connect <EP>     Zenoh connect endpoint (e.g., tcp/localhost:7447)
      --zenoh-listen <EP>      Zenoh listen endpoint (e.g., tcp/0.0.0.0:7447)
      --metrics-addr <ADDR>    Expose /healthz, /readyz, /metrics on this address
                               Example: '0.0.0.0:9100' (disabled when unset)
      --buffer-size <BYTES>    Buffer size for read operations [default: 65536]
      --read-timeout <SECS>    Timeout for reading headers [default: 10]
      --drain-timeout <SECS>   Connection drain timeout [default: 5]
      --log-level <LEVEL>      Log level: trace, debug, info, warn, error, off [default: info]
      --log-format <FORMAT>    Log format: pretty, compact, json [default: pretty]
  -h, --help                   Print help
  -V, --version                Print version
```

### Listener options

| Option | Meaning |
|---|---|
| *(none)* | Auto-detect: TLS→SNI passthrough, HTTP/1→Host, WebSocket upgrade, else opaque tunnel |
| `proto=raw` | No detection, pure L4 tunnel — required for server-speaks-first protocols (SMTP, MySQL, …) |
| `cert=PATH,key=PATH` | **Cert implies termination**: the bridge is the TLS endpoint; backends get plaintext. Needs the `tls-termination` build feature. Without a cert, TLS is never decrypted |
| `route=request` | Per-request Host routing on keep-alive HTTP/1.1 (an L7 proxy plane; plaintext h1 only) |

### Migrating from 0.6.x

| 0.6.x | 0.7.0 |
|---|---|
| `--import s/a` | `--listen s/a,proto=raw` |
| `--http-import s/a` · `--auto-import s/a` | `--listen s/a` (auto-detect: non-HTTP bytes now relay opaquely instead of 400ing) |
| `--ws-import s/a` | `--listen s/a` (upgrade auto-detected) |
| `--http-multiroute-import s/a` | `--listen s/a,route=request` |
| `--https-terminate s/a --tls-cert C --tls-key K` | `--listen s/a,cert=C,key=K` |
| `--export s/b` | `--backend s/b` |
| `--http-export s/d/b` | `--backend s@d/b` |
| `--ws-export s/ws://u` | `--backend s/ws://u` |
| `-m/--mode` `-e/--connect` `-l/--listen` `-c/--config` | `--zenoh-mode` `--zenoh-connect` `--zenoh-listen` `--zenoh-config` |

### Configuration File

Use a Zenoh configuration file for advanced settings:

```bash
zenoh-bridge-tcp \
  --zenoh-config zenoh-config.json5 \
  --backend 'myservice/127.0.0.1:8003' \
  --listen 'myservice/0.0.0.0:8002'
```

## Examples

### Example 1: Plain TCP bridge for HTTP

```bash
# Terminal 1: Start HTTP server
python3 -m http.server 8003

# Terminal 2: Backend side
zenoh-bridge-tcp --backend 'http/127.0.0.1:8003'

# Terminal 3: Listener side (can be on a different machine)
zenoh-bridge-tcp --listen 'http/127.0.0.1:8002' --zenoh-connect tcp/backend-host:7447

# Terminal 4: Test
curl http://127.0.0.1:8002
```

### Example 2: Host routing with multiple backends

```bash
# Terminal 1: Start multiple HTTP servers
python3 -m http.server 8001  # API backend
python3 -m http.server 8002  # Web backend

# Terminal 2: Expose the API backend for api.example.com
zenoh-bridge-tcp --backend 'http-service@api.example.com/127.0.0.1:8001'

# Terminal 3: Expose the Web backend for web.example.com
zenoh-bridge-tcp --backend 'http-service@web.example.com/127.0.0.1:8002'

# Terminal 4: One listener routes to both backends
zenoh-bridge-tcp --listen 'http-service/0.0.0.0:8080'

# Terminal 5: Test routing by Host header
curl -H "Host: api.example.com" http://127.0.0.1:8080/  # -> API backend
curl -H "Host: web.example.com" http://127.0.0.1:8080/  # -> Web backend
```

### Example 3: HTTPS / gRPC routing by SNI (zero-decrypt)

```bash
# Expose HTTPS (or gRPC-over-TLS) backends by their SNI hostname
zenoh-bridge-tcp --backend 'edge@api.example.com/127.0.0.1:8443'
zenoh-bridge-tcp --backend 'edge@web.example.com/127.0.0.1:8444'

# One listener; TLS is detected and routed by SNI, never decrypted —
# HTTP/2 and gRPC ride through unchanged, end-to-end encrypted
zenoh-bridge-tcp --listen 'edge/0.0.0.0:8443'

# Test — the SNI picks the backend
curl https://api.example.com:8443/ --resolve api.example.com:8443:127.0.0.1
curl https://web.example.com:8443/ --resolve web.example.com:8443:127.0.0.1
```

### Example 4: TLS termination (cert implies it)

```bash
# The listener holds the certificate; backends receive plaintext.
# HTTP/1.1 routes by Host, HTTP/2/gRPC by :authority (ALPN-negotiated).
# Needs a build with --features tls-termination.
zenoh-bridge-tcp \
  --listen 'edge/0.0.0.0:8443,cert=/etc/tls/fullchain.pem,key=/etc/tls/privkey.pem'

zenoh-bridge-tcp --backend 'edge@api.example.com/127.0.0.1:8080'
```

### Example 5: WebSocket bridge

```bash
# Terminal 1: Start WebSocket echo server (using websocat or similar)
websocat -s 127.0.0.1:9000

# Terminal 2: Expose the WebSocket backend (URL scheme selects the transport)
zenoh-bridge-tcp --backend 'wsecho/ws://127.0.0.1:9000'

# Terminal 3: Listener — the upgrade is auto-detected
zenoh-bridge-tcp --listen 'wsecho/0.0.0.0:8080'

# Terminal 4: Connect WebSocket client
websocat ws://127.0.0.1:8080
```

### Example 6: Raw tunnel for server-speaks-first protocols

```bash
# Terminal 1: An SMTP-ish server that greets first
nc -l 8003

# proto=raw skips the protocol peek: the bridge would otherwise wait for
# client bytes that never come before the server's greeting
zenoh-bridge-tcp --backend 'echo/127.0.0.1:8003'
zenoh-bridge-tcp --listen 'echo/127.0.0.1:8002,proto=raw'

# Terminal 4: Client
nc 127.0.0.1 8002
```

### Example 7: With a Zenoh router

```bash
# Terminal 1: Start Zenoh router
zenohd

# Terminal 2: Backend side (client mode)
zenoh-bridge-tcp \
  --backend 'service/127.0.0.1:8003' \
  --zenoh-mode client \
  --zenoh-connect tcp/localhost:7447

# Terminal 3: Listener side (client mode)
zenoh-bridge-tcp \
  --listen 'service/0.0.0.0:8002' \
  --zenoh-mode client \
  --zenoh-connect tcp/localhost:7447
```

## Docker Deployment

Build and run with Docker:

```bash
# Build image
docker build -t zenoh-bridge-tcp .

# Run the demo topology (backend bridge + listener bridge + echo backend)
docker-compose up -d

# Add a Zenoh router + client-mode bridge
docker-compose --profile with-router up -d
```

## Testing

The project includes comprehensive integration tests. Use `cargo nextest run` for best results (parallel execution with process isolation).

### Test Suites
- **`tests/export_import_integration.rs`** - Core export/import functionality
- **`tests/tcp_sanity_tests.rs`** - Basic TCP sanity checks
- **`tests/http_integration.rs`** - HTTP/HTTPS service bridging
- **`tests/liveliness_integration.rs`** - Liveliness detection
- **`tests/multi_service_integration.rs`** - Multiple concurrent services
- **`tests/http_routing_integration.rs`** - HTTP routing with multiple backends
- **`tests/https_routing_integration.rs`** - HTTPS routing with SNI
- **`tests/http_edge_cases.rs`** - Edge cases and error handling
- **`tests/http_multiroute_integration.rs`** - Per-request HTTP multiroute
- **`tests/ws_integration.rs`** - WebSocket bridging
- **`tests/drain_integration.rs`** - Connection drain on shutdown
- **`tests/auto_import_integration.rs`** - Protocol auto-detection
- **`tests/https_termination_integration.rs`** - TLS termination
- **`tests/stress_test.rs`** - Load and stress testing
- **`tests/bug_demonstrations.rs`** - Verification tests for 16 audit bug fixes
- **`tests/coverage_integration.rs`** - Large messages, partial transfers, concurrent clients, rapid connect/disconnect

Run tests:

```bash
# All tests (recommended)
cargo nextest run

# Unit tests only
cargo test --lib

# Specific test suite
cargo nextest run --test http_routing_integration
```

### Network Topology Tests (nlink-lab)

End-to-end tests using [nlink-lab](https://github.com/p13marc/nlink-lab) to create isolated network namespaces with realistic WAN conditions. These require Linux with network namespace support and `nlink-lab` installed on the host.

```bash
# Raw TCP multi-hop: client -> import bridge -> zenoh -> export bridge -> backend
./tests/nlink/run-multi-hop-test.sh

# HTTP host-header routing multi-hop: multiple backends, single import listener
./tests/nlink/run-multi-hop-http-test.sh

# With WAN simulation
./tests/nlink/run-multi-hop-test.sh --wan-delay 100ms --wan-loss 1%

# Skip cargo build if already built
./tests/nlink/run-multi-hop-test.sh --skip-build
```

See [tests/nlink/README.md](tests/nlink/README.md) for debugging tips and topology details.

See [tests/README.md](tests/README.md) for detailed testing documentation.

## Zenoh Key Expression Design

The bridge uses a structured key expression pattern:

### Opaque connections
```
{service_name}/tx/{client_id}      # Client -> Backend data
{service_name}/rx/{client_id}      # Backend -> Client data
{service_name}/clients/{client_id} # Liveliness token
```

### Hostname-routed connections (Host / SNI / :authority)
```
{service_name}/{dns}/tx/{client_id}      # Client -> Backend data (for specific DNS)
{service_name}/{dns}/rx/{client_id}      # Backend -> Client data (for specific DNS)
{service_name}/{dns}/clients/{client_id} # Liveliness token (per DNS)
{service_name}/{dns}/available           # Backend availability signal
```

Each connection gets a unique `client_id`, ensuring isolation between clients. The `{dns}` component — minted from the Host header, TLS SNI, or h2 `:authority` — routes one listener to many backends; backends announce themselves at `{service}/{dns}/available`, so the bus itself is the route table.

## Logging

Control log verbosity with CLI flags or `RUST_LOG` environment variable:

```bash
# Using CLI flags (recommended)
zenoh-bridge-tcp --log-level debug --backend 'service/127.0.0.1:8003'

# JSON format for production/log aggregation
zenoh-bridge-tcp --log-level info --log-format json --backend 'service/127.0.0.1:8003'

# Compact format (less verbose)
zenoh-bridge-tcp --log-format compact --backend 'service/127.0.0.1:8003'

# Using RUST_LOG (takes precedence over --log-level)
RUST_LOG=debug zenoh-bridge-tcp --backend 'service/127.0.0.1:8003'

# Module-specific with RUST_LOG
RUST_LOG=zenoh_bridge_tcp=debug,zenoh=warn zenoh-bridge-tcp --backend 'service/127.0.0.1:8003'
```

### Log Formats

- **pretty** (default): Human-readable with colors
- **compact**: Single-line format, less verbose
- **json**: Structured JSON, ideal for log aggregation (ELK, Loki, etc.)

## Observability

Pass `--metrics-addr <addr>` to expose a small HTTP surface (disabled by default),
suitable for Kubernetes probes and Prometheus scraping:

```bash
zenoh-bridge-tcp --backend 'api/127.0.0.1:3000' --metrics-addr 0.0.0.0:9100
```

| Endpoint | Purpose | Response |
|---|---|---|
| `GET /healthz` | Liveness | `200 ok` while the process runs |
| `GET /readyz`  | Readiness | `200 ready` once bridges are started, else `503 not ready` |
| `GET /metrics` | Metrics | Prometheus text exposition (v0.0.4) |

Counters are labelled per **service** (`service="…"`):

- `zbridge_ready` — gauge, 1 when ready
- `zbridge_active_connections{service}` — gauge of open connections
- `zbridge_connections_total{service}` — counter of connections opened
- `zbridge_bytes_total{service,direction="up|down"}` — bytes relayed (`up` =
  client→backend, `down` = backend→client)
- `zbridge_connections_outcome_total{service,outcome="completed|reset|failed"}`
  — how connections ended
- `zbridge_grpc_status_total{service,code}` — gRPC calls (terminated h2 or
  plaintext h2c) by `grpc-status` code (a failed gRPC call still carries HTTP
  200, so this is the meaningful signal)

Wire up a Kubernetes probe:

```yaml
livenessProbe:  { httpGet: { path: /healthz, port: 9100 } }
readinessProbe: { httpGet: { path: /readyz,  port: 9100 } }
```

> Byte counters are recorded on every data plane, including the
> `route=request` (per-request HTTP) plane.

## Performance Considerations

- **Buffer Size**: 64KB (65,536 bytes) default per connection
- **Concurrent Connections**: Limited by system resources (file descriptors, memory)
- **Latency**: Adds ~1-2ms overhead vs direct TCP (depends on Zenoh setup)
- **Throughput**: Tested with HTTP, HTTPS, and raw TCP; handles typical workloads well

## Use Cases

### Traditional TCP Bridging
- **Service Discovery**: Expose services without static IP addresses
- **Network Abstraction**: Abstract away network topology
- **Cloud-Edge Bridging**: Connect edge devices to cloud services via Zenoh
- **Legacy Integration**: Make TCP services Zenoh-native
- **Multi-Region Deployment**: Leverage Zenoh's peer-to-peer or routed mesh
- **Protocol Bridging**: Connect TCP clients to Zenoh-based backends

### HTTP/HTTPS Routing
- **Virtual Host Routing**: Route HTTP traffic by hostname to different backends
- **Multi-Tenant SaaS**: Single listener routes customers to their dedicated backends
- **API Gateway**: Route API requests by domain to microservices
- **SNI-Based Load Distribution**: Distribute HTTPS traffic without TLS termination
- **Development/Staging Environments**: Route by hostname to different environments
- **Hybrid Cloud**: Route traffic to backends across different networks via Zenoh

## Dependencies

Core dependencies:
- `zenoh` 1.8.0 - Zenoh distributed data bus
- `zenoh-ext` - Extended pub/sub with reliability features
- `tokio` - Async runtime
- `tokio-util` - CancellationToken for graceful shutdown
- `clap` - Command-line parsing
- `anyhow` / `thiserror` - Error handling
- `tracing` / `tracing-subscriber` - Structured logging (pretty, compact, json)
- `httparse` - HTTP/1.x parser (for HTTP routing)
- `tls-parser` - TLS/SNI parser (for HTTPS routing)
- `tokio-tungstenite` - WebSocket support
- `futures-util` - Async stream utilities
- `backon` - Retry with exponential backoff
- `uuid` - Unique client ID generation

Development/test dependencies include: `axum`, `hyper`, `rustls`, `reqwest`, `futures` for protocol testing.

## Version Information

- **Current Version**: 0.5.0
- **Zenoh Version**: 1.8.0
- **Rust Edition**: 2024
- **MSRV**: 1.97

## Quality Tools

The project uses standard Rust quality tools:

```bash
# Format code
cargo fmt

# Lint
cargo clippy

# Check dependencies
cargo deny check
```

Configuration files:
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
- One listener -> N backends (multi-tenant)
- No configuration changes needed for new backends
- HTTPS works without TLS termination (end-to-end encryption)
- Automatic DNS normalization (case-insensitive, port-aware)
- Backend availability detection (HTTP 502 when unavailable)

**Documentation:**
- [HTTP/HTTPS Routing Guide](docs/HTTP_ROUTING_GUIDE.md) - Complete guide with examples

## Contributing

Contributions welcome! Please:

1. Run `cargo fmt` before committing
2. Ensure `cargo clippy` passes
3. Add tests for new features
4. Update documentation as needed

## License

Licensed under the MIT license ([LICENSE-MIT](LICENSE-MIT) or http://opensource.org/licenses/MIT).

## Additional Resources

- [Zenoh Documentation](https://zenoh.io/docs/)
- [Zenoh GitHub](https://github.com/eclipse-zenoh/zenoh)
- [Test Documentation](tests/README.md)
