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
- **L7 parsing is delegated to the [`flowscope`](https://crates.io/crates/flowscope) crate** (features `http` + `http2` + `tls`, pure-compute, no async/CAP/root). `flowscope::classify::classify_first_bytes` detects the protocol; `flowscope::tls::TlsParser` extracts SNI (reassembling segmented/PQ ClientHellos); `flowscope::http::HttpProxyParser` frames HTTP/1.1 streaming — method-aware bodies, chunked, `1xx` interim, upgrades, and RFC 9112 §6.3 smuggling defense (`SmugglingPolicy::Strict` → typed `HttpPoison` → 400/502). Under `--https-terminate` the negotiated ALPN selects the head reader: `h2` → `flowscope::http2::Http2Parser` peeks the first stream's `:authority` (`read_h2_head`, Phase C #50), else `Host` via `read_http_head`. The bridge routes on the head and relays raw bytes it never retains — for h2 the multiplexed streams flow opaquely to one backend (single-authority proxy, not a per-stream demux). A terminated-h2 connection's response is tapped read-only (`bridge_import_connection`'s `ResponseTap` → `connection::h2_response_tap`) to surface each stream's gRPC status (`grpc-status` trailers, or Trailers-Only) as the `zbridge_grpc_status_total{service,code}` metric + a log line.
- `src/dns.rs` normalizes the DNS routing key (lowercase, strip default 80/443); `src/http_util.rs` holds the 400/502/504 byte templates.
- `src/import/listener.rs` tracks per-connection tasks via `JoinSet` with graceful drain on shutdown
- `src/export/bridge.rs` cancels old connections before spawning replacements, releases mutex before await
- `src/metrics.rs` is the observability surface (G7): a `LazyLock` global registry of per-service atomic `Counters` (active/total/bytes/outcomes) plus a dependency-free HTTP server for `/healthz` (liveness), `/readyz` (readiness flag), and `/metrics` (Prometheus text). Enabled with `--metrics-addr`. Data planes record via `metrics::conn_start(service)` — an RAII `ConnGuard` (active gauge dec on drop) whose `counters()` is cloned into each direction for lock-free per-chunk byte counting.

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

Each data subscriber (`/tx/`, `/rx/`) is drained through `src/backpressure.rs`
(`rx_channel`): a **non-blocking** callback (`try_send`) into a bounded
`tokio::mpsc` instead of Zenoh's default FIFO handler, whose blocking callback
would stall the shared session's reception thread and head-of-line-block every
client (D2). On overflow: `Stream` mode resets that one connection (byte-exact,
never drops), `Telemetry` mode sheds the sample. Depth is `--rx-channel-capacity`.
