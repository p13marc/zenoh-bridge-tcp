# CLI reference

```
zenoh-bridge-tcp [OPTIONS]
```

At least one `--listen` or `--backend` is required. **Both flags repeat**: each
occurrence is an independent attachment point running as its own task in the
one bridge process, and any mix is valid — a single process can hold several
listeners and several backends at once.

```bash
zenoh-bridge-tcp \
  --listen 'web/0.0.0.0:8080' \
  --listen 'smtp/0.0.0.0:2525,proto=raw' \
  --backend 'api@api.example.com/127.0.0.1:3000' \
  --backend 'api@web.example.com/127.0.0.1:3001' \
  --backend 'chat/ws://127.0.0.1:9000'
```

All specs are validated before anything starts; one bad spec aborts startup
with an error naming it.

## `--listen <SPEC>`

```
<service>/<addr>[,proto=raw][,cert=PATH,key=PATH][,route=request]
```

| Part | Meaning |
|---|---|
| `service` | Zenoh service name. May not contain `/`, `@`, `,`, or key-expr metacharacters. |
| `addr` | Socket address to bind: `0.0.0.0:8080`, `127.0.0.1:8000`, `[::1]:8080` (IPv6 fine — no comma ambiguity). |
| `proto=raw` | Opaque L4 tunnel, no protocol peek. Required for server-speaks-first protocols (SMTP, MySQL, …). Default `proto=auto` detects TLS/HTTP/1/h2c/WebSocket and routes by SNI / Host / `:authority`. |
| `cert=PATH,key=PATH` | Terminate TLS at the bridge with this PEM cert/key (**cert implies termination** — without it TLS is passed through encrypted). Each terminating listener carries its own cert. Requires the `tls-termination` build feature. |
| `route=request` | Per-request Host routing on keep-alive HTTP/1.1 (plaintext h1 only). Default routes once per connection. |

Options are `key=value`, comma-separated, order-free; duplicates and unknown
keys are rejected. Invalid combinations (`proto=raw` with `cert=` or
`route=request`; `route=request` with `cert=`) are rejected at startup. See
[routing.md](routing.md) for what each mode does on the wire.

## `--backend <SPEC>`

```
<service>[@<host>]/<target>
```

| Part | Meaning |
|---|---|
| `service` | Zenoh service name (same rules as above). |
| `@host` | Register this backend for hostname routing: it announces `{service}/{host}/available` and receives the traffic whose Host / SNI / `:authority` matched. Normalized (lowercase, default ports stripped). Omit it for a service's only backend. |
| `target` | `host:port` connects over TCP; a `ws://…` or `wss://…` URL connects as a WebSocket client. The scheme *is* the protocol choice. |

## Zenoh session

| Flag | Default | Meaning |
|---|---|---|
| `--zenoh-mode <MODE>` | `peer` | `peer`, `client`, or `router`. |
| `--zenoh-connect <EP>` | — | Endpoint to connect to, e.g. `tcp/host:7447`. |
| `--zenoh-listen <EP>` | — | Endpoint to listen on, e.g. `tcp/0.0.0.0:7447`. |
| `--zenoh-config <FILE>` | — | JSON5 Zenoh config file. When set, the other `zenoh-*` flags are ignored. |

## Data plane tuning

| Flag | Default | Meaning |
|---|---|---|
| `--reliability <MODE>` | `stream` | `stream`: byte-exact, resets a connection on unrecoverable loss or backpressure overflow. `telemetry`: sheds and counts drops instead. |
| `--buffer-size <BYTES>` | `65536` | TCP read/write buffer per direction (min 1024). |
| `--rx-channel-capacity <N>` | `256` | Per-connection Zenoh reception buffer, in samples. A slow client is isolated at this bound (reset in `stream`, shed in `telemetry`) so it cannot head-of-line-block others. |
| `--cache-size <N>` | `256` | AdvancedPublisher cache depth (late-joiner recovery window). |
| `--heartbeat-interval-ms <MS>` | `500` | Publisher heartbeat for sample-miss detection/recovery. |
| `--max-connections <N>` | `1024` | Concurrent-connection cap per listener (semaphore backpressure on accept). |
| `--read-timeout <SECS>` | `10` | Deadline for reading the routing head (HTTP request head / TLS ClientHello). |
| `--drain-timeout <SECS>` | `5` | Grace period for draining buffered data when a connection or the process shuts down (min 1). |
| `--availability-timeout-ms <MS>` | `1000` | How long a listener waits for a `{service}/{host}/available` token before answering 502. |
| `--max-header-size <BYTES>` | `16384` | Cap on HTTP/TLS routing heads. |
| `--max-response-size <BYTES>` | `10485760` | `route=request` only: response cap, exceeding it answers 502. |

## Observability and logging

| Flag | Default | Meaning |
|---|---|---|
| `--metrics-addr <ADDR>` | disabled | Serve `/healthz`, `/readyz`, `/metrics` on this address (e.g. `0.0.0.0:9100`). See [observability.md](observability.md). |
| `--log-level <LEVEL>` | `info` | `trace`, `debug`, `info`, `warn`, `error`, `off`. `RUST_LOG` overrides it. Noisy dependencies (zenoh, rustls) are damped by default. |
| `--log-format <FORMAT>` | `pretty` | `pretty` (= `full`), `verbose`, `compact`, or `json`. Applies to the stdout/stderr/file sinks only. |
| `--log-target <SPEC>` | `stdout` | Repeatable. `stdout`, `stderr`, `file=PATH[,rotation=daily\|hourly\|minutely\|never]`, `journald`, or `syslog[,ident=NAME][,facility=daemon\|user\|local0..local7]`. |
| `--log-color <WHEN>` | `auto` | `auto` (colour only on a terminal), `always`, or `never`. |

Log sinks, the structured field vocabulary, and the per-connection access log
are documented in [observability.md](observability.md#logging).
