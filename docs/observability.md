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

### Sinks

`--log-target` names where logs go. It is repeatable, so the same event stream
can reach several places at once, and defaults to `stdout` when unset.

| Target | Meaning |
|---|---|
| `stdout` | Default. |
| `stderr` | For setups that reserve stdout for data. |
| `file=PATH[,rotation=daily\|hourly\|minutely\|never]` | Rotating file via a non-blocking background writer. `rotation` defaults to `never` (leave it to logrotate). The directory is created at startup. |
| `journald` | Native systemd journal fields. Linux only. |
| `syslog[,ident=NAME][,facility=daemon\|user\|local0..local7]` | Local `syslog(3)`. Unix only. `ident` defaults to `zenoh-bridge-tcp`, `facility` to `daemon`. |

```bash
# Console plus a rotating file
zenoh-bridge-tcp --backend 'svc/127.0.0.1:8003' \
  --log-target stdout --log-target file=/var/log/zenoh-bridge/bridge.log,rotation=daily

# Under systemd
zenoh-bridge-tcp --backend 'svc/127.0.0.1:8003' --log-target journald

# Central rsyslog collector
zenoh-bridge-tcp --backend 'svc/127.0.0.1:8003' --log-target syslog,facility=local0
```

`--log-format` shapes only the `stdout`/`stderr`/`file` sinks: `pretty` (default,
single-line with the span scope inline; `full` is an alias), `verbose`
(multi-line, one field per line), `compact`, `json`. journald and syslog carry
fields natively and ignore it.

`--log-color` is `auto` (default), `always`, or `never`. `auto` emits ANSI only
when the sink is an interactive terminal, so redirected output and `file=` sinks
stay escape-free.

### Why native journald

Running under systemd already captures stdout into the journal, but that
collapses every structured field into one opaque `MESSAGE=` string.
`--log-target journald` sends the fields natively, so they are queryable:

```bash
journalctl -u zenoh-bridge-tcp SERVICE=api           # one service
journalctl -u zenoh-bridge-tcp CLIENT_ID=client_1a2b # one connection, end to end
journalctl -u zenoh-bridge-tcp DIRECTION=up          # one relay direction (needs --log-level debug)
```

Field names are uppercased with no prefix, so they match the vocabulary below
one-for-one, and the whole span scope is flattened onto each event — a relay
event carries both its own `DIRECTION` and the connection's `CLIENT_ID`.

Because both bridges log the same `CLIENT_ID`, a `CLIENT_ID=` query returns the
import and export sides of one connection interleaved, across processes.

### Levels and dependency noise

```bash
zenoh-bridge-tcp --log-level debug --backend 'svc/127.0.0.1:8003'

# RUST_LOG overrides --log-level entirely and allows per-module filters
RUST_LOG=zenoh_bridge_tcp=debug,zenoh=trace zenoh-bridge-tcp --backend 'svc/127.0.0.1:8003'
```

Levels: `trace`, `debug`, `info` (default), `warn`, `error`, `off`.

Zenoh, rustls and tungstenite all log through the same subscriber, and zenoh is
extremely chatty. When `RUST_LOG` is unset, the default filter floors those
dependencies at `min(--log-level, warn)` so `--log-level debug` shows the
bridge's own debug output rather than zenoh's routing internals. Setting
`RUST_LOG` disables the damping and hands over full control.

> Note: `EnvFilter` matches log targets by plain string prefix, not by module
> boundary, so a `RUST_LOG=zenoh=warn` directive also matches
> `zenoh_bridge_tcp::…` and silences the bridge itself. Write
> `RUST_LOG=zenoh=warn,zenoh_bridge_tcp=info` when you want that combination.

Containers do **not** pin `RUST_LOG`, so `--log-level` works in them.

### Field vocabulary

Values live in fields, not in the message text — messages are static strings, so
they are stable aggregation keys. Most fields come from the connection span and
are inherited by every event inside it rather than repeated per line.

| Field | Meaning |
|---|---|
| `service` | Service name from the `--listen`/`--backend` spec. Matches the `service` metric label. |
| `client_id` | Per-connection id, identical on the import and export sides — the join key between the two processes. |
| `remote_addr` | Client's address (import side). |
| `mode` | Which plane handled it: `import`, `export`, … |
| `dns` | Routed hostname, recorded on the span once routing resolves. |
| `routed_by` | What the route was taken from: `sni`, `host`, or `authority`. |
| `direction` | `up` (client→backend) or `down` (backend→client). Same convention as `zbridge_bytes_total`. |
| `error` | Error text, always a field and never interpolated into the message. |
| `key` | Zenoh key expression. |
| `backend`, `ws_url` | Backend target being dialled. |
| `alpn`, `proto_guess` | Negotiated/observed protocol on TLS listeners. |
| `request_id`, `request_num` | Per-request correlation on a `route=request` listener. |
| `outcome`, `bytes_up`, `bytes_down`, `duration_ms` | Access log, below. |

### Access log

Every connection that reaches a data plane emits exactly one record when it
closes, on the `zenoh_bridge_tcp::access` target so it can be selected on its
own (`RUST_LOG=zenoh_bridge_tcp::access=info`):

```json
{
  "timestamp": "2026-08-05T05:52:47.769713Z",
  "level": "INFO",
  "target": "zenoh_bridge_tcp::access",
  "fields": {
    "message": "connection closed",
    "outcome": "completed",
    "bytes_up": 21,
    "bytes_down": 21,
    "duration_ms": 2008
  },
  "span": {
    "name": "connection",
    "client_id": "client_894e7b6f0c69475b90dd0e65db97ab07",
    "service": "echo",
    "mode": "import",
    "remote_addr": "127.0.0.1:49188"
  }
}
```

`outcome` uses the same `completed`/`reset`/`failed` labels as
`zbridge_connections_outcome_total`, and the byte counts are the per-connection
share of `zbridge_bytes_total` — so the metric tells you the rate and the access
log tells you which connection. The import and export sides log the same
`client_id`, which is what makes an end-to-end trace possible in an aggregator.

Note that a bare TCP probe (a health checker opening and closing the port) is a
real connection to the bridge and gets its own zero-byte record.

### gRPC status

`zbridge_grpc_status_total{service,code}` has a companion log line: as each
terminated-h2 or h2c stream completes, its `grpc-status` is logged with
`stream_id` and `code` (plus the status `name`), inheriting `service` and
`client_id` from the connection span. A non-OK code logs at `warn`, an OK one at
`debug`. A failed gRPC call still carries HTTP 200, so this is the meaningful
error signal on both surfaces.
