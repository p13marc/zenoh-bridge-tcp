# Plan 16: Feature Roadmap

**Priority**: Enhancement (after remaining bugs are fixed)
**Report ref**: Sections 7-8
**Effort**: Varies

This plan captures feature ideas from the analysis report, prioritized by value and feasibility. Each feature should get its own plan file when work begins.

## Tier 1: Quick Wins (Low effort, high value)

### Health check endpoint
- Add `--healthz-port <port>` flag
- Spawn a minimal HTTP server that returns 200 on `/healthz` and `/readyz`
- `/healthz` — process is alive
- `/readyz` — Zenoh session is open and at least one bridge task is running
- Useful for Kubernetes liveness/readiness probes and Docker healthchecks
- **Effort**: Low (< 1 day). Use `hyper` or a bare `TcpListener` with hand-crafted HTTP response.

### Dry-run / validate mode
- Add `--validate` flag that parses all specs, loads Zenoh config, loads TLS certs (if any), then exits 0/1
- Already partially done: `Args::validate()` now parses specs. Remaining: validate Zenoh config and TLS certs.
- **Effort**: Trivial (a few lines in main.rs)

### Config file precedence warning
- Already planned in PLAN-14
- **Effort**: Trivial

### Per-service config overrides
- Allow `--export 'svc/127.0.0.1:8000?buffer_size=32768&drain_timeout=10'`
- Parse query string from spec, override BridgeConfig fields per-service
- **Effort**: Low

## Tier 2: Medium Value (Medium effort)

### Prometheus metrics
- Add optional `--metrics-port <port>` flag
- Export metrics via `/metrics` endpoint in Prometheus text format
- Key metrics: `bridge_connections_active`, `bridge_bytes_tx_total`, `bridge_bytes_rx_total`, `bridge_errors_total`, `bridge_connection_duration_seconds`
- Use `prometheus` or `metrics` crate
- **Effort**: Medium (2-3 days)

### Circuit breaker for backends
- Track consecutive backend connection failures per service
- After N failures (configurable), stop trying for a cooldown period
- Publish "circuit open" status on Zenoh for observability
- Auto-recover when a probe connection succeeds
- **Effort**: Medium

### SIGHUP config reload
- On SIGHUP, re-read config file (if `--config` was used)
- Diff old vs new specs, stop removed services, start new ones
- Existing connections are not disrupted
- **Effort**: Medium-High (state management complexity)

### Streaming responses in multiroute
- Currently multiroute buffers the entire response before checking completion
- Stream chunks directly to the client as they arrive from Zenoh
- Still need to parse headers to detect response end, but don't buffer beyond headers
- Reduces memory usage and latency for large responses
- **Effort**: Medium (refactor multiroute response handling)

### mTLS support for import listeners
- Add `--tls-client-ca <path>` flag for client certificate verification
- Reject connections that don't present a valid client cert
- **Effort**: Medium (TLS configuration, depends on `tls-termination` feature)

## Tier 3: Ambitious (High effort)

### gRPC / HTTP/2 support
- HTTP/2 framing is fundamentally different from HTTP/1.1 (multiplexed streams)
- Would require h2 crate integration and stream-level routing
- **Effort**: High (new protocol, significant architecture change)

### UDP support
- Bridge UDP datagrams over Zenoh
- Stateless: each datagram is independent (no connection tracking)
- Could use Zenoh `put()` directly (no AdvancedPublisher needed)
- **Effort**: Medium-High (new transport, different semantics from TCP)

### Load balancing across multiple backends
- Round-robin or least-connections when multiple exports register for the same service/dns
- Requires tracking backend health and connection counts
- **Effort**: High (state management, health checking, failover logic)

### Graceful restart (zero-downtime)
- Pass listening sockets to a new process via Unix socket / fd passing
- New process takes over, old process drains
- Complex: requires systemd socket activation or custom fd-passing protocol
- **Effort**: Very High

## Recommended Order

1. Dry-run/validate mode (trivial, immediate value)
2. Health check endpoint (low effort, high ops value)
3. Per-service config (low effort, user-requested flexibility)
4. Prometheus metrics (medium effort, essential for production)
5. Circuit breaker (medium effort, reliability improvement)
6. Streaming responses (medium effort, performance improvement)
7. Everything else based on user demand
