# Development

## Building

Rust 1.97+ (edition 2024; pinned as `rust-version`, enforced by the CI msrv
job).

```bash
cargo build --release                             # default build
cargo build --release --features tls-termination  # adds TLS-terminating listeners (cert=/key=)
```

The `tls-termination` feature gates rustls and the terminating listener path.
A default build rejects `cert=`/`key=` specs at startup with a clear error;
everything else — including h2c `:authority` routing — is in the base build.
When adding terminated-TLS code, gate it (helpers *and* tests) with
`#[cfg(feature = "tls-termination")]`, or the default build breaks.

L7 parsing (protocol classification, SNI extraction, HTTP/1.1 framing, h2
header peeking) is delegated to the [flowscope](https://crates.io/crates/flowscope)
crate — pure compute, no async, no capabilities. Do not hand-roll parsers.

## Testing

Run tests with **nextest** — `.config/nextest.toml` carries a serial override
for `http_edge_cases` and a `retries = 2` default profile (the Zenoh+socket
integration suites are timing-sensitive under load); plain `cargo test` loses
that isolation.

```bash
cargo nextest run                                  # default features
cargo nextest run --features tls-termination       # + termination suites
cargo test --lib                                   # unit tests only
cargo nextest run --test http_routing_integration  # one suite
```

Suite conventions worth knowing:

- Integration tests live in `tests/*_integration.rs`, one file per concern
  (routing, liveliness, backpressure, drain, metrics, termination, …);
  `tests/README.md` documents them in detail.
- After a client connects to a listener, tests sleep ~2s before sending data:
  liveliness has to propagate (listener declares token → backend sees it →
  backend connects) before bytes can flow.
- Background test servers must never `.unwrap()` their `.serve().await` —
  a panic during runtime teardown SIGABRTs the whole test binary.

### Network topology tests (nlink-lab)

End-to-end tests in isolated network namespaces with simulated WAN conditions.
Require Linux with netns support and
[nlink-lab](https://github.com/p13marc/nlink-lab) on the host (not in
containers):

```bash
./tests/nlink/run-multi-hop-test.sh                            # raw TCP multi-hop
./tests/nlink/run-multi-hop-http-test.sh                       # HTTP host routing
./tests/nlink/run-multi-hop-test.sh --wan-delay 100ms --wan-loss 1%
```

See `tests/nlink/README.md` for topology details and debugging.

## Quality gates

CI (`.forgejo/workflows/ci.yml`) runs: `fmt`, `clippy --all-features`, nextest
(all features + doc tests), a **default-feature build** (clippy + nextest — it
exercises the feature gating), msrv (1.97), `cargo deny`, `cargo machete`, and
docs. Locally:

```bash
cargo fmt
cargo clippy --all-features
cargo deny check
```

Releases (`release.yml`) publish a binary tarball with SHA256SUMS and a
container image. Note: the released binary is built with default features —
TLS termination is available in the container image or a source build.
