# Observability

## Health and metrics endpoints

`--metrics-addr <addr>` (disabled by default) serves a small dependency-free
HTTP surface suitable for Kubernetes probes and Prometheus scraping:

```bash
zenoh-bridge-tcp --backend 'api/127.0.0.1:3000' --metrics-addr 0.0.0.0:9100
```

| Endpoint | Purpose | Response |
|---|---|---|
| `GET /healthz` | Liveness | `200 ok` while the process runs |
| `GET /readyz` | Readiness | `200 ready` once all bridges are started, else `503 not ready` |
| `GET /metrics` | Metrics | Prometheus text exposition (v0.0.4) |

```yaml
livenessProbe:  { httpGet: { path: /healthz, port: 9100 } }
readinessProbe: { httpGet: { path: /readyz,  port: 9100 } }
```

## Metrics

Counters carry a `service` label matching the spec's service name. Bytes are
recorded on every data plane, including the `route=request` per-request plane.

| Metric | Type | Meaning |
|---|---|---|
| `zbridge_ready` | gauge | 1 once the bridge is ready |
| `zbridge_active_connections{service}` | gauge | Currently open connections |
| `zbridge_connections_total{service}` | counter | Connections opened |
| `zbridge_bytes_total{service,direction}` | counter | Bytes relayed; `up` = client→backend, `down` = backend→client |
| `zbridge_connections_outcome_total{service,outcome}` | counter | How connections ended: `completed`, `reset`, or `failed` |
| `zbridge_grpc_status_total{service,code}` | counter | gRPC calls by `grpc-status` code, from terminated-h2 and h2c response trailers. A failed gRPC call still carries HTTP 200, so this is the meaningful error signal |

## Logging

```bash
# CLI flags
zenoh-bridge-tcp --log-level debug --log-format json --backend 'svc/127.0.0.1:8003'

# RUST_LOG takes precedence over --log-level, and allows per-module filters
RUST_LOG=zenoh_bridge_tcp=debug,zenoh=warn zenoh-bridge-tcp --backend 'svc/127.0.0.1:8003'
```

Formats: `pretty` (default, human-readable, colored), `compact` (single-line),
`json` (structured, for ELK/Loki-style aggregation).
