# zenoh-bridge-tcp — Technical Analysis

*Analysis date: 2026-08-01. Scope: correctness, behavior, security, protocol support (HTTP/HTTPS/gRPC/raw), a benchmark plan, and opportunities to reuse the sibling projects **flowscope** and **netring**. Backward-compatibility breaks are explicitly permitted per the request.*

---

> ### Guarantees this analysis holds the design to
>
> **Capability budget — the bridge core stays cap-free and cross-platform.** It must run as an unprivileged process: no `CAP_NET_RAW`, no `CAP_NET_ADMIN`, no root, no raw sockets, no Linux-only syscalls. This rules **netring** out as a dependency (§11) and makes **flowscope** — pure-compute, no tokio, no OS deps — the only adoptable of the two (§10).
>
> **Protocol reassurance — HTTPS and gRPC-over-TLS work *today*.** Use SNI passthrough (`--http-import` or `--auto-import`): the bridge relays the encrypted stream opaquely and never touches ALPN, so HTTP/2 and gRPC ride through and still get SNI routing. What does *not* work is *terminated* HTTP/2 (`--https-terminate` sets no ALPN) and HTTP/1.1-only `--http-multiroute-import`. Full matrix in §7.

---

## 1. Executive summary

The bridge is well-structured and its happy paths work, but it treats the Zenoh pub/sub bus as if it were a reliable, ordered byte pipe — and it is not, under the current configuration. The most serious problems are not crashes; they are **silent** data-integrity and lifecycle defects that only surface under load, on error, or on partial connection close, and the test suite (strong on happy paths) does not exercise them.

The five must-fix issues, in priority order:

1. **Silent stream corruption over Zenoh** — publishers use the default `CongestionControl::Drop`, and unrecoverable sample-misses are flushed past the gap with only a log warning. A dropped or lost sample delivers *corrupted* bytes to the TCP peer, undetected. (§3)
2. **A backend read/publish error hangs the client forever** — no EOF and no error signal is emitted on the mid-stream error path, and the export holds no liveliness token to fall back on. (§4, C1)
3. **TCP half-close (FIN) is neither propagated nor tolerated** — a client that shuts down its write half after sending a request has its response discarded. `printf 'GET / HTTP/1.0\r\n\r\n' | nc -N host port` reproduces it. (§4, B1/B2)
4. **`Host` header is injected unvalidated into Zenoh key expressions** — `Host: *` bypasses the backend-availability gate; the SNI path validates hostnames rigorously while the HTTP path validates nothing. (§6, F1)
5. **HTTP multiroute mishandles request bodies, HEAD, 1xx, pipelining, and upgrades** — the least-tested 340 lines in the codebase. (§5)

Everything here is fixable, and most fixes are small. Later sections then argue that a substantial share of the parsing surface — and all of the missing observability — can be *deleted and replaced* by adopting the author's own **flowscope** library (§10), with **netring** kept strictly out-of-process as a sidecar/design-reference (§11) so the bridge stays capability-free. §7 gives the per-mode HTTP/HTTPS/gRPC/raw support matrix, and §8 specifies a benchmark to confirm rsync/scp-grade throughput.

**Section map:** §2 severity table · §3 data-integrity deep dive · §4 half-close & lifecycle · §5 multiroute HTTP · §6 security · **§7 protocol support matrix (HTTP/HTTPS/gRPC/raw)** · **§8 benchmark plan** · §9 improvements & new features · §10 flowscope (readiness matrix + phased adoption + regression gate) · §11 netring (no-dependency/sidecar-only) · §12 roadmap.

**A note on file:line references:** these were accurate at analysis time against the current tree. Treat them as entry points; verify against the working copy before editing.

---

## 2. Severity-ranked findings

| # | Sev | Area | Location | Symptom |
|---|-----|------|----------|---------|
| A1 | 🔴 Critical | Data integrity | `export/bridge.rs:298`, `import/bridge.rs:198` | Default `Drop` congestion control silently drops payload bytes under TX pressure → stream corruption |
| A2 | 🔴 Critical | Data integrity | `export/bridge.rs:241,261`; `import/bridge.rs:54,74` | Unrecoverable sample-miss is gap-flushed; no `sample_miss_listener`, no teardown → corrupted bytes delivered |
| C1 | 🔴 Critical | Lifecycle | `export/bridge.rs:298-307` | Backend mid-stream read/publish error emits no EOF/error → client hangs forever |
| B1 | 🟠 High | Half-close | `import/bridge.rs:192-195`; `export/bridge.rs:333-339` | Client→backend FIN never signaled; backend read-half never closes |
| B2 | 🟠 High | Half-close | `import/bridge.rs:241-243` | Client half-close cancels the response direction before draining → response discarded |
| F1 | 🟠 High | Security | `http_parser.rs:232-251`; `multiroute.rs:136,152-159` | `Host` header interpolated into key exprs unvalidated; `Host: *` bypasses availability gate |
| A3 | 🟠 High | Data integrity | `export/bridge.rs:260`; `import/bridge.rs:73` | 64-sample cache too small for the startup window; large uploads lose early samples |
| E1 | 🟠 High | HTTP multiroute | `multiroute.rs:115-120,193-196` | Request body truncated; leftover bytes parsed as a bogus next request |
| C2 | 🟠 High | Lifecycle | `import/bridge.rs:254-280` | `error_monitor` false/err path leaks two detached tasks |
| D2 | 🟠 High | Backpressure | `export/bridge.rs:331` (+ flume 256 default) | One slow TCP writer head-of-line-blocks every client on the shared session |
| B3 | 🟡 Med | Half-close | `export/bridge.rs:316` | Backend EOF unconditionally cancels peer → queued client→backend data dropped |
| D1 | 🟡 Med | Resource | `export/bridge.rs:78` | Export client-map entries never freed on natural completion → unbounded growth |
| D3 | 🟡 Med | Resource | `listener.rs:63` (+ siblings) | No connection cap on any listener |
| E3 | 🟡 Med | HTTP multiroute | `multiroute.rs:230-236,207` | `HEAD` responses hang ~30s (method not remembered for framing) |
| E4 | 🟡 Med | HTTP multiroute | `multiroute.rs:246-249` | `Expect: 100-continue` / any 1xx interim response deadlocks |
| E5 | 🟡 Med | HTTP multiroute | `http_parser.rs:113-118` (unused) | WebSocket upgrade / `CONNECT` / `h2c` silently mishandled in multiroute |
| E6 | 🟡 Med | Security/HTTP | `multiroute.rs:230-233` | Response-parse errors swallowed → smuggling defenses never block, only stall |
| F2 | 🟡 Med | Security | `export/mod.rs:46-60` | `--export`/`--import` service names unvalidated → `*/...` subscribes to all services |
| F4 | 🟡 Med | DoS | `auto.rs:105`, `connection.rs:31`, `ws.rs:66`, `tls.rs:74` | Untimed peek/handshake + no data-phase read timeout → cheap idle-connection exhaustion |
| G2 | 🟡 Med | Correctness | `transport.rs:137-138` vs `:22-24` | Zero-length WebSocket frame read as EOF → silent teardown |
| C3 | 🟢 Low | Observability | `export/bridge.rs:424` | `handle_client_bridge` always returns `Ok(())`; subtask failures swallowed |
| D4 | 🟢 Low | Resource | `multiroute.rs:218-220` | Response fully buffered in RAM while also streaming; limit checked after extend |
| D5 | 🟢 Low | Resource | `listener.rs:94` (+ siblings) | Finished tasks reaped only on next accept |
| E2 | 🟢 Low | HTTP multiroute | `multiroute.rs:239,243-272` | HTTP/1.1 pipelining broken both directions |
| E7 | 🟢 Low | Latency | `multiroute.rs:190` | Hard-coded 100ms sleep per request (race band-aid) |
| F3 | 🟢 Low | Correctness | `http_parser.rs:154-163,184-196,233` | Absolute-URI vs Host precedence inverted; duplicate Host accepted; Unicode lowercasing |
| G1 | 🟢 Low | API | `transport.rs:75,129` | `buffer_size` argument to `read_data` is ignored |
| G3 | 🟢 Low | Config | `args.rs:248-252`; `multiroute.rs:333` | Four config fields have no CLI flag; availability timeout hardcoded to 1000ms |
| G5 | 🟢 Low | Lifecycle | `main.rs:80-84,326` | Process hangs with zero listeners if all bridge tasks die (e.g. `EADDRINUSE`) |
| G6 | 🟢 Low | TLS | `tls_parser.rs`; `connection.rs:80,122` | Split ClientHello unparseable; can write an HTTP 400 onto a TLS socket |

Legend: 🔴 critical (silent corruption or permanent hang) · 🟠 high (data loss / security / DoS in realistic use) · 🟡 medium · 🟢 low.

---

## 3. Data-integrity deep dive — the defining risk

The bridge splices a TCP byte stream across Zenoh pub/sub. TCP promises **reliable, ordered, gap-free** delivery; the current Zenoh configuration promises none of those three, and the bridge does nothing to reconcile the difference. Three compounding issues:

**A1 — `CongestionControl::Drop` (the default).** There is no `.congestion_control(...)` call anywhere in `src/`. Zenoh's default for `put`/push traffic is `Drop`: when the session's transmit queue is full, samples are dropped locally, before they ever hit the wire. For telemetry that is a sensible default; for a byte stream it is silent corruption. Every data `put` — `export/bridge.rs:298` and `import/bridge.rs:198` — is exposed.

**A2 — Unrecoverable misses are flushed past the gap.** The subscribers are `AdvancedSubscriber`s with `recovery(...)`, so out-of-order samples are buffered and a retransmission query is issued for a detected hole. But when recovery *fails* (the source is gone, or the sample aged out of every cache), zenoh-ext's `flush_sequenced_source` emits the later buffered samples anyway — skipping the hole — with only a `tracing::warn!` and a `Miss` callback. The bridge never registers a `sample_miss_listener()` (zero occurrences in the tree) and never tears the connection down on a miss. Net effect: **a byte-range vanishes from the middle of the stream and the TCP peer is none the wiser.** This is the single most dangerous behavior in the codebase because it is undetectable downstream — a corrupted file, a desynced protocol, a truncated response, with no error anywhere.

**A3 — The 64-sample cache is too small for the startup window.** `CacheConfig::default().max_samples(64)` at `export/bridge.rs:260`, `import/bridge.rs:73`, and `multiroute.rs:181`. The import publishes the buffered initial request (and then streams the client's data) *before* the export has connected to the backend — and the export's backend connect is a backoff retry loop of up to ~3.1s (`export/tcp.rs:31-33`). Any client that streams more than 64 chunks in that window (~4 MiB at the 64 KiB default `buffer_size`) evicts its earliest samples from the cache before the export's subscriber ever queries history, so the history reply is already incomplete — which then triggers the A2 gap-flush. This is not an exotic race; it is the ordinary path for a large upload arriving immediately after connect.

**Recommended fix (correctness mode).** Introduce an explicit reliability posture for the data-plane publishers and subscribers:
- Set `.congestion_control(CongestionControl::Block)` and `.priority(Priority::RealTime or DataHigh)` on the data publishers so a full queue applies backpressure instead of dropping.
- Register a `sample_miss_listener()` on both data subscribers and, on any unrecoverable miss, **reset the TCP connection** (RST) rather than deliver corrupted bytes — a reset is a correct, detectable failure; a silent gap is not.
- Raise or make configurable the cache `max_samples`, and gate the import's data publishing on backend readiness (the `available` liveliness token already exists for HTTP mode; extend the same handshake to raw TCP) so the startup eviction window closes.

These are behavior changes and may be worth a `--reliability {stream|telemetry}` flag, but for a TCP bridge the stream posture should be the default.

---

## 4. Half-close, error paths, and lifecycle

The bidirectional relay models each direction as a task with a `CancellationToken`, and the outer `select!` cancels the *peer* direction as soon as *either* completes. For a full-duplex byte stream that is wrong: the two directions are independent, and closing one must not tear down the other.

**B1 — Client→backend EOF is never signaled.** When the TCP client half-closes, `client_to_zenoh` sees an empty read and just `break`s (`import/bridge.rs:192-195`). The export side's `zenoh_to_backend` has no empty-payload→EOF branch (`export/bridge.rs:333-339`) — it would `write_all(&[])`, a no-op. So the backend's read half never observes FIN. Request/response backends that wait for FIN-as-end-of-request (some line protocols, `HTTP/1.0` with `Connection: close`) stall.

**B2 — Worse, the response direction is cancelled immediately.** When `client_to_zenoh` finishes, the arm at `import/bridge.rs:241-243` calls `cancel_zenoh_to_client.cancel()` *before* the `drain_timeout` wait — so the drain only bounds waiting for a task already told to stop. Repro: `printf 'GET / HTTP/1.0\r\n\r\n' | nc -N host port` (or any client, e.g. gRPC clients, that shuts down its write half after the request) — the response is discarded and the client gets an empty reply.

**B3 — Symmetric on the export side.** `backend_to_zenoh` exiting calls `signal_peer_z2b.cancel()` unconditionally (`export/bridge.rs:316`), so on backend EOF any still-queued client→backend data is dropped.

**C1 — The mid-stream backend error path is a permanent hang.** On clean backend EOF the export publishes an empty-payload EOF marker (`export/bridge.rs:289-295`). But on a backend *read error* (`:303-307`) or *publish error* (`:298-301`) it just `break`s — no EOF, no error sample. The `error/{client_id}` channel is only ever used for *initial connect* failure (`export/tcp.rs:117-122`), never mid-stream. Because the export holds no liveliness token of its own, the import has no secondary signal that the peer is gone: its subscriber simply stops receiving, and the client connection stays open indefinitely. This also feeds D1 (the export's client-map entry leaks).

**C2 — `error_monitor` leaks tasks on its non-happy paths.** `import/bridge.rs:254-280` only acts `if let Ok(true)`. If the error subscriber's channel closes (returns `false`) or the monitor task returns `Err(JoinError)`, the arm body does nothing, the function undeclares the liveliness token and returns `Ok(())` — while `zenoh_to_client` and `client_to_zenoh` are still running. Dropping a `JoinHandle` does not abort a Tokio task, so both continue detached, holding the reader/writer, while the export sees the liveliness `Delete` and kills the backend.

**Recommended fixes.**
- Decouple the two directions: half-close on one side should propagate a *directional* EOF (see the framing proposal in §9.1), not cancel the peer. Only cancel the peer after *its own* drain completes or the drain timeout fires.
- Make the EOF marker unambiguous and emit it on *every* terminal path, including read/publish errors, so the peer always learns the stream ended (and whether cleanly). Distinguish "clean EOF" from "aborted" so the receiving side can send FIN vs RST appropriately.
- In `error_monitor`'s false/err arms, cancel both directions and await them (bounded) before returning — never leave detached tasks.

---

## 5. HTTP multiroute correctness

`src/import/multiroute.rs` implements per-request Host routing on a persistent keep-alive connection. It is 340 lines with three happy-path integration tests (all bodyless `GET`s), and essentially every non-trivial HTTP/1.1 behavior is mishandled:

- **E1 — Request bodies are truncated.** `parse_http_request` returns as soon as the *headers* are complete (`multiroute.rs:115`); the body is never read. Any `POST` whose body arrives in a later TCP segment — which is the norm for curl, Go's `net/http`, and most Java clients, all of which flush headers first — is forwarded to the backend truncated. The leftover body bytes then sit in the socket and are parsed as the *next* request line (`:193-196`), fail, and silently `break` the loop.
- **E2 — Pipelining is broken both ways.** Two pipelined requests in one segment are published as one blob; the bridge treats the first response as the whole response and drops the second. Conversely, bytes the backend sends after the first complete response (in the same Zenoh sample) are written to the client verbatim before the completeness check, desyncing the client stream.
- **E3 — `HEAD` hangs ~30s.** Body framing is derived from the response alone (`:230-236`); the request method is not remembered. A `HEAD` reply with `Content-Length: N` and no body waits for N body bytes that never come, until the 30s timeout at `:207`.
- **E4 — `Expect: 100-continue` and any 1xx deadlock.** `100 Continue` parses as `NoBody` → "response complete" → the per-request pub/sub and liveliness are torn down, and the loop waits for a new request while the client waits for the real response. Same for `103 Early Hints`.
- **E5 — Upgrades are mishandled.** `parsed.is_websocket_upgrade` (`http_parser.rs:113-118`) is never read in multiroute. A WebSocket upgrade, an `Upgrade: h2c`, or a `CONNECT` tunnel all get their `101`/tunnel response parsed as `NoBody`, the machinery is torn down, and the loop then tries to parse the client's post-upgrade frames as an HTTP request → silent break.
- **E6 — Response-parse errors are swallowed** (`if let Ok(Some(...))` at `:230-233`). A response that trips the RFC-7230 checks (`TE`+`CL`, conflicting `CL`s, `CL` > 1 GiB) or exceeds 64 headers returns `Err`; `body_framing` stays `None`, every byte is still forwarded, and the connection hangs until the 30s timeout. **The request-smuggling defenses in `http_response_parser.rs` therefore never actually block anything — they only convert a bad response into a stall.**
- **E7 — A hard-coded 100ms sleep per request** (`multiroute.rs:190`), labeled "for Zenoh subscriber/publisher to establish," directly contradicts the comment at `import/bridge.rs:113` ("No sleep needed!"). It is a permanent latency floor on every keep-alive request and an amplification lever.

Given the breadth of these defects, the highest-leverage move is to replace multiroute's request/response framing with flowscope's `HttpExchangeParser` rather than hand-patch it — **but** flowscope's response framing is not complete today (no chunked, not method-aware, no 1xx/upgrade; see the readiness matrix in §10.2), so this replacement is gated on the Phase-B upstream work in §10.3. In the interim, the §10.1 regression net should pin these behaviors and the worst offenders (E1 body truncation, E3 HEAD hang) can be hand-fixed.

---

## 6. Security posture

**F1 — `Host` header → Zenoh key-expression injection (highest security severity).** The normalized `Host` value flows straight into key expressions with no character validation. `normalize_dns` (`http_parser.rs:232-251`) only lowercases and strips ports 80/443; the result is interpolated at `multiroute.rs:136,152-159` and (via `connection.rs`) `import/bridge.rs:37,50,66,85`. Because `*` and `/` are *valid* key-expression syntax:
- `Host: *` makes the availability probe `svc/*/available`, which matches **every** registered DNS backend — the "is this host available/authorized?" gate is bypassed for any hostname.
- `Host: a/b` grafts the key-space: keys become `svc/a/b/tx/…`, crossing tenant boundaries.

The asymmetry is stark: the **SNI path validates hostnames rigorously** (`validate_sni_hostname`, `tls_parser.rs:147-199` — ASCII-only, ≤253 bytes, per-label ≤63, alnum+hyphen, no leading/trailing hyphen or trailing dot), and the identical interpolation on the **HTTP path validates nothing**. Fix: run the `Host` value through the same validation as SNI (or a strict `[a-z0-9.-]` allowlist) before it can become a key segment, and reject `*`, `/`, and other key-expression metacharacters outright.

**F2 — Service names from the CLI are unvalidated key expressions.** `export/mod.rs:46-60` and `import/mod.rs:44-58` only split on `/`. `--export '*/127.0.0.1:80'` subscribes to `*/clients/*` — every service's clients. This is operator-supplied, so lower risk than F1, but it should still be validated at parse time (`args.rs`).

**F3 — HTTP request-target handling diverges from RFC 7230.** The absolute-URI authority should win over `Host` for absolute-form targets (§5.4); the code returns `Host` first and only falls back to the URI (`http_parser.rs:154-163`), so `GET http://internal/ HTTP/1.1` + `Host: public` routes to `public`. Duplicate `Host` headers are accepted (first wins, `:184-196`) — a classic routing-desync primitive when the backend honors the last. `normalize_dns` uses Unicode `to_lowercase()` (`:233`), so a `Host` with U+212A KELVIN SIGN routes to the ASCII `k` key while forwarding the non-ASCII bytes; `http_parser.rs:779-782` even asserts this as intended.

**F4 — Cheap idle-connection / slowloris exhaustion.** There is no connection cap on any listener, and several pre-read waits are untimed: `stream.peek(...)` in `auto.rs:105` and `connection.rs:31`, `accept_async` in `ws.rs:66`/`auto.rs:169`, and the TLS `accept` in `tls.rs:74`. A client that connects and sends nothing pins a task + fd indefinitely. The data phase has no read timeout at all (`read_timeout` only guards the header/ClientHello parse), so an idle *established* bridge also lives forever. Fix: a `Semaphore`-based connection cap per listener, a handshake/peek timeout, and an idle data-phase timeout.

**Mitigating factor worth stating:** multiroute creates a fresh liveliness token per request and the export opens a new backend TCP connection per token — backend connections are never pooled, which removes the classic CL.TE/TE.CL *backend*-desync smuggling primitive. The residual smuggling risk is client-side desync (E1/E2) and the inert response defenses (E6).

---

## 7. Protocol support matrix — HTTP / HTTPS / gRPC / raw

The bridge's data pump is **protocol-agnostic**: both directions relay raw chunks with zero payload inspection (`bridge_import_connection` at `import/bridge.rs:190-201`; `handle_client_bridge` at `export/bridge.rs:284-361`; `TcpReader::read_data` is a bare socket read, `transport.rs:74-83`). All protocol constraint comes from the per-mode *front-end* that runs before the pump. So the question "does HTTPS/gRPC/rsync work?" reduces to: does the chosen mode parse the stream, or relay it opaquely?

**Verdict matrix** (`works` = carried and routed · `works-nr` = carried but no Host/SNI routing, all clients hit one backend key · `breaks` = rejected or desynced):

| Mode (flag) | HTTP/1.1 | HTTPS passthrough | gRPC-over-TLS | gRPC h2c (cleartext) | raw rsync / scp / SSH |
|---|---|---|---|---|---|
| Raw `--import` / `--export` | works-nr | works-nr | works-nr | works-nr | **works-nr** |
| `--http-import` (HTTP + SNI on one listener) | **works** | **works** (SNI passthrough) | **works** (SNI passthrough) | breaks (400) | breaks (parsed as HTTP → 400) |
| `--https-terminate` (feature `tls-termination`) | works (over TLS) | n/a (terminates) | **breaks** (no ALPN `h2`) | n/a | breaks |
| `--http-multiroute-import` | works (per-request) | breaks | breaks | breaks | breaks |
| `--auto-import` | works | **works** (SNI routes) | **works** (SNI routes) | works-nr | works-nr |

**How to read it, per use case:**

- **rsync / scp / SSH (raw byte streams).** Use raw `--import` / `--export`. The relay is fully opaque (`import/connection.rs:127-129` takes the no-parse branch), so any byte stream — including SSH's `SSH-2.0…` banner and rsync's protocol — passes through untouched. No Host/SNI exists in these, so there is no routing key; all clients on a service map to the same backend namespace. `--auto-import` also handles them (falls back to `RawTcp`, `protocol_detect.rs:50`) if you want raw and HTTP/TLS to share one listener. §8 benchmarks this path.
- **HTTPS (browsers, HTTP/1.1 or HTTP/2 over TLS).** Use `--http-import` or `--auto-import`. TLS is **never terminated** — the SNI is read from the cleartext ClientHello (`connection.rs:33-79`, `tls_parser.rs:204-250`) and the encrypted bytes are relayed verbatim. This is textbook L4 SNI passthrough.
- **gRPC.** gRPC is HTTP/2, and HTTP/2-over-TLS requires the client and server to negotiate ALPN `h2`. In **passthrough**, the proxy doesn't touch ALPN — the endpoints negotiate it directly — so **gRPC-over-TLS works today** through `--http-import` / `--auto-import`, routed by SNI, with no HTTP/2 awareness needed in the bridge. This matches the general L4-vs-L7 rule for gRPC ([HAProxy gRPC docs](https://www.haproxy.com/documentation/haproxy-configuration-tutorials/protocol-support/grpc/), [Red Hat: gRPC/HTTP2 ingress](https://www.redhat.com/en/blog/grpc-or-http/2-ingress-connectivity-in-openshift)).

**What does *not* work, and why:**

- **`--https-terminate` + gRPC/HTTP2.** After terminating TLS the code parses HTTP/1.1 (`import/tls.rs:128`) and, critically, `load_tls_config` never sets `alpn_protocols` (`tls_config.rs:59-62`), so the server never offers `h2`. An h2 client gets no ALPN agreement; a prior-knowledge h2 client's decrypted `PRI * HTTP/2.0…` fails the HTTP/1 parse. **Small fix, big payoff:** populate `alpn_protocols` in `tls_config.rs` so at least HTTP/1.1 negotiates cleanly and the path is ready for a future h2 parser — but true terminated-gRPC *routing* by `:authority` needs an HTTP/2 parser the bridge does not have (this is the Phase-C flowscope item, §10.3).
- **`--http-multiroute-import` + anything but HTTP/1.1.** It re-parses every request (`multiroute.rs:115`) and relies on HTTP/1 response framing to loop, so gRPC, h2c, TLS, and raw all break.
- **Plaintext h2c (`PRI * HTTP/2.0` prior knowledge).** Not in `HTTP_METHODS` (`protocol_detect.rs:19-29`) → treated as `RawTcp`: it *carries* fine over raw/auto (opaque) but gets no routing, and is rejected (400) by `--http-import`.

**Bottom line:** for HTTP, HTTPS, and gRPC-over-TLS, `--auto-import` is the single mode that carries and routes all three today; for rsync/scp use raw mode. The only gRPC gap is *terminated* h2 routing, which is deferred to §10.3 Phase C.

---

## 8. Benchmark plan — validating rsync/scp-grade throughput

rsync and scp are raw TCP streams, so they exercise the opaque raw-relay path (§7). The goal is a repeatable measurement of **throughput (MB/s)** and **added latency** versus a direct TCP connection, so "not too much overhead" becomes a number with a pass/fail line. There is **no benchmark infrastructure today** (no `benches/` dir; neither `criterion` nor `divan` in `[dev-dependencies]`), so this is a green-field addition.

**Where the overhead actually is.** Zenoh's own wire overhead is negligible — ~5 bytes/message, with published throughput near iperf baselines and up to tens of Gbps on fast links ([Zenoh vs MQTT/Kafka/DDS](https://zenoh.io/blog/2023-03-21-zenoh-vs-mqtt-kafka-dds/), [Zenoh performance](https://zenoh.io/blog/2021-07-13-zenoh-performance-async/)). The bridge's overhead will instead come from (a) the **per-chunk `to_vec()` allocation + memcpy** on the hot path (finding G1, `transport.rs:80`) plus a second copy at `publisher.put(&data[..])`, and (b) the **two-hop store-and-forward** (client → import-publish → export-subscribe → backend, and back). The benchmark exists to quantify (a)+(b) and to catch regressions when the §3/§4 reliability fixes land.

**Harness 1 — in-process micro-benchmark (criterion).** Add `criterion` as a dev-dependency and `benches/raw_throughput.rs` with `harness = false`. Drive the library directly, mirroring the existing in-process pattern at `tests/http_integration.rs:90-130`:
- Two `zenoh::open(Config::default())` sessions (peer mode, loopback); `tokio::spawn(run_export_mode)` + `run_import_mode` (both `pub`, `src/export/mod.rs:115`, `src/import/mod.rs:91`); readiness via the existing `wait_for_port` helper (`tests/common/mod.rs:49`); backend via `start_echo_server()` (`common/mod.rs:99`).
- **Control group:** the same large-buffer transfer straight through a loopback `TcpStream` with no bridge — `tests/tcp_sanity_tests.rs` is the natural baseline. Report bridge MB/s as a percentage of direct-TCP MB/s.
- **Sweep** `--buffer-size` (e.g. 16 KiB / 64 KiB / 256 KiB / 1 MiB) since it sets the chunk size and interacts with both the alloc cost (G1) and the 64-sample cache (A3). Measure unidirectional bulk (upload-shaped, like rsync sending) and a request/response ping (latency-shaped).

**Harness 2 — WAN-realistic end-to-end (extend `tests/nlink/`).** The netns topologies already parameterize `wan_delay` / `wan_loss` via `tc netem` (`multi-hop-bridge.nll:30-31`; `--wan-delay` / `--wan-loss` on the runners), but they only assert correctness — no throughput. Extend `run-multi-hop-test.sh` with a large-file transfer step (timed `dd | nc`, or `iperf3`, through the importer) capturing bytes/elapsed, and add a `rate` clause on the WAN link (currently only `delay`/`loss` are set) to model a real uplink. Sweep delay (e.g. 0 / 30 / 100 ms) and loss (0 / 0.1 / 1%) — loss is where finding A2 (silent gap-flush) and A1 (`Drop` congestion control) will show up as either corruption or throughput collapse, so run this **before and after** the §3 fixes.

**Pass/fail targets (proposed, tune on first run):**
- Loopback throughput ≥ **80%** of direct-TCP at 64 KiB buffer; investigate if below (likely the double-copy, G1).
- Added one-way latency < **1 ms** on loopback for a small request/response.
- **Zero** byte corruption or truncation across a lossy-WAN sweep (1% loss) — any mismatch is finding A1/A2 manifesting and is a release blocker, not a perf issue.

Building and running this harness is a follow-up (it adds a dependency and needs a release build); this section is the spec.

---

## 9. Improvements & new features

Backward-compatibility breaks are on the table, so these are framed as the *right* design rather than the minimal patch.

**9.1 A real message framing layer (fixes G2, B-series, and the EOF ambiguity).** Today every payload is opaque bytes and "empty payload" doubles as the EOF marker — which collides with legitimate zero-length WebSocket frames (`transport.rs:137-138`) and cannot express *directional* half-close or *aborted* vs *clean* close. Replace the bare `Vec<u8>` payload with a tiny framed message (a one-byte tag + payload, or a Zenoh attachment) carrying `Data | HalfCloseRead | HalfCloseWrite | Abort`. This single change makes B1/B2/B3, C1, and G2 expressible instead of hacked around, and lets the receiver choose FIN vs RST correctly. (Note: this is a wire-format change — bump a protocol version so old and new bridges don't silently misframe each other.)

**9.2 Reliability posture (fixes A1/A2).** A `--reliability {stream|telemetry}` flag; `stream` (the default for a TCP bridge) sets `CongestionControl::Block`, registers sample-miss listeners, and resets the connection on unrecoverable loss.

**9.3 Backpressure and load-shedding (fixes D2/D3).** Bound each listener with a connection `Semaphore`; replace the default 256-slot blocking subscriber handler with an explicit bounded channel that applies TCP backpressure per client instead of head-of-line-blocking the shared session. netring's `OverloadDetector`/`LoadShedder` (§11-ii) is a ready design blueprint even if the dependency is not taken.

**9.4 Expose the hidden config (fixes G3).** `heartbeat_interval`, `availability_timeout`, `max_header_size`, and `max_response_size` exist in `BridgeConfig` but have no CLI flag (`args.rs:248-252`) and are effectively hardcoded; `check_backend_available` even hardcodes 1000ms (`multiroute.rs:333`) instead of using the field. Thread them through `Args` and the config constructor. Add `--buffer-size` guidance since §8 shows it drives throughput.

**9.5 Observability from zero (fixes G7).** There is no metrics, health, or readiness code at all. Adopt flowscope's `flowscope_*` metric vocabulary (§10) and/or netring's `MonitorHealth` shape (§11-ii) for `/healthz`, `/readyz`, and per-connection/per-key counters — essential for running this under Kubernetes or any supervisor.

**9.6 Robust startup (fixes G5).** If every bridge task fails to start (e.g. `EADDRINUSE`), `main` currently logs and then blocks forever on the shutdown token with zero listeners. Fail fast: if all spawned bridges exit early, cancel the shutdown token and exit non-zero.

**9.7 New feature ideas enabled by the above.**
- **Connection access logs in SIEM formats** (Suricata EVE / Zeek / IPFIX / NDJSON) — free once flowscope is in (§10).
- **Per-tenant / per-key bandwidth attribution and RED metrics** — netring's `owner_bandwidth` and `red` modules are the model, copied not depended-on (§11-ii).
- **In-line abuse detection on import listeners** — port-scan / beaconing / DGA detectors from flowscope's `DetectorRegistry`, applied to clients hitting a public listener.
- **A `zenoh` sink for netring** — publish detections/flow records onto the Zenoh bus from `netring-exporters` (§11-iii), making the bridge's own bus the transport for observability.

---

## 10. flowscope integration — the strong match

**What it is.** `~/git/flowscope` (v0.22.0, MIT/Apache, edition 2024) is a runtime-free, cross-platform, **library-only** passive network-telemetry crate: no tokio, no OS-specific code, no NIC/root requirement, bounded memory. It has 67 integration-test files, criterion+dhat benches, a fuzz corpus, and CI with `fmt/clippy/test/msrv/features/docs/semver/fuzz-smoke`. It satisfies the **capability budget** (§ guarantees box) outright — verified by grep: zero `tokio`/`async`/`libc`/`AF_PACKET`/`cfg(unix)` in the default + `http` + `tls` code paths; the `SessionParser` trait is explicitly sync (`src/session.rs:483`). This is why flowscope is adoptable and netring (§11) is not.

**Why it fits this bridge exactly.** Its `SessionParser` trait consumes precisely the two byte-stream directions the bridge already holds:

```rust
// flowscope/src/session.rs:518,522
fn feed_initiator(&mut self, bytes: &[u8], ts: Timestamp, out: &mut Vec<Self::Message>);
fn feed_responder(&mut self, bytes: &[u8], ts: Timestamp, out: &mut Vec<Self::Message>);
```

No packets, no pcap, no NIC, no root — just the client and backend directions of a proxied stream, which the bridge has in hand at `export/tcp.rs` / `import/connection.rs`.

### 10.1 Regression safety net — build this FIRST

Do not touch the parsers until there is a test net that pins current behavior, because the existing suite is happy-path-heavy and will not catch a regression. Verified coverage gaps (data-path integration): **zero** tests for multiroute POST/body, HEAD, chunked-through-the-bridge, 100-continue, half-close, mid-stream backend error, the TLS-termination data path, and the auto-mode TLS+WebSocket branches. The harness to build on already exists:

- **In-process driver** (fast, deterministic): `run_export_mode` + `run_import_mode` are `pub` (`src/export/mod.rs:115-170`, `src/import/mod.rs:91-171`); the template is `tests/http_integration.rs:90-130` (two `zenoh::open` sessions, spawn both modes, `wait_for_port`). Mock backend: `start_echo_server()` (`tests/common/mod.rs:99`).
- **Subprocess driver** (end-to-end): `BridgePair` in `tests/common/mod.rs:169-290`.
- One fragility to fix while here: the export side has no port to poll, so tests use a hardcoded `sleep(500ms)` (`common/mod.rs:184`) — replace with a liveliness-based readiness poll so the net is not timing-flaky.

**Add these characterization tests (each should pass on today's code, then still pass after the swap):** golden byte-stream round-trip on all three carry paths (raw, HTTP-routed, SNI-passthrough), a `nc -N` half-close, a mid-stream backend kill, and multiroute POST-with-delayed-body / HEAD / chunked / 100-continue. Several of these encode *buggy* current behavior (§4, §5) — that is intentional: the net's job is to make the refactor behavior-preserving; fix the bugs in separate, clearly-labeled commits so the two concerns don't tangle.

### 10.2 flowscope readiness matrix — what's ready vs what must be added

The previous draft of this section overstated flowscope's HTTP completeness. Verified against flowscope's source, here is the honest picture:

| Bridge need | flowscope today | Verdict |
|---|---|---|
| **TLS SNI + ALPN** routing key, with TCP-segment + **PQ ClientHello** reassembly | `TlsParser` → `TlsClientHello::sni()`/`alpn` (`src/tls/parser.rs:171-185`), reassembly confirmed (`src/tls/session.rs:346-359`, `src/tls/pq.rs`) | ✅ **Ready** — closes bug G6 |
| **HTTP/1.x Host** routing key (method, path, headers) | `HttpParser`/`HttpExchangeParser` → `HttpRequest::host()` (`src/http/types.rs:89`); pipelining handled | ✅ **Ready** |
| App-protocol **classification** (h1/h2/h3/DoH/DoT from ALPN+SNI+port) | `app_proto::classify` (`src/app_proto.rs:135,205`) | ✅ **Ready** (classify only, no h2 framing) |
| SIEM export (EVE/Zeek/IPFIX/NDJSON) + `flowscope_*` metrics | `src/emit/*`, `src/obs.rs` | ✅ **Ready** — addresses G7 |
| HTTP/1 **response completion** (chunked, HEAD-aware, 1xx/204/304, upgrade/CONNECT) | Only `Content-Length` + close-to-EOF; **chunked deferred** (`src/http/mod.rs:63`, `src/http/parser.rs:304-312`), not method-aware, no 1xx/upgrade handling | ⚠️ **Must add to flowscope** before it can replace `http_response_parser.rs`/multiroute framing |
| Explicit **`is_done()`/`is_poisoned()`** desync signaling on the HTTP parser | Trait defaults only (never returns true) | ⚠️ **Must add** |
| **HTTP/2 framing + HPACK + gRPC `:authority`** per-stream (for *terminated* gRPC routing) | Absent — "HTTP/2 out of scope" (`src/http/mod.rs:62`), no `Http2` `ParserKind` | ❌ **Large new parser** (upstream contribution) |

So flowscope wraps the *same* `httparse`/`tls-parser` crates the bridge already uses, and is genuinely fuzz-tested and PQ-aware — but it is **not a drop-in for the response-framing layer today**, and it does **not** enable terminated-gRPC routing.

### 10.3 Phased adoption

- **Phase A (now, low risk) — replace `tls_parser.rs` + `http_parser.rs`.** Adopt flowscope's `TlsParser` (SNI/ALPN, with reassembly — closes G6, the split-ClientHello bug the bridge has) and `HttpParser` (Host routing). These are ready today and cover the request-side routing key, which is what `--http-import` / `--auto-import` actually need. Net line reduction ~1,500 (the two largest hand-rolled modules) with a behavior upgrade. Also adopt `app_proto::classify` to firm up `protocol_detect.rs`.
- **Phase B (contribute upstream, then adopt) — response framing.** Add chunked decode, method-aware bodies (HEAD), 1xx/204/304, and `is_done()`/`is_poisoned()` to flowscope's HTTP parser, then replace `http_response_parser.rs` and the multiroute framing loop with `HttpExchangeParser`. This is where the §5 multiroute bugs (E1–E6) actually get fixed — but only *after* flowscope gains the missing framing, so it is gated on the upstream work, not a same-day swap.
- **Phase C (optional, larger) — terminated HTTP/2 + gRPC routing.** Contribute an HTTP/2 parser (connection preface + frame layer + HPACK dynamic table + per-`stream-id` `:authority`/`:path`) to flowscope, pair it with the ALPN fix in `tls_config.rs` (§7), and only then can `--https-terminate` route decrypted gRPC by `:authority`. Until this lands, gRPC is served via SNI passthrough (§7), which needs none of it.

**Wiring (all phases).** Instantiate a `SessionParser` per bridged connection; `feed_initiator` on client→backend bytes, `feed_responder` on backend→client, alongside the existing relay tasks — parser messages give the routing key and framing state; the raw bytes still flow over Zenoh unchanged. Adopt additively (run flowscope beside the current parser, diff outputs against the §10.1 net), then delete the replaced module once parity holds.

**Licensing tripwire.** The `ja4plus` feature is FoxIO License 1.1 (source-available, non-commercial, patent-pending) and is excluded from flowscope's `l7`/`full` sets. Plain JA3 and JA4-client (`tls-fingerprints`) are royalty-free — use those, keep `ja4plus` off. Feature-wise the bridge only needs `http` + `tls` (each pulls just `httparse`/`tls-parser` + `bytes`); the packet-oriented `extractors` feature is not required when feeding bytes directly.

---

## 11. netring integration — out-of-process only

**What it is.** `~/git/netring` (v0.29.0 workspace, `netring` + `netring-exporters`) is **Linux-only** zero-copy packet I/O (AF_PACKET TPACKET_V3, AF_XDP) plus a declarative async Monitor pipeline on tokio. It delegates flow/L7 logic to flowscope; netring is the capture+orchestration+sinks half. It is the most mature of the three (94 test files, miri + cargo-deny + a checked-in `cargo-public-api` lock in CI).

**Hard rule: netring must never become a dependency of the bridge.** It observes **wire frames on an interface** and requires `CAP_NET_RAW` (plus `CAP_NET_ADMIN` for promiscuous/XDP and `CAP_IPC_LOCK` for locked rings), and it is Linux-only. Linking it into the bridge would import that capability requirement into the bridge process and violate the capability budget (§ guarantees box) — the bridge must keep running as an unprivileged, cross-platform process. netring also *cannot* see the bridge's TCP byte streams (it reads frames off a NIC, not the proxied payload), so it could not replace or assist the data path even if caps were acceptable. Three integration shapes remain, none of which add a bridge dependency:

**(i) A separate, opt-in sidecar process.** Run netring as its own privileged process watching the bridge's listener/backend interface — `Monitor::builder().interface(...)` with `flow`/`session` subscriptions, optionally inside the bridge's container netns via `.netns(...)`. It observes the bridge from outside; the bridge itself stays cap-free. This is a deployment choice for the operator, not a code change to the bridge.

**(ii) A design reference to copy (no dependency).** Reimplement, in the bridge, the small primitives netring already validated:
- **Overload / load-shedding** — `OverloadDetector`, `OverloadConfig` (`enter_at`/`recover_at` hysteresis), `LoadShedder`, `ShedPolicy` (`src/monitor/overload.rs`) — the design blueprint for the backpressure fix D2 (§9).
- **Health/readiness** — the `MonitorHealth` shape (`src/monitor/health.rs:143`: `is_ready()`, `is_live(window)`, `active_flows()`, `drops()`) for the `/healthz`/`/readyz` endpoints the bridge lacks (G7).
- **RED metrics + owner-attributed bandwidth** (`src/monitor/red.rs`, `src/monitor/owner_bandwidth.rs`) as the model for per-Zenoh-key/per-tenant byte accounting.

**(iii) An outbound contribution living in netring's tree.** A **`ZenohAnomalySink` / `ZenohFlowExporter` in `netring-exporters`** — sibling to its OTLP/Kafka/Parquet sinks — publishing detections/flow records onto the Zenoh bus (implementing netring's object-safe `AnomalySink`, `src/anomaly/sink.rs:65`, or `FlowExporter`, `src/export/mod.rs:153`). The exporters crate exists precisely to keep heavy deps (a `zenoh` dep qualifies) out of netring core. This code ships in netring, not the bridge, so again no bridge dependency.

**Naming caution:** `netring::bridge::Bridge` (`src/bridge.rs`) is an L2 two-interface packet-forwarding bridge for IPS/transparent-tap — same word, entirely different layer from zenoh-bridge-tcp.

**Verified across both sibling trees:** neither flowscope nor netring references Zenoh today (exhaustive grep of `.rs`/`.md`/`.toml`).

---

## 12. Prioritized roadmap

**P0 — Correctness (silent corruption & hangs; do these first).**
0. **Regression safety net** — the characterization tests in §10.1 (golden round-trip on all three carry paths, half-close, mid-stream error, multiroute POST/HEAD/chunked/100-continue) plus the readiness-poll fix for the flaky export-side `sleep(500ms)`. This is the gate every later change is verified against, so it comes first.
1. A1/A2 reliability posture: `CongestionControl::Block` on data publishers + sample-miss listener that resets the connection on unrecoverable loss. (§3)
2. C1: emit an EOF/error signal on *every* terminal backend path, not just clean EOF. (§4)
3. B1/B2/B3: decouple the two directions; propagate directional half-close instead of cancelling the peer. Depends on the framing layer §9.1. (§4)
4. C2: cancel-and-await both directions on `error_monitor`'s false/err arms — no detached tasks. (§4)
5. A3: gate import data-publishing on backend readiness; raise/configure cache size. (§3)

**P1 — Hardening (data loss & security in realistic use).**
6. F1: validate the `Host` header with the SNI ruleset before it becomes a key segment; reject `*`/`/`. F2: validate CLI service names. (§6)
7. D2/D3/F4: connection caps, per-client backpressure, handshake/peek/idle timeouts — copy netring's overload/load-shedding design (§11-ii), no dependency. (§6, §9.3)
8. E1/E3/E4/E5/E6: fix multiroute framing. Prefer flowscope's `HttpExchangeParser`, but only after the Phase-B upstream framing work (§10.3); hand-patch in the interim if needed. (§5, §10)
9. G2: stop treating empty WS frames as EOF (falls out of the §9.1 framing layer). (§9)
10. G3/G5: expose hidden config; fail fast when all listeners die. (§9)
11. ALPN fix: populate `alpn_protocols` in `tls_config.rs:59-62` so `--https-terminate` negotiates HTTP/1.1 cleanly and is ready for a future h2 parser. (§7)

**P2 — Refactor, observability & measurement (structural leverage).**
12. **Benchmark harness** (§8): add the criterion in-process raw-throughput bench + direct-TCP control, and the nlink WAN extension. Run it before/after items 1 and 7 — those change throughput. Establishes the rsync/scp overhead baseline.
13. Adopt **flowscope Phase A** (§10.3): replace `tls_parser.rs` + `http_parser.rs` (closes G6, ~1,500 lines out), adopt `app_proto::classify`. Then **Phase B** (contribute chunked/method-aware/`is_done` upstream) to replace the response-framing layer and properly close item 8.
14. Observability: adopt flowscope's `flowscope_*` metrics vocabulary and a `MonitorHealth`-shaped `/healthz`/`/readyz` (netring design reference, §11-ii). (G7)
15. Optional: **flowscope Phase C** (§10.3) — contribute an HTTP/2+HPACK+`:authority` parser upstream for terminated-gRPC routing; and the outbound `ZenohFlowExporter` in `netring-exporters` (§11-iii). Only if terminated-h2 routing or netring-sourced telemetry is actually wanted; SNI passthrough (§7) already carries gRPC without either.

**Testing note.** The defects above are almost all invisible to the current suite because it exercises happy paths. The §10.1 regression net (item 0) is the place to add them; each of these should fail on today's code: large-upload-at-connect (A3), `nc -N` half-close (B2), mid-stream-backend-error hang (C1), `POST`-with-delayed-body multiroute (E1), `HEAD` (E3), and `Host: *` key-injection (F1).
