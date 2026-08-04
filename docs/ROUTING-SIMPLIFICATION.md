# Routing simplification report

*Date: 2026-08-04. Question: why does the bridge expose so many TCP-routing options, can the
surface be simplified, and what can we borrow from nginx / Envoy / Traefik? Constraints: must
keep HTTP/1.1 and gRPC first-class, ideally HTTP/2 too, plus raw TCP and ws/wss.*

---

## 1. Executive summary

The bridge grew **9 routing flags** (6 import modes, 3 export modes) organically, one flag per
feature increment. The mainstream proxies solved the same problem with a different decomposition:
**one listener concept + automatic protocol inspection + declarative match rules**, and they get
HTTP/1, HTTP/2, and gRPC through a *single* user-facing mode because gRPC *is* HTTP/2 and the
codec is negotiated (ALPN / preface sniff), not configured.

But the classic proxies are only half the comparison. The bridge's inspection exists to **mint
a Zenoh key**, not to select from a configured route table — backends self-announce via
liveliness tokens, and N importer bridges × M exporter bridges form a many-to-many mesh over
one bus (one bridge can serve five others with zero extra config). That puts its real peer
group among the *discovery-driven* systems — Envoy-with-xDS, Traefik providers, and above all
**Skupper** (whose connector/listener/routingKey model maps one-to-one onto our
export/import/service-key) — where the consensus is: a node configures only its *local
attachment points*; the route table is discovered, never written (§3.4). The bridge already
works this way; the CLI just doesn't say so.

The good news: the code is already 80% of the way there internally. `--auto-import` already does
Envoy-style protocol inspection (via `flowscope::classify`) and dispatches to the same handlers
that back `--import`, `--http-import`, and `--ws-import`. Those three flags are, functionally,
"auto-import with the detection pinned". The flag surface is bigger than the behavior surface.

**Recommendation (detail in §5):** collapse the import side to one `--listen` flag whose default
behavior is today's auto-detect, with two orthogonal options (`tls=terminate|passthrough`,
`proto=auto|raw`) instead of per-protocol flags; collapse the export side to one `--backend`
flag (WebSocket distinguished by its `ws://` URL, host-routing by an optional `@host` part).
Close the one real protocol gap — plaintext HTTP/2 (h2c / prior-knowledge gRPC) is currently
rejected by auto-detect — and the "HTTP/1 + gRPC + HTTP/2" requirement holds in every
deployment shape without the user ever choosing a protocol mode.

---

## 2. Current surface: what we have and why

### 2.1 The nine flags

| Flag | Routes by | TLS | Granularity | Handler it lands in |
|---|---|---|---|---|
| `--import` | nothing (opaque) | passthrough | connection | `connection::handle_import_connection(http_mode=false)` |
| `--http-import` | Host header **or** SNI | passthrough | connection | `handle_import_connection(http_mode=true)` |
| `--http-multiroute-import` | Host header, **per request** | none (plaintext h1 only) | request | `multiroute::run_exchange` |
| `--auto-import` | detect: TLS→SNI, HTTP→Host, WS, else opaque | passthrough | connection | dispatches to the two above + WS |
| `--ws-import` | nothing | n/a | connection | `ws::run_ws_import_mode` |
| `--https-terminate` | h1→Host, h2→`:authority` (ALPN) | **terminated** | connection | `tls::run_https_terminate_import_mode` |
| `--export` | — | — | — | `export::tcp` |
| `--http-export` | registers `{service}/{dns}/available` | — | — | `export::tcp` + availability token |
| `--ws-export` | — | — | — | `export::ws` |

### 2.2 Why it accreted this way

Each flag encodes a *bundle* of decisions that are actually independent axes:

1. **Protocol identification** — opaque vs HTTP vs TLS vs WebSocket (auto-detectable; flowscope
   already does it reliably).
2. **Routing key extraction** — none vs Host vs SNI vs `:authority` (fully determined by axis 1;
   never an independent user choice).
3. **TLS posture** — passthrough vs terminate (a genuine user decision: who holds the cert).
4. **Routing granularity** — per-connection vs per-request (a genuine decision with real
   trade-offs; only meaningful for plaintext HTTP/1.1).

Only axes 3 and 4 are real user decisions. Axes 1 and 2 are auto-detectable and *already
auto-detected* by `--auto-import`. Nine flags for two real decisions is the smell.

### 2.3 Redundancy in the code, not just the CLI

`src/import/auto.rs` classifies first bytes and then calls exactly the same functions the
dedicated flags call: TLS → `handle_import_connection(http_mode=true)` (SNI path), HTTP/1 →
same function (Host path), WebSocket upgrade → `bridge_import_connection` with WS transport,
everything else → `handle_import_connection(http_mode=false)`. Meanwhile five separate accept
loops exist (`listener.rs`, `auto.rs`, `ws.rs`, `tls.rs`, `multiroute.rs`), each re-implementing
the same semaphore + JoinSet + drain pattern. Unifying the CLI would also let the accept loops
collapse into one generic listener parameterized by a connection-handler — Envoy's
listener/filter-chain split, in miniature.

### 2.4 What makes this bridge different from a proxy — the bus *is* the control plane

Before copying proxy designs wholesale, name the thing they *don't* have. Envoy, nginx, and
HAProxy route a connection to a backend **they were told about** — a static route table, or one
pushed by an external control plane. This bridge routes a connection to a **Zenoh key
expression**, and the distributed bus does the rest:

- **Inspection exists to *mint keys*, not to select from a list.** The sniffed SNI / Host /
  `:authority` becomes part of the key (`{service}/{dns}/tx/{client_id}`). The importer never
  knows, and never needs to know, where the backend is.
- **Backends self-announce.** An exporter declares `{service}/{dns}/available` as a liveliness
  token; importers gate on it. Registration, health, and deregistration are the same mechanism.
- **The topology is a mesh, not a chain.** N importer bridges and M exporter bridges share one
  bus: one exporter can serve five importers, an importer can reach any exporter, and adding a
  node is joining the bus — zero config changes anywhere else. A proxy needs an external
  control plane (Envoy's xDS, Consul, Istio) to approximate this; here the data plane and the
  control plane are the same substrate, with AdvancedPub/Sub reliability and liveliness
  built in.

The consequence for simplification: what the user must configure at each node is only the
**local attachment points** — which ports to listen on, which local backends to expose. The
route table itself is emergent. That is *less* than any proxy config file expresses, which is
exactly why two flags can be enough where Envoy needs a YAML tree.

### 2.5 Protocol coverage today (vs the stated requirement)

Required set: **HTTP/1.1, gRPC, HTTP/2 (ideally), raw TCP, ws and wss.**

| Protocol | Passthrough (SNI) | Terminated | Plaintext |
|---|---|---|---|
| HTTP/1.1 | ✅ | ✅ (Host) | ✅ (Host; multiroute optional) |
| HTTP/2 / gRPC over TLS | ✅ (SNI, zero-decrypt) | ✅ (`:authority`, single-authority relay) | — |
| **h2c / prior-knowledge gRPC (plaintext)** | n/a | n/a | ❌ **rejected** (`auto.rs` errors on `Http2Preface`) |
| WebSocket (`ws://`) | n/a | — | ✅ (auto-detected upgrade; multiroute splices upgrades too) |
| WebSocket over TLS (`wss://`) | ✅ (it's TLS on the wire → SNI-routed opaquely) | ⚠️ should work (route on Host, opaque relay carries the upgrade) but **untested** | n/a |
| Raw TCP (rsync, Postgres, …) | ✅ | n/a | ✅ |

Export side: `--ws-export` already accepts both `ws://` and `wss://` backend URLs
(`parse_ws_export_spec`).

The only hole in "HTTP/1 + gRPC + HTTP/2" is plaintext h2. The building block already exists:
`read_h2_head` (built for terminated h2 in PR #61) peeks `:authority` from an h2 preface — it
is just not wired into the auto-detect path (and is currently gated behind `tls-termination`).
Raw TCP and ws are fully covered; terminated wss needs an e2e test to move from ⚠️ to ✅.

---

## 3. How the industry models this

### 3.1 Envoy — listener → inspect → match → action

Envoy's decomposition is the reference model: one **listener** (bind address) → **listener
filters** that *inspect* without consuming (`tls_inspector` peeks the ClientHello for
TLS-vs-plaintext + SNI + ALPN; `http_inspector` sniffs h1 vs h2) → several **filter chains**
selected by a declarative `filter_chain_match` (SNI with `*.example.com` wildcards, ALPN,
port, source IP; most-specific-wins) → a terminal filter: `tcp_proxy` (opaque L4) or
`http_connection_manager` (L7).

Two properties matter for us:

- **One listener mixes behaviors.** The same port can have one chain doing SNI *passthrough*
  (`tcp_proxy`, no decrypt) for `db.internal` and another chain *terminating* TLS and routing
  HTTP for `*.example.com`. Terminate-vs-passthrough is a per-chain attribute, not a global mode.
- **No h1/h2/gRPC modes.** The HTTP connection manager uses `codec_type: AUTO`: ALPN if
  available, otherwise wire inference (the h2 preface). Internally every protocol is normalized
  ("HTTP/1.1 is made to look like HTTP/2 to higher layers") and routing is one virtual-host
  table (`domains:` matched against Host / `:authority`) shared by h1, h2, and gRPC — gRPC is
  just HTTP/2 with optional extra filters.

```yaml
listeners:
- address: {socket_address: {port_value: 443}}
  listener_filters: [{name: tls_inspector}]        # populates SNI/ALPN for matching
  filter_chains:
  - filter_chain_match: {server_names: ["db.internal"]}
    filters: [{name: tcp_proxy, cluster: db}]      # SNI passthrough, never decrypted
  - filter_chain_match: {server_names: ["*.example.com"]}
    transport_socket: {tls: ...}                   # terminate here
    filters:
    - name: http_connection_manager
      typed_config:
        codec_type: AUTO                           # h1/h2 negotiated, never configured
        route_config:
          virtual_hosts:
          - domains: ["api.example.com"]
            routes: [{match: {prefix: "/"}, route: {cluster: api}}]
```

### 3.2 nginx — the cautionary tale of two modes

nginx is the one proxy that *does* expose two user-facing top-level modes: `stream {}` (L4)
and `http {}` (L7), with separate listen sockets. Within `stream`, `ssl_preread on;` gives
Envoy-style SNI/ALPN inspection without termination, and a `map` turns it into a backend:

```nginx
stream {
  map $ssl_preread_server_name $upstream {
      backend.example.com  backend1;
      default              backend2;
  }
  server { listen 443; ssl_preread on; proxy_pass $upstream; }
}
```

Within `http`, everything is unified again: `server_name` virtual hosts, `http2 on;` on the
listen socket, and gRPC is just `grpc_pass` in a location — the same routing model for h1, h2,
and gRPC. The lesson cuts both ways: even nginx never makes h1/h2/gRPC separate modes, but its
stream/http split is widely considered its most awkward seam (you cannot share a port or a
routing table across the two) — it is the pattern our six import flags generalize, and the one
the newer proxies deliberately avoided.

### 3.3 Traefik — entrypoints → routers → services

**Entrypoints** are just ports. **Routers** attach rules to them (`HostSNI(\`a.com\`)` for TCP,
`Host(\`a.com\`)` for HTTP) and point at services. TCP and HTTP routers **coexist on one
entrypoint** (TCP rules evaluated first, HTTP takes over if none match). Passthrough is a
per-router boolean (`tls: {passthrough: true}`). gRPC "works without specific configuration";
the backend's protocol is a property of the *target URL scheme* (`h2c://` for plaintext HTTP/2).
Notably for a CLI-first tool: Traefik allows flags/env for the *static* frame (ports, providers)
but routing data is always declarative (file, labels, CRDs) — flags bootstrap, they don't route.

### 3.4 Discovery-driven systems — the bridge's real peer group

The §2.4 observation (the bus is the control plane) means the closest cousins are not the
classic proxies at all, but the *interconnects*:

**Envoy + xDS.** Envoy alone is a pure data plane — its listeners, route tables, clusters, and
endpoints are all *pushed at runtime* over the xDS APIs (LDS/RDS/CDS/EDS), and the node itself
is configured with nothing but a bootstrap ("how do I reach the management server"). A
standalone Envoy in a dynamic mesh is inert; Istio/Consul/Gloo exist to be that management
server. The bridge fuses the split: Zenoh liveliness tokens and key propagation *are* the xDS
layer, with no separate control-plane deployment.

**Traefik providers.** Traefik's static config is install-time (ports, providers); routing is
*dynamic config* watched from providers — a Docker container that starts with
``labels: ["traefik.http.routers.whoami.rule=Host(`whoami.example.com`)"]`` instantly gets a
router, and the route vanishes when the container dies. Same shape as our
`{service}/{dns}/available` liveliness token — except Traefik's announcement channel is a
local API (Docker socket, k8s API), single-cluster scope, while Zenoh tokens propagate across
a WAN mesh.

**Skupper — the closest cousin (they literally say "routing key").** Skupper builds an L7
service network over an AMQP router mesh. The mapping is one-to-one: *site* ≈ bridge process,
*link* ≈ Zenoh peering, **connector** (expose a local workload under a `routingKey`) ≈ our
`--export`, **listener** (open a local port matched to a `routingKey`) ≈ our `--import`:

```yaml
kind: Connector    # site "east" — the exposer          ≈ --backend backend/…
spec: { routingKey: backend, selector: app=backend, port: 8080 }
---
kind: Listener     # site "west" — the accessor          ≈ --listen backend/…
spec: { routingKey: backend, host: east-backend, port: 8080 }
```

Reachability propagates through the router network automatically; every site sees every
exposed service; delivery is anycast or multicast, so **N listeners × M connectors is the
native topology** — one connector serving five listeners needs zero extra config, exactly the
"one bridge serves five bridges" property. Users configure only local attachment points plus
the inter-site links. Notably, Skupper does **no L7 sniffing**: one listener per routing key.

**OpenZiti (brief).** Services defined in a central controller; SDK-embedded backends
create/destroy *terminators* automatically on connect/disconnect (≈ liveliness tokens); clients
dial by service name; multi-terminator cost-based selection gives many-to-many. Heavier
(central controller, identity layer) where Zenoh/Skupper are federated.

**Consul mesh gateways — prior art for our differentiator.** A Consul mesh gateway at the
datacenter edge **sniffs TLS SNI and routes on it without decrypting** — no certs at the
gateway — collapsing all cross-DC services onto one port. Caveat: the SNI values are
mesh-minted (`<service>.<dc>…consul`) and the SNI→route map is still pushed via xDS. The
bridge generalizes the trick to *user-facing* hostnames, plaintext HTTP, and terminated h2,
minting the pub/sub key directly from the sniff with no pushed map at all.

### 3.5 Caddy and HAProxy, briefly

Caddy makes the domain the config: `example.com { reverse_proxy h2c://backend }` gets certs,
h1/h2/h3 negotiation, and gRPC with zero protocol configuration — again, backend protocol lives
in the target address. Its raw-TCP sibling (caddy-l4) uses the same matcher/handler
decomposition as Envoy. HAProxy is frontend/backend with per-frontend `mode tcp|http`; SNI
passthrough is a `use_backend ... if { req.ssl_sni -i ... }` rule in a tcp frontend, termination
is `ssl crt` on the bind line — per-frontend attributes, not global modes.

---

## 4. Design principles to copy

1. **One listener concept.** Ports are dumb; behavior is attached per-connection by inspection
   and match rules. Nobody ships `--http-listen` vs `--tls-listen` vs `--ws-listen`.
2. **Sniff, don't configure.** Protocol identification (TLS? h1? h2 preface? WS upgrade?) is
   the proxy's job, done by peeking without consuming. The user never states the client's
   protocol. We already have this (`flowscope::classify` + peek in `auto.rs`) — it just isn't
   the default door.
3. **h1 = h2 = gRPC, one routing table.** The codec is negotiated (ALPN on TLS, preface on
   plaintext); the routing key is Host / SNI / `:authority` — semantically the same hostname.
   A hostname-keyed table serves all three. Our Zenoh key space
   (`{service}/{dns}/...`) *is* that table already.
4. **Terminate vs passthrough is an attribute, not a mode.** Per filter-chain (Envoy), per
   router (Traefik), per frontend (HAProxy). It is the "who holds the cert" decision — the one
   genuine user choice on axis 3 (§2.2).
5. **Backend protocol belongs to the target address.** `ws://`/`wss://`, `h2c://` schemes on
   the export side, not extra flags.
6. **Configure attachment points, discover routes.** The consensus across every
   discovery-driven system (§3.4) — xDS bootstrap, Traefik static config, Skupper
   connector/listener, Ziti enrollment — is that a node states only *what it listens on, what
   it exposes, and how it joins the fabric*. The route table is always propagated, never
   written. The bridge already works this way; the simplification is to make the CLI say no
   more than that. A config file (if ever added) should likewise enumerate attachment points
   only — a `[[routes]]` section would be a step *backwards* from what the bus provides.
7. **Sniff-to-mint is the differentiator — keep it central.** Skupper and Ziti need one
   listener per routing key; Consul's SNI-sniffing gateway needs a pushed SNI map. Sniffing
   SNI/Host/`:authority` to *mint* the Zenoh key dynamically lets one port serve arbitrarily
   many `{service}/{dns}` routes with zero per-hostname config anywhere in the mesh. This is
   why auto-detect should be the *default* door, not a sixth mode.

---

## 5. Proposal

### 5.1 Target surface: two flags

**`--listen '<service>/<addr>[,opt...]'`** — replaces all six import flags. Default behavior is
today's `--auto-import`: peek, classify, route TLS by SNI, HTTP/1 by Host, WebSocket upgrades
transparently, everything else opaque. Options cover the two *real* decisions from §2.2:

| Option | Meaning | Today's equivalent |
|---|---|---|
| *(none)* | auto-detect, TLS passthrough | `--auto-import` |
| `proto=raw` | no peek, opaque L4 tunnel | `--import` |
| `tls=terminate` | terminate TLS (needs `--tls-cert/--tls-key`); h1 by Host, h2 by `:authority` via ALPN | `--https-terminate` |
| `route=request` | per-request Host routing on keep-alive h1 | `--http-multiroute-import` |

`proto=raw` must stay a real option, not just a compatibility alias: auto-detect *peeks and
waits for client bytes*, which breaks server-speaks-first protocols (SMTP, MySQL greetings —
the server banner never arrives because the bridge is waiting on the client). Envoy has the
same escape hatch (listener-filter timeout); nginx stream simply doesn't preread unless asked.

**`--backend '<service>[@<host>]/<target>'`** — replaces all three export flags. The target's
scheme carries the protocol (principle 5): `127.0.0.1:8003` is TCP, `ws://127.0.0.1:9000` is
WebSocket. The optional `@host` part registers `{service}/{host}/available` for hostname
routing (today's `--http-export`), and drops the ambiguous three-slash-part spec format.

### 5.2 Migration table

| Today (9 flags) | Proposed (2 flags) |
|---|---|
| `--import svc/a:p` | `--listen svc/a:p,proto=raw` |
| `--http-import svc/a:p` | `--listen svc/a:p` |
| `--auto-import svc/a:p` | `--listen svc/a:p` |
| `--ws-import svc/a:p` | `--listen svc/a:p` (WS is auto-detected) |
| `--http-multiroute-import svc/a:p` | `--listen svc/a:p,route=request` |
| `--https-terminate svc/a:p` | `--listen svc/a:p,tls=terminate` |
| `--export svc/b:p` | `--backend svc/b:p` |
| `--http-export svc/dns/b:p` | `--backend svc@dns/b:p` |
| `--ws-export svc/ws://u` | `--backend svc/ws://u` |

Semantic deltas are tiny: `--ws-import` *forced* the WS upgrade where auto-detect requires the
request to look like one (`peek_is_websocket`) — in practice identical for real WS clients; and
`--http-import` already routed both Host *and* SNI (despite its name), which is exactly what
the unified listener does.

### 5.3 Close the one protocol gap: plaintext h2 (h2c / prior-knowledge gRPC)

`auto.rs` currently **rejects** `WireProtocol::Http2Preface`. The fix is small and makes the
"HTTP/1 + gRPC + HTTP/2" requirement hold in every cell of the §2.5 matrix:

- Wire the existing `read_h2_head` (`import/connection.rs`, built for terminated h2 in PR #61)
  into the auto-detect path: peek the preface + first stream's `:authority`, route, relay
  opaquely — the same single-authority relay semantics as terminated h2.
- This requires moving flowscope's `http2` feature from `tls-termination`-gated to always-on
  (it is pure-compute, no async/caps — the gating was only there because terminated h2 was its
  sole consumer), and un-gating `read_h2_head` + its tests.

This is Envoy's `codec_type: AUTO` in miniature: TLS → ALPN decides, plaintext → preface
decides, user configures nothing.

### 5.4 Coverage guarantee after the collapse

The two-flag surface must not silently drop anything the nine flags carried. Checklist against
the required protocol set:

| Requirement | How the unified surface serves it |
|---|---|
| Raw TCP (rsync, scp, SSH, Postgres, Redis, …) | `--listen ...,proto=raw` (opaque, no peek — safe for server-speaks-first protocols) or plain `--listen` when client speaks first |
| HTTP/1.1 | `--listen` (Host-routed), `route=request` for per-request fan-out, `tls=terminate` for HTTPS |
| gRPC / HTTP/2 over TLS | `--listen` (SNI passthrough, zero-decrypt) or `tls=terminate` (`:authority`-routed) |
| h2c / plaintext gRPC | `--listen` after §5.3 lands |
| `ws://` | `--listen` (upgrade auto-detected) → `--backend svc/ws://…` |
| `wss://` | `--listen` (TLS → SNI passthrough, opaque) or `tls=terminate` (add the missing e2e test); `--backend svc/wss://…` already parses |

### 5.5 What *not* to build

- **Per-stream h2 demux** (routing different h2 streams of one connection to different
  backends) — that is a full HTTP/2 proxy (Envoy HCM territory). The single-authority relay is
  the right scope for a byte-relay bridge; keep it documented as such.
- **Per-request routing for h2** — same reason. `route=request` stays h1-plaintext-only.
- **A rules language** (path matching, header predicates, middlewares). The Zenoh key space is
  the routing table; hostname is the key. Adding Traefik-style rules would recreate the
  complexity we are removing.

### 5.6 Phased plan

1. **Phase 1 — CLI collapse (no behavior change).** Add `--listen`/`--backend` spec parsing
   that dispatches to the existing mode functions. Old flags become hidden deprecated aliases.
   README rewrites the matrix around the two flags. Pure `args.rs`/`main.rs` work.
2. **Phase 2 — h2c routing (§5.3) + coverage tests.** Un-gate flowscope `http2`, handle
   `Http2Preface` in the auto path. New e2e tests: plaintext gRPC client → `--listen` → export
   → h2c backend, and a terminated-`wss://` round-trip (closes the ⚠ in §2.5).
3. **Phase 3 — internal unification.** Collapse the five accept loops
   (`listener.rs`/`auto.rs`/`ws.rs`/`tls.rs`/`multiroute.rs`) into one generic listener
   parameterized by a connection handler (the Envoy listener/filter-chain split, in miniature).
   Enables per-listener `tls=terminate` + passthrough coexistence later, if ever wanted.
4. **Phase 4 — optional config file** (principle 6). A small TOML/JSON5 `[[listen]]` /
   `[[backend]]` file for deployments with many *local attachment points*,
   `--listen`/`--backend` remaining the one-liner path. Attachment points only — no routes
   section, ever: the mesh's route table lives on the bus (§3.4), and a Skupper-style
   site file is the model, not an Envoy route config. Only worth doing when a real deployment
   outgrows the CLI.
5. **Phase 5 — remove deprecated flags** at the next breaking release (0.7 or 1.0).

### 5.7 Net effect

| | Before | After |
|---|---|---|
| Routing flags | 9 | 2 (+2 TLS cert flags) |
| User decisions | pick 1 of 6 import modes | 2 orthogonal options, both defaulted |
| h1/h2/gRPC | supported in *some* modes, user must consult a matrix | negotiated everywhere (ALPN + preface), matrix deleted |
| Accept loops in code | 5 | 1 (Phase 3) |
| "Which mode do I use?" README section | needed | obsolete |

---

## Sources

- Envoy: [tls_inspector](https://www.envoyproxy.io/docs/envoy/latest/configuration/listeners/listener_filters/tls_inspector),
  [FilterChainMatch](https://www.envoyproxy.io/docs/envoy/latest/api-v3/config/listener/v3/listener_components.proto),
  [HCM `codec_type: AUTO`](https://www.envoyproxy.io/docs/envoy/latest/api-v3/extensions/filters/network/http_connection_manager/v3/http_connection_manager.proto),
  [HTTP connection management](https://www.envoyproxy.io/docs/envoy/latest/intro/arch_overview/http/http_connection_management)
- nginx: [ssl_preread](https://nginx.org/en/docs/stream/ngx_stream_ssl_preread_module.html),
  [grpc module](https://nginx.org/en/docs/http/ngx_http_grpc_module.html)
- Traefik: [TCP routers](https://doc.traefik.io/traefik/reference/routing-configuration/tcp/routing/router/),
  [gRPC guide](https://doc.traefik.io/traefik/v3.6/user-guides/grpc/)
- Caddy: [automatic HTTPS](https://caddyserver.com/docs/automatic-https), [caddy-l4](https://github.com/mholt/caddy-l4)
- HAProxy: [SNI-based load balancing](https://www.haproxy.com/blog/enhanced-ssl-load-balancing-with-server-name-indication-sni-tls-extension)
- Envoy xDS: [dynamic configuration overview](https://www.envoyproxy.io/docs/envoy/latest/intro/arch_overview/operations/dynamic_configuration)
- Traefik: [providers overview](https://doc.traefik.io/traefik/reference/install-configuration/providers/overview/)
- Skupper: [overview](https://skupper.io/docs/overview/index.html), [listener concept](https://skupper.io/resources/listener.html),
  [YAML site example](https://github.com/skupperproject/skupper-example-yaml)
- OpenZiti: [services & terminators](https://netfoundry.io/docs/openziti/learn/core-concepts/services/overview)
- Consul: [mesh gateways (SNI-sniffing, no decrypt)](https://developer.hashicorp.com/consul/docs/east-west/mesh-gateway)

---

## Addendum (2026-08-04) — approved decisions and issue map

Final review + flowscope gap analysis done; the following decisions **supersede** the
corresponding parts of §5 and are tracked in epic
[#81](https://github.com/p13marc/zenoh-bridge-tcp/issues/81):

1. **Clean break in 0.7.0** — the 9 flags are removed outright; no deprecation aliases
   (supersedes §5.6 Phases 1 & 5's alias step).
2. **Comma key=value option syntax**: `--listen '<svc>/<addr>[,proto=raw][,cert=P,key=P][,route=request]'`;
   `--backend '<svc>[@<host>]/<target>'`.
3. **Zenoh session flags renamed** (`--zenoh-mode/--zenoh-connect/--zenoh-listen/--zenoh-config`,
   shorts dropped) — resolves the previously unnoticed collision: `-l/--listen` was the Zenoh
   listen endpoint.
4. **Per-listener TLS material**: `cert=`/`key=` options replace the global
   `--tls-cert/--tls-key` (multiple terminating listeners with different certs;
   SNI-selected multi-cert stays out of scope).
5. **Host-routed WebSocket export** becomes supported (`--backend svc@host/ws://…`), removing
   the current `ExportBackend::WebSocket` asymmetry.
6. **Cert implies termination** (amended after review): there is no `tls=terminate` keyword —
   wherever §5 of this report writes one, read "cert=/key= present". A listener with
   `cert=`/`key=` terminates TLS; without them it is a passthrough tunnel (SNI-routed, zero
   key material on the bridge). Same convention as Envoy (a filter chain with a TLS
   `transport_socket` terminates) and Caddy (providing a cert *is* the config). Also
   simplifies validation: `cert=`/`key=` require each other and conflict with `proto=raw`
   and `route=request`.

**flowscope gaps found** (0.23.0): no request-side WS-upgrade detection
([flowscope#204](https://github.com/p13marc/flowscope/issues/204)); `HttpProxyParser::push`
reports post-switch dropped bytes as consumed
([flowscope#205](https://github.com/p13marc/flowscope/issues/205)); h2 raw-head parity filed
unscheduled ([flowscope#206](https://github.com/p13marc/flowscope/issues/206)). Confirmed
non-gaps: ClientHello ALPN exposure, prior-knowledge h2c parsing, gRPC status both directions.

**Issue map:** #71 wss-terminate e2e safety net → #72 spec grammar + CLI swap (two PRs) →
#76 generic head reader → #73 generic accept loop → #75 dns_suffix + WS-export host routing →
#74 h2c/gRPC `:authority` routing (with RFC 9113 §3.4 timeout fallback to opaque relay) →
#77 RFC-correct WS detection (flowscope 0.24) → #80 ALPN surfacing → #78 docs + 0.7.0
release. Backlog: #79 attachment-points config file (no routes section, ever — §3.4).
