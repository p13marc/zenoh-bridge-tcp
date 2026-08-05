# zenoh-bridge-tcp

A bidirectional bridge between TCP and the [Zenoh](https://zenoh.io) distributed
data bus. Expose local TCP or WebSocket services onto the bus (`--backend`), and
open local ports that reach them from anywhere on the bus (`--listen`) — with
protocol detection, hostname routing, and zero route configuration.

```
TCP client ──► bridge --listen ──► Zenoh bus ──► bridge --backend ──► TCP service
```

## How it works

A listener needs no protocol configuration. It peeks the first bytes of each
connection, classifies the protocol, and **mints a Zenoh key** from what it
finds: the TLS **SNI** (passthrough — never decrypted), the HTTP/1 **Host**,
the HTTP/2 **`:authority`** (plaintext h2c, or decrypted when the listener
holds a cert), a WebSocket upgrade's Host, or nothing at all for opaque
traffic. Backends **self-announce** on the bus with liveliness tokens, so the
route table is never written anywhere: N listener bridges and M backend
bridges form a many-to-many mesh with zero per-route configuration. HTTP/1.1,
HTTP/2, and gRPC all ride through the same door — the codec is detected,
never configured.

The only decisions left to the operator are the ones the wire cannot answer:
`proto=raw` for server-speaks-first protocols, `cert=`/`key=` when the bridge
should be the TLS endpoint, and `route=request` for per-request HTTP/1.1
routing. The full design is in [docs/routing.md](docs/routing.md).

Reliability over the bus is not best-effort: publisher caches, sample-miss
detection, and recovery (zenoh-ext) make streams byte-exact, and an
unrecoverable loss resets that one connection instead of delivering a gap.

## Quick start

```bash
# Machine A: expose a local HTTP server as service "web"
python3 -m http.server 8003
zenoh-bridge-tcp --backend 'web/127.0.0.1:8003'

# Machine B: accept clients for "web" on :8002
zenoh-bridge-tcp --listen 'web/0.0.0.0:8002' --zenoh-connect tcp/machine-a:7447

curl http://127.0.0.1:8002
```

Both flags **repeat** — one process can carry any number of listeners and
backends, each running independently:

```bash
zenoh-bridge-tcp \
  --listen 'web/0.0.0.0:8080' \
  --listen 'smtp/0.0.0.0:2525,proto=raw' \
  --backend 'api/127.0.0.1:3000' \
  --backend 'db/127.0.0.1:5432'
```

## The two flags

```
--listen  '<service>/<addr>[,proto=raw][,cert=PATH,key=PATH][,route=request]'
--backend '<service>[@<host>]/<target>'
```

| Listener option | Meaning |
|---|---|
| *(none)* | Auto-detect: TLS→SNI passthrough, HTTP/1→Host, h2c→`:authority`, WebSocket upgrade, else opaque tunnel |
| `proto=raw` | No detection, pure L4 tunnel — required for server-speaks-first protocols (SMTP, MySQL, …) |
| `cert=PATH,key=PATH` | **Cert implies termination**: the bridge is the TLS endpoint, backends get plaintext (h1 by Host, h2/gRPC by `:authority`). Needs the `tls-termination` build feature. Without a cert, TLS is never decrypted |
| `route=request` | Per-request Host routing on keep-alive HTTP/1.1 (plaintext h1 only) |

A backend's protocol is a property of its target: `host:port` is TCP,
`ws://…`/`wss://…` is WebSocket. The optional `@host` registers the backend
for hostname routing. Full reference: [docs/cli.md](docs/cli.md).

## Examples

**Hostname routing — one listener, many backends.** Backends announce which
hostname they serve; the listener routes each connection by Host/SNI:

```bash
zenoh-bridge-tcp --backend 'edge@api.example.com/127.0.0.1:8001'   # anywhere on the bus
zenoh-bridge-tcp --backend 'edge@web.example.com/127.0.0.1:8002'   # anywhere else
zenoh-bridge-tcp --listen 'edge/0.0.0.0:8080'

curl -H "Host: api.example.com" http://127.0.0.1:8080/   # → :8001
curl -H "Host: web.example.com" http://127.0.0.1:8080/   # → :8002
```

**HTTPS / gRPC by SNI, zero-decrypt.** The same listener routes TLS by SNI
without terminating — HTTP/2 and gRPC ride through end-to-end encrypted:

```bash
zenoh-bridge-tcp --backend 'edge@api.example.com/127.0.0.1:8443'
zenoh-bridge-tcp --listen 'edge/0.0.0.0:8443'
curl https://api.example.com:8443/ --resolve api.example.com:8443:127.0.0.1
```

**TLS termination at the bridge** (build with `--features tls-termination`):

```bash
zenoh-bridge-tcp --listen 'edge/0.0.0.0:8443,cert=/etc/tls/fullchain.pem,key=/etc/tls/privkey.pem'
zenoh-bridge-tcp --backend 'edge@api.example.com/127.0.0.1:8080'   # receives plaintext
```

**WebSocket backend** — the URL scheme selects the transport, the upgrade is
auto-detected on the listener:

```bash
zenoh-bridge-tcp --backend 'chat/ws://127.0.0.1:9000'
zenoh-bridge-tcp --listen 'chat/0.0.0.0:8080'
websocat ws://127.0.0.1:8080
```

**Raw tunnel** for protocols where the server speaks first:

```bash
zenoh-bridge-tcp --backend 'mail/127.0.0.1:25'
zenoh-bridge-tcp --listen 'mail/0.0.0.0:2525,proto=raw'
```

## Building

Rust 1.97+ (edition 2024).

```bash
cargo build --release                             # → target/release/zenoh-bridge-tcp
cargo build --release --features tls-termination  # adds TLS-terminating listeners
cargo nextest run                                 # tests (nextest recommended)
```

Docker:

```bash
docker build -t zenoh-bridge-tcp .
docker-compose up -d                     # demo topology: backend bridge + listener bridge + echo
docker-compose --profile with-router up -d
```

## Observability

`--metrics-addr 0.0.0.0:9100` serves `/healthz`, `/readyz`, and Prometheus
`/metrics` (per-service connection, byte, outcome, and gRPC-status counters).

Logs are structured, with values in fields rather than interpolated into
messages. `--log-target` is repeatable and accepts `stdout`, `stderr`,
`file=PATH` (with optional rotation), `journald` (native fields, so
`journalctl CLIENT_ID=…` works), and `syslog`. Every connection emits one
access-log record on close with its outcome, byte counts and duration.

Details: [docs/observability.md](docs/observability.md).

## Documentation

| Doc | Contents |
|---|---|
| [docs/routing.md](docs/routing.md) | How routing works: auto-detection, the Zenoh key space, backend discovery, protocol support matrix, 0.6.x migration |
| [docs/cli.md](docs/cli.md) | Complete flag and spec reference |
| [docs/observability.md](docs/observability.md) | Metrics, health endpoints, logging |
| [docs/development.md](docs/development.md) | Building, feature gates, test suites, CI |
| [tests/README.md](tests/README.md) | Integration-test details |
| [CHANGELOG.md](CHANGELOG.md) | Release history |

## License

MIT ([LICENSE-MIT](LICENSE-MIT) or http://opensource.org/licenses/MIT).
