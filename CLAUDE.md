# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

Zenoh TCP Bridge is a bidirectional bridge that connects TCP services to the Zenoh distributed data bus. It allows exposing TCP backends as Zenoh services (export mode) or making Zenoh services accessible via TCP listeners (import mode).

## Build and Test

```bash
cargo build --release --features tls-termination  # tls-termination gates --https-terminate/--tls-cert/--tls-key

# Run all tests (nextest recommended for isolation)
cargo nextest run

# Network topology tests (require nlink-lab on the host, not in containers)
./tests/nlink/run-multi-hop-test.sh
./tests/nlink/run-multi-hop-http-test.sh
./tests/nlink/run-multi-hop-test.sh --wan-delay 100ms --wan-loss 1%
```

## Architecture

### Validation and Safety

- `src/args.rs` validates all CLI arguments early: buffer_size >= 1024, drain_timeout >= 1s, log format/level, and all spec formats
- `src/tls_parser.rs` enforces RFC 6066/1035 SNI hostname rules (253-byte limit, 63-byte labels, ASCII-only, no trailing dots)
- `src/http_response_parser.rs` rejects smuggling attempts (TE+CL, duplicate CL), bounds Content-Length to 1GB
- `src/import/listener.rs` tracks per-connection tasks via `JoinSet` with graceful drain on shutdown
- `src/export/bridge.rs` cancels old connections before spawning replacements, releases mutex before await

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
