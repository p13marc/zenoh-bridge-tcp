# Routing

How a connection entering a `--listen` port finds its `--backend`, and why the
bridge has no route table.

## The model: attachment points, not routes

The bridge's configuration surface is two repeatable flags:

- `--listen '<service>/<addr>[,options]'` — a local port that accepts clients.
- `--backend '<service>[@<host>]/<target>'` — a local service exposed onto the
  Zenoh bus.

That is all a node ever states: *what it listens on* and *what it exposes*.
There is no route table to write, because routing happens on the Zenoh key
space itself:

1. A listener peeks the first bytes of each connection, classifies the
   protocol, and **mints a Zenoh key** from what it finds — the TLS SNI, the
   HTTP/1 `Host`, the HTTP/2 `:authority`, or nothing at all for opaque
   traffic.
2. Backends **self-announce** by declaring a liveliness token
   (`{service}/{host}/available`, or `{service}/available` for a backend with
   no `@host` — the service's default); listeners gate on it. Registration,
   health check, and deregistration are the same mechanism.
3. N listener bridges and M backend bridges joined to one bus form a
   **many-to-many mesh**: one backend can serve five listeners, adding a node
   is joining the bus, and no other node's configuration changes.

Classic proxies (nginx, Envoy, HAProxy) route a connection to a backend they
were *told about* — a static table or one pushed by a control plane (xDS,
Consul). Here the data plane and the control plane are the same substrate:
inspection exists to mint keys, not to select from a list, and the listener
never knows where a backend runs. The closest cousin is Skupper's
connector/listener/routingKey model; the difference is that the bridge also
*sniffs* the hostname from the wire, so one port serves arbitrarily many
`{service}/{host}` routes with zero per-hostname configuration anywhere.

## The listener pipeline

By default (`proto=auto`, no cert) a listener runs this per connection:

```
accept ──► peek first bytes ──► classify (flowscope)
              │
              ├─ TLS ClientHello ──► extract SNI ──► route, relay ENCRYPTED (passthrough)
              ├─ HTTP/1 request ───► read Host ────► route, relay
              │     └─ WebSocket upgrade ──► route by Host, splice the upgrade
              ├─ HTTP/2 preface ───► peek first stream's :authority ──► route, relay (h2c)
              └─ anything else ────► no key ──► opaque L4 tunnel
```

Notes on the branches:

- **TLS passthrough** never decrypts. Routing uses the cleartext SNI from the
  ClientHello (segmented and post-quantum-sized hellos are reassembled). ALPN
  is untouched, so HTTP/2 and gRPC-over-TLS ride through end-to-end encrypted.
- **h2c** (plaintext HTTP/2, including prior-knowledge gRPC): the bridge peeks
  the first request stream's `:authority`, routes, then relays the raw bytes.
  Clients that wait for the server's SETTINGS before sending headers fall back
  to an opaque relay after a timeout (RFC 9113 §3.4).
- **Terminated TLS** (`cert=`/`key=` on the spec): the bridge is the TLS
  endpoint and backends receive plaintext. ALPN negotiates `h2`/`http/1.1`;
  h1 routes by `Host`, h2 by `:authority`. Requires a build with
  `--features tls-termination`.
- **h2 is a single-authority relay**, not a per-stream demux: all multiplexed
  streams of one connection go to the backend chosen from the first stream.
  Routing different streams of one h2 connection to different backends would
  require a full HTTP/2 proxy, which is deliberately out of scope — as is a
  rules language (path matching, header predicates). The hostname is the key;
  the key space is the routing table.

The remaining listener options cover only the decisions the wire cannot
answer:

| Option | Meaning |
|---|---|
| `proto=raw` | Skip the peek entirely: an opaque L4 tunnel. **Required for server-speaks-first protocols** (SMTP, MySQL, …) — auto-detect waits for client bytes that never come before the server's greeting. |
| `cert=PATH,key=PATH` | **Cert implies termination.** There is no `tls=` keyword: presence of key material is the decision. No cert = passthrough, zero key material on the bridge. |
| `route=request` | Re-route every request of a keep-alive HTTP/1.1 connection independently (a full L7 proxy plane: both directions parsed and framed; plaintext h1 only). Default is route-once-per-connection. |

Contradictory combinations are rejected at startup: `proto=raw` cannot
terminate TLS or route per-request (it parses nothing), and `route=request`
cannot terminate (it is the plaintext-h1 plane).

## The Zenoh key space

Opaque connections (no routing key), and hostname-routed connections that
fell back to the default backend:

```
{service}/tx/{client_id}        client → backend bytes
{service}/rx/{client_id}        backend → client bytes
{service}/clients/{client_id}   liveliness token (client presence)
{service}/available             liveliness token (default backend presence)
```

Hostname-routed connections (Host / SNI / `:authority`):

```
{service}/{host}/tx/{client_id}
{service}/{host}/rx/{client_id}
{service}/{host}/clients/{client_id}
{service}/{host}/available      liveliness token (backend presence)
```

**Resolution order** for a hostname-routed connection: a backend that
announced `{service}/{host}/available` for exactly this hostname wins; else,
if a default backend announced `{service}/available`, the connection relays on
the bare service keys to it; else the listener refuses fast (HTTP and
WebSocket answer 502, TLS and h2c close). A plain `--backend '<svc>/<target>'`
is therefore the service's catch-all: it serves opaque traffic *and* every
hostname no `@host` backend claims.

The lifecycle, from both sides:

- **Listener side**: on accept, mint a unique `client_id`, declare the
  `clients/{client_id}` liveliness token, publish client bytes to `tx/`,
  subscribe to `rx/`. For hostname-routed traffic, first resolve the backend
  (host token, else default token — both probed concurrently) and answer
  HTTP 502 if neither is alive.
- **Backend side**: watch for `clients/*` liveliness tokens. When one appears,
  connect to the local target (lazy — no client, no backend connection),
  subscribe to that client's `tx/`, publish backend bytes to `rx/`. Tear down
  when the token disappears.

Hostnames are normalized before becoming key segments: lowercased, default
ports 80/443 stripped, and the character set restricted so no Zenoh key-expr
metacharacter (`*`, `?`, `$`, …) can enter the key space from the wire or the
CLI.

## Reliability on the bus

TCP demands a reliable ordered byte pipe; a pub/sub bus does not provide one
by default. Every data-plane publisher/subscriber therefore uses zenoh-ext's
**AdvancedPublisher/AdvancedSubscriber** — publisher cache, sample-miss
detection, heartbeat, and recovery — instead of raw put/subscribe.

`--reliability` picks the posture when recovery fails anyway:

- `stream` (default): byte-exact or dead — an unrecoverable loss or a full
  reception buffer resets that one connection, never delivers a gap.
- `telemetry`: drops are tolerated and counted (for sensor-style payloads
  where fresh data beats complete data).

Each connection's subscriber is drained through a bounded per-connection
channel with a non-blocking callback, so one slow client saturates its own
buffer (`--rx-channel-capacity`) and is reset or shed — it can never
head-of-line-block other clients sharing the Zenoh session.

## Protocol support matrix

| Protocol | Passthrough listener | Terminated (`cert=`) | Plaintext |
|---|---|---|---|
| HTTP/1.1 | — | ✅ routed by Host | ✅ routed by Host (`route=request` optional) |
| HTTP/2 / gRPC over TLS | ✅ SNI, zero-decrypt | ✅ `:authority`, single-authority relay | — |
| h2c / prior-knowledge gRPC | n/a | n/a | ✅ `:authority` |
| WebSocket (`ws://`) | n/a | — | ✅ upgrade auto-detected |
| WebSocket over TLS (`wss://`) | ✅ SNI (it's TLS on the wire) | ✅ | n/a |
| Raw TCP (rsync, Postgres, SSH, …) | ✅ SNI if TLS, else opaque | n/a | ✅ opaque (`proto=raw` if server speaks first) |

On the backend side the protocol is a property of the target address:
`host:port` is TCP, `ws://…` / `wss://…` is a WebSocket backend. `@host`
composes with either; leaving it off makes the backend the service's default
(catch-all for opaque traffic and every unclaimed hostname).

## Migrating from 0.6.x

0.7.0 removed the nine per-protocol routing flags in favor of the two-flag
surface (clean break, no aliases):

| 0.6.x | 0.7.0 |
|---|---|
| `--import s/a` | `--listen s/a,proto=raw` |
| `--http-import s/a` · `--auto-import s/a` | `--listen s/a` (non-HTTP bytes now relay opaquely instead of 400ing) |
| `--ws-import s/a` | `--listen s/a` (upgrade auto-detected) |
| `--http-multiroute-import s/a` | `--listen s/a,route=request` |
| `--https-terminate s/a --tls-cert C --tls-key K` | `--listen s/a,cert=C,key=K` |
| `--export s/b` | `--backend s/b` |
| `--http-export s/d/b` | `--backend s@d/b` |
| `--ws-export s/ws://u` | `--backend s/ws://u` |
| `-m/--mode` `-e/--connect` `-l/--listen` `-c/--config` | `--zenoh-mode` `--zenoh-connect` `--zenoh-listen` `--zenoh-config` |
