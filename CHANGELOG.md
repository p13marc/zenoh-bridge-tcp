# Changelog

## [0.10.0] - 2026-08-31

A pre-release bug-fix sweep triggered by a field report: a browser on
`http://api.local:8080` got a 502 while `curl -H 'Host: api.local'` worked.
The root cause (routing keys kept non-default ports) and a deep audit of the
data planes produced one breaking change and a set of correctness fixes.

### BREAKING: routing keys are host-only

- **Any `:port` is stripped from the routing key**, not just 80/443. A browser
  always puts a non-default listener port into `Host` (`api.local:8080`), the
  backend registers the bare `@api.local`, and SNI structurally cannot carry a
  port — so the port-bearing key could never match and HTTP on a non-default
  port 502'd while HTTPS worked. Keys are now the bare host everywhere
  (Host, h2 `:authority`, SNI, `--backend @host`, `--http-export`).
- **`--backend 'svc@host:port/…'` is rejected** with an actionable error
  (silently stripping would merge `@a:8443` and `@a:9443` behind the
  operator's back). Drop the port from the spec.
- **IPv6 hosts are now routable**: brackets are stripped during normalization
  (`Host: [::1]:8080`, h2 `[::1]:8080`, and `--backend 'svc@[::1]/…'` or
  `@::1` all produce the key `::1`). Previously the import side minted
  bracket/portful keys the export side could not even register.
- **Client-supplied hostnames are validated** before touching Zenoh key
  expressions (alphanumerics, `-`, `_`, `.`, `:`): `Host: *` no longer steers
  a wildcard liveliness probe across the whole service namespace, and
  `Host: a/b` no longer injects key segments. Invalid hosts answer 400 (or
  close where no HTTP is expressible).
- **Duplicate `--backend` scopes are rejected**: two backends with the same
  (service, host) in one process share a Zenoh session, so the HA election
  could elect both (identical claims) and interleave responses. Run HA pairs
  as separate processes.

### Fixed

- **Chunked request bodies through `route=request` completed instead of
  504ing**: the chunked terminator (`0\r\n\r\n`, surfaced by flowscope as a
  Trailers event) was dropped, so the backend waited for the end of the body
  until the response idle timeout. Request trailers are now forwarded.
- **A backend response carrying `Connection: close` (or bare HTTP/1.0, or
  until-close framing) ends the `route=request` client connection** after the
  response. Previously the next request on that connection stalled 30s and
  got a 504 because the parser's response direction was closed for good.
- **Import drain actually drains**: on shutdown, connections that outlive
  `--drain-timeout` are now cancelled through their data plane's own token
  (EOF markers, undeclares, access logs all run), with a 1s grace before the
  abort fallback. Previously `abort_all()` killed only coordinator tasks and
  orphaned the relay halves, which kept moving bytes until the session closed.
- **Export drain no longer always times out**: the per-connection watchdog and
  the drain loop shared the same budget, so the outer timer always lost and
  then *detached* the task instead of aborting it.
- **Same-process HA election claims are unique** (per-claim UUID): previously
  two same-scope backends starting in the same millisecond both elected
  themselves.
- **The normal end of a connection no longer leaks a 60s error-signal
  publisher per connection** (per *request* in `route=request` mode): the
  holder now probes the client's liveliness token and releases immediately
  when it is already gone (the EOF-vs-undeclare race at every normal close).
- **TLS-terminating listeners answer 400/502 over the established session**
  (h1) instead of closing silently — a browser now sees a 502 page, not
  `ERR_CONNECTION_CLOSED`; h2 and passthrough still close (nothing else is
  expressible), now with a clean close_notify.
- **A TLS ClientHello without SNI falls back to the service default backend**
  (mirroring h2c) instead of being dropped — IP-addressed and legacy clients
  reach a catch-all backend when one exists.
- **`--metrics-addr` bind failure is fatal**, like a data-port bind failure.
  Previously the error was logged in a detached task and the process kept
  running "ready" with the health port connection-refused.
- **Multiroute metrics tell the truth**: a 504, a D2 overflow reset, and an
  oversized-response truncation are now recorded as failed/reset, not
  `completed`.
- **Pipelined-request bytes survive exchange boundaries** in `route=request`
  mode: a backpressured tail belonging to the next request was dropped on
  most exchange exits, poisoning a valid pipeline with a spurious 400.
- **Error responses are FIN'd, not RST'd**: sockets are shut down for writing
  after 400/502/504 writes, so unread request bytes no longer make the kernel
  destroy the queued response.
- **400 bodies name the actual reason** (malformed framing, unroutable host,
  timeout) instead of always claiming "Missing Host header".
- **Head readers keep the parser's refused tail** across reads (latent
  desync if `--max-header-size` is configured past the parser's refusal
  window); the metrics accept loop backs off on EMFILE like the data-plane
  loop; the D2 Stream-overflow warning fires once per connection instead of
  per sample; listeners log the actually-bound address (relevant for port 0).

### Known limitations

- ALPN on terminating listeners always offers `h2` first and cannot be
  restricted to `http/1.1`; an h2-negotiated connection relays h2 frames to
  the plaintext backend, which must speak h2c.
- h2 is a single-authority relay; browser connection coalescing across SANs
  routes second authorities to the first authority's backend (documented with
  mitigations in `docs/routing.md`).
- `--rx-channel-capacity` bounds samples, not bytes (see `docs/routing.md`).

## [0.9.1] - 2026-08-12

- Docker image build fix: `COPY benches` so the 0.9.0 image builds again (#13).

## [0.9.0] - 2026-08-11

A deep-audit release: correctness and lifecycle hardening across the export and
import data planes, a new HA capability, and a test suite that is now
deterministic under `--retries 0`.

### Added

- **Active/standby exporter election.** Two `--backend` bridges announcing the
  same service are now an HA pair: one is elected active (oldest claim wins),
  the others stand by and take over on its death. Previously both served every
  client, interleaving responses and double-executing requests. See
  `docs/routing.md`.
- **`wss://` backends actually work.** `tokio-tungstenite` is built with
  `rustls-tls-native-roots`; a `--backend 's/wss://host'` now validates against
  the system trust roots. Previously every `wss://` dial failed with "TLS
  support not compiled in", despite being CLI-accepted and documented.
- **A configurable per-attempt backend dial timeout** (`connect_timeout`, 10s):
  a blackholed backend no longer runs each dial to the OS SYN timeout.

### Fixed

- **The backend-unavailable signal is now recoverable.** It was a fire-once,
  uncached `session.put()` raced by its own trigger, so under interest-
  propagation skew (WAN, load) it was lost and the client hung forever. It is
  now a cached AdvancedPublisher held until the client token disappears, with a
  history-recovering subscriber. It also fires on EVERY export failure path
  (dial, Zenoh setup, mid-connection reset, backend read error), not just dial.
- **The export liveliness loop no longer stalls.** `handle_client_disconnect`
  held the connection-map mutex across the drain await (self-deadlock vs the
  task's own self-removal), so every abrupt disconnect froze the loop for
  `drain_timeout`; the dial ran inline, so one slow/blackholed backend deafened
  the whole exporter. Dials moved into the per-client task; the disconnect path
  cancels without waiting; duplicate client `Put`s are ignored.
- **Truncated streams are no longer reported as clean completions.** A backend
  (or client) read error used to publish the clean-EOF half-close marker, so
  the peer saw a well-formed FIN on a truncated body; it now resets.
- **Import door hardening.** Auto-detect re-peeks until the classifier decides
  (a short first segment no longer silently misroutes a TLS/HTTP client to the
  default backend); TLS-terminating and WebSocket handshakes are bounded
  (an idle client could pin a task+fd+permit forever); the h2c timeout-fallback
  probes for a backend before relaying (no-backend now closes fast); every
  connection's Zenoh setup phase is time-bounded (a stalled declare no longer
  hangs a task holding a connection-limit permit); the accept-error path backs
  off instead of spinning at 100% CPU under fd exhaustion.
- **Truthful readiness and orderly shutdown.** `/readyz` reports 200 only after
  every listener has bound (a bind failure now fails the process instead of
  running partially deaf with readiness green), answers 503 during the drain
  window instead of connection-refused, and the process exits through `main` so
  buffered file-log lines flush.
- **Logging robustness.** An empty-but-set `RUST_LOG` no longer silences the
  process; a malformed one warns and falls back; an unwritable `file=` sink is a
  clean startup error instead of a panic.
- `--read-timeout 0` and (0.8.1) `--max-response-size` overshoot fixes carried
  forward.

### Changed

- **The test suite is deterministic with zero retries.** Every test runs on a
  private Zenoh multicast scouting domain (`common::ScoutDomain`), removing the
  cross-test contention that made discovery slow and flaky; `.config/nextest.toml`
  now sets `retries = 0`. Added a three-bridge topology suite (fan-out, relay
  node, late joiner) and an HA suite, and replaced several vacuous tests (which
  printed a failure and returned success) with hard assertions.
- Dead `src/error.rs` removed; relay hot paths no longer copy each chunk.

## [0.8.1] - 2026-08-10

Bug-fix and consolidation release. No new features, no CLI additions.

### Fixed

- **Silent byte loss in `Stream` mode (the reason for this release).** Each
  connection publishes on a fresh `{service}/tx/{client_id}` key, but the export
  side only subscribes after it observes that connection's liveliness token — a
  few milliseconds later. Bytes relayed into that window survived only in the
  publisher cache (`--cache-size`, 256 *samples*), so a client that started
  writing immediately had everything older than the cache dropped: a 1 MiB burst
  reached the backend as ~435 KB, was relayed on as if whole, and the connection
  was logged `outcome="completed"`. The sample-miss listeners could not catch it,
  because samples published before a subscriber existed are never known to have
  been missed. The relay now waits (bounded, best-effort, `Stream` mode only) for
  a matching subscriber before forwarding, which closes the window. This broke
  the byte-exactness guarantee `Stream` exists to provide.

- **`route=request` killed healthy streaming responses after ~30s.** The
  response budget was computed once per exchange and used as an *absolute*
  deadline, so a large download, an SSE stream or a long poll died with
  "Response timeout" while bytes were actively arriving. It is now an idle
  budget, refreshed on every response sample.

- **The multiroute 502 contradicted itself.** That door deliberately keeps the
  connection alive so a client can retry a different Host, but answered with
  `Connection: close` — which every compliant client obeys, making the retry
  path unreachable outside of raw-socket tests. It now sends a keep-alive 502;
  the connection-scoped doors, which really do close, are unchanged.

- **`--read-timeout 0` was accepted.** Unlike every other tunable it had no
  floor, and zero made every head read time out instantly: the bridge started
  cleanly and then refused every connection. Now rejected at startup.

- **The `/healthz`, `/readyz`, `/metrics` server could be pinned open.** Its
  request read had no timeout and connections were spawned uncapped, so a peer
  that connected and said nothing held a task and a file descriptor
  indefinitely. It now honours `--read-timeout` and bounds concurrency, matching
  the hardening the data plane already had.

- **`--max-response-size` was enforced after the fact.** The cap was checked
  *after* writing each chunk, so it could be overshot by a whole chunk (measured:
  65,579 bytes written against a 4,096-byte cap), and trailers were counted but
  never checked. It is now checked before writing. Its documentation promised an
  HTTP 502 that the code cannot send — the response head is long gone by then —
  and now describes what actually happens: truncate and close.

### Changed

- The relay no longer copies every chunk it forwards. Both bridge directions did
  `.to_bytes().to_vec()` directly beneath comments claiming the path was
  zero-copy; the multiroute path already did it correctly.
- `route=request` no longer reallocates a `--buffer-size` buffer (64 KiB by
  default) on every read-loop iteration.

## [0.8.0] - 2026-08-05

### Added

- **A plain `--backend '<svc>/<target>'` is now the service's default
  (catch-all) backend.** It declares the `{service}/available` liveliness
  token, and every host-routed listener plane (plain HTTP/1 by Host, TLS
  passthrough by SNI, h2c by `:authority`, terminated TLS, `route=request`
  multiroute, WebSocket upgrades) falls back to it when no `@host` backend
  claims the hostname — `@host` backends keep precedence. Previously the
  simplest deployment (`--listen 'web/0.0.0.0:8002'` + `--backend
  'web/127.0.0.1:8003'` + `curl http://127.0.0.1:8002`) answered 502, because
  auto-detected HTTP routed by Host and a plain backend registered no
  availability at all.

- **Log sinks** (`--log-target`, repeatable): `stdout`, `stderr`,
  `file=PATH[,rotation=daily|hourly|minutely|never]`, `journald`, and
  `syslog[,ident=,facility=]`. Previously there was exactly one sink (stdout)
  and no layer composition to add another. journald and syslog are target-gated
  rather than feature-gated, so the released binary can use them.
- **Native journald fields.** Capturing stdout under systemd collapses every
  field into one opaque `MESSAGE=`; `--log-target journald` sends them natively,
  so `journalctl CLIENT_ID=… SERVICE=…` works.
- `--log-color auto|always|never`. `tracing-subscriber` does not tty-detect, so
  ANSI escapes were previously written unconditionally — including into
  redirected output.
- `--log-format` gained `full` (an alias for the existing `pretty` renderer) and
  `verbose` (multi-line, one field per line).
- **Per-connection access log**: one record per connection on close, on the
  `zenoh_bridge_tcp::access` target, carrying `outcome`, `bytes_up`,
  `bytes_down` and `duration_ms`, with identity inherited from the connection
  span. Connection close previously logged a bare `Connection closed` with
  nothing in it.
- `direction` (`up`/`down`) is a log field for the first time, matching the byte
  metrics. It was previously encoded only in English prose.
- The observability metrics surface (G7/#43 — `--metrics-addr`, `/healthz`,
  `/readyz`, `/metrics`) shipped without a changelog entry; recorded here.

### Changed

- **WebSocket upgrades with no announced backend now fail fast with a 502**
  instead of proceeding blindly onto the bus and hanging until a timeout. The
  previous blind fallback existed so plain backends could serve WS at all;
  the gated default-backend resolution replaces it on every plane.
- **Mixed-version note:** a pre-change plain *exporter* declares no
  `{service}/available` token, so a post-change importer will 502 host-keyed
  traffic (including WebSocket) against it. Upgrade exporters first; opaque
  (`proto=raw`) traffic is unaffected.
- **Log messages are static strings**, with all values in fields. This makes
  them stable aggregation keys, but breaks anything grepping for interpolated
  text such as `Client <id>:` — that identity is now the `client_id` field,
  inherited from the connection span. Errors moved from `{:?}` inside the
  message to an `error` field.
- **Noisy dependencies are damped by default.** When `RUST_LOG` is unset, zenoh,
  rustls and tungstenite are floored at `min(--log-level, warn)`, so
  `--log-level debug` no longer buries the bridge under zenoh routing internals.
  `RUST_LOG` still overrides everything.
- **Containers no longer pin `RUST_LOG=info`** (Dockerfile, docker-compose.yml).
  Because `RUST_LOG` takes precedence, that made `--log-level` a silent no-op in
  every shipped container.
- The `route=request` plane now records `zbridge_connections_outcome_total`,
  which it never did — the counter was silently always zero for that plane.
- Logging init moved after argument validation, so an invalid `--log-format` no
  longer installs a fallback subscriber before being rejected.
- `zenoh`/`zenoh-ext` bumped to 1.9.0; routine `cargo update`.

### Fixed

- `--log-level off` is now documented; it was already accepted.
- `tests/auto_import_integration.rs` piped bridge stdout/stderr without ever
  reading the pipes — at `--log-level debug` a bridge could fill the pipe buffer
  and block forever.

## [0.7.0] - 2026-08-04

### Breaking

- **The 9 routing flags are replaced by 2** (epic #81, design in
  `docs/routing.md`):
  `--listen '<service>/<addr>[,proto=raw][,cert=PATH,key=PATH][,route=request]'`
  and `--backend '<service>[@<host>]/<target>'`. Auto-detection is the default
  door; **cert presence implies TLS termination** (no cert = passthrough, zero
  key material on the bridge); a `ws://`/`wss://` target selects WebSocket.
  Migration:

  | 0.6.x | 0.7.0 |
  |---|---|
  | `--import s/a` | `--listen s/a,proto=raw` |
  | `--http-import s/a` · `--auto-import s/a` | `--listen s/a` |
  | `--ws-import s/a` | `--listen s/a` (upgrade auto-detected; the forced-upgrade semantics of `--ws-import` are gone) |
  | `--http-multiroute-import s/a` | `--listen s/a,route=request` |
  | `--https-terminate s/a --tls-cert C --tls-key K` | `--listen s/a,cert=C,key=K` (per-listener certs) |
  | `--export s/b` | `--backend s/b` |
  | `--http-export s/d/b` | `--backend s@d/b` |
  | `--ws-export s/ws://u` | `--backend s/ws://u` |

- **Zenoh session flags renamed**: `-m/--mode` → `--zenoh-mode`,
  `-e/--connect` → `--zenoh-connect`, `-l/--listen` → `--zenoh-listen`,
  `-c/--config` → `--zenoh-config`. Short flags are dropped (`-l` must not
  silently change meaning between 0.6 and 0.7).
- A default (non-`tls-termination`) build now rejects `cert=`/`key=` at spec
  validation with an error naming the missing feature, instead of a clap
  unknown-argument error for `--https-terminate`.

### Added

- **Plaintext HTTP/2 (h2c) / prior-knowledge gRPC routing** (#74): the
  auto-detect door routes an h2 client preface by the first stream's
  `:authority` and relays the multiplexed streams opaquely (single-authority
  relay), with the gRPC-status response tap attached — completing
  h1/h2/gRPC support in every deployment shape, plaintext included. A client
  that waits for the server's SETTINGS (RFC 9113 §3.4) falls back to an
  opaque un-keyed relay on head-read timeout instead of being dropped.
  `zbridge_grpc_status_total` now works in the default build.
- `--backend 'svc@host/ws://…'` (#75): WebSocket backends register for
  hostname routing (`{service}/{host}/available`), and the auto-detect door
  routes WS upgrades by their Host — one listener, many WS backends. The
  removed `--ws-export` could not express this.
- ClientHello **ALPN logging** on SNI passthrough (#80): `alpn=` +
  `proto_guess=` on the routing log line, without decrypting anything.
- Terminated-`wss://` end-to-end test coverage (#71).

### Fixed

- **`route=request` no longer busy-spins on a plaintext-h2 client** (PR #94):
  an h2 preface where a request line was expected tunnelled the parser and
  the re-offer loop pinned a tokio worker thread at 100% CPU, unabortable.
  The connection now closes promptly and the listener stays healthy.
- **Multiroute protocol switches keep the coalesced first bytes** (PR #94 +
  flowscope 0.24.1): a backend flushing the `101` together with the first
  WebSocket frame lost that frame inside the parser; the tunnel residue is
  now spliced to the client before the opaque relay.
- Host-routed WebSocket routing no longer folds a Zenoh liveliness *error*
  into its bare-key fallback (silent wrong-backend risk); only a genuinely
  un-announced host falls back, and it is logged (PR #94).
- `--backend 'svc@*/…'` (or any Zenoh keyexpr metacharacter in a host) is
  rejected at validation — a `*` host would have registered a live wildcard
  capturing the whole service's routing (PR #94).
- flowscope's internal 64 KiB head cap no longer silently overrides a larger
  `--max-header-size`; the parsers are built from the configured cap (PR #94).
- Spec options with empty values (`cert=`), duplicates, and trailing commas
  are rejected at parse time; a displayed `ListenSpec` now round-trips
  through its own parser (PR #94).
- **WS upgrade detection survives a head split across TCP segments** (#77
  part 1): the single 4096-byte peek misrouted split handshakes to the
  plain-HTTP path (clients got 502 instead of 101); detection now re-peeks
  with a growing, deadline-bounded window.
- Head-read timeouts now report how many bytes were consumed (#76).

### Changed

- **One accept loop** (#73): the five per-mode listener loops collapsed into
  `run_accept_loop` + per-connection handlers. Log lines converge to one
  `New connection` / `Import bridge …` family with a `mode=` field.
- **One dns-key convention** (#75): both sides thread the bare hostname and
  the `{service}/{dns}` segment is joined in exactly one place; on-wire keys
  are unchanged.
- One generic head reader (#76) behind the SNI/HTTP/h2 head parsers.
- flowscope `http2` is a base dependency (#74); the `tls-termination` feature
  now gates only the rustls stack.
- `docs/HTTP_ROUTING_GUIDE.md` removed — superseded by the README's routing
  sections and `docs/routing.md`.

## [0.5.0] - 2026-04-08

### Added

- **Deep audit and 16 bug fixes**: Comprehensive code audit identified and resolved 16 bugs across HTTP parsing, TLS handling, shutdown correctness, and CLI validation
- **Strict SNI validation**: TLS ClientHello SNI parsing now enforces RFC 6066 and RFC 1035 hostname rules (253-byte limit, 63-byte labels, no leading/trailing hyphens, ASCII-only, no trailing dots)
- **CLI argument validation**: Early validation of `--buffer-size` (minimum 1024), `--drain-timeout` (minimum 1s), `--log-format`, `--log-level`, and all spec formats before starting the bridge
- **Import task tracking**: Import listener uses `JoinSet` for per-connection task tracking with graceful drain on shutdown
- **29 new tests**: Coverage integration tests (large messages up to 200KB, partial transfers, concurrent clients, rapid connect/disconnect cycles) and 16 bug fix verification tests
- **Bug demonstration test suite**: `tests/bug_demonstrations.rs` with 16 tests verifying each audit fix

### Fixed

- HTTP response smuggling: reject requests with both Transfer-Encoding and Content-Length headers
- Duplicate Content-Length header validation
- Content-Length bounds checking (max 1GB)
- Chunked transfer encoding overflow safety
- Empty client ID rejection
- Export reconnect ordering: cancel old connection before spawning new one
- Explicit cancellation signal send instead of relying on drop
- Mutex release before await in export shutdown path
- Multiroute 504 response guard for unavailable backends
- TLS handshake size consistency validation (max 16KB)
- Main process drain timeout now uses configured value instead of hardcoded default

### Changed

- **Zenoh upgraded to 1.8.0** (from 1.6.2)
- Export shutdown drains task handle map, releases mutex, sends cancellation signals, then awaits with drain timeout
- Import shutdown stops accept loop on cancellation, then drains active connections up to drain timeout

## [0.4.0] - 2026-02-22

### Changed

- **Transport trait abstraction**: Introduced `TransportReader` and `TransportWriter` traits in `src/transport.rs`, replacing duplicated TCP/WebSocket bridging logic with generic `bridge_import_connection<R, W>()` and `handle_client_bridge<R, W>()`
- **Unified export liveliness loop**: Consolidated TCP and WebSocket export modes into a single `run_export_loop()` that dispatches based on `ExportBackend` enum
- **Module directory split**: Split monolithic `export.rs` (1005 lines) and `import.rs` (1387 lines) into focused submodule directories (`src/export/` and `src/import/`) with no public API changes

### Fixed

- WebSocket export now sends error signal to import side on backend connection failure (was silently dropping)
- Integration tests use dynamic ports and unique service names to prevent cross-test interference
- WebSocket integration tests resilient to stale processes and race conditions

## [0.3.0] - 2026-02-21

### Added

- **Graceful shutdown**: CancellationToken-based shutdown propagation across all tasks
- **Backend reconnection**: Exponential backoff retry when backend connections fail
- **TLS termination**: Optional TLS termination on import side (`--https-terminate`, `--tls-cert`, `--tls-key`), feature-gated behind `tls-termination`
- **Protocol auto-detection**: `--auto-import` mode that detects TLS, HTTP, WebSocket, or raw TCP and dispatches accordingly
- **Connection draining**: Configurable drain timeout (`--drain-timeout`) allows in-flight data to flush before connections close
- **Bidirectional HTTP mode**: `--http-multiroute-import` for per-request HTTP/1.1 routing with keep-alive support
- HTTP 504 response helper for backend timeout/unavailability
- WebSocket upgrade detection in HTTP parser
- HTTP response body framing parser for chunked/content-length/close-delimited responses
- Shared test utilities module with `PortGuard`, `wait_for_port`, `BridgeProcess`, and `unique_service_name`
- Explicit Zenoh resource undeclaration on task exit

### Fixed

- HTTP body truncation: parser now returns full buffer including body bytes (BUG-1)
- Connection ID collisions: use UUID v4 for globally unique client IDs (BUG-4)
- Buffer size config not threaded through export/import call chains (BUG-2)
- Hardcoded Content-Length in `http_400_response` (BUG-3)
- Race condition on fast publishers: increased AdvancedPublisher cache from 10 to 64 (BUG-5)
- Export mode missed pre-existing clients on startup (BUG-6)
- WebSocket export had no error signal on backend failure (BUG-7)
- `normalize_dns` port stripping used incorrect parsing (BUG-8)
- Removed emoji from log messages for cleaner structured logging

### Changed

- Test infrastructure overhauled: dynamic ports, unique service names, meaningful assertions, wait-based synchronization instead of sleeps

## [0.2.0]

Initial release with export/import modes, HTTP/HTTPS routing, and WebSocket support.
