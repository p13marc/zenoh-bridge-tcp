# nlink-lab Network Topology Tests

End-to-end tests for zenoh-bridge-tcp using [nlink-lab](https://github.com/p13marc/nlink-lab) to create isolated network topologies with realistic WAN conditions.

## Prerequisites

- Linux with network namespace support
- `nlink-lab` installed and in PATH (or set `NLINK_LAB` env var)
- Rust toolchain (to build the bridge)

## Tests

### multi-hop-bridge.nll — Raw TCP Multi-Hop

Tests the full data path: `client -> import bridge -> zenoh -> export bridge -> backend`

```
[backend:8003] --- [site-a/exporter] ===WAN=== [site-b/importer:8002] --- [client]
```

- Echo server at backend, bridge export at site-a, bridge import at site-b
- WAN link with configurable delay and loss
- Tests: single connection, multiple sequential connections, latency bounds

```bash
./tests/nlink/run-multi-hop-test.sh
./tests/nlink/run-multi-hop-test.sh --wan-delay 100ms --wan-loss 1%
```

### multi-hop-http-bridge.nll — HTTP Host-Header Routing Multi-Hop

Tests HTTP routing through the bridge: single import listener routes to multiple backends by Host header.

```
[backend-api:8001] --\                       /-- [client]
                      [site-a] ===WAN=== [site-b/importer:8080]
[backend-web:8002] --/
```

- Two HTTP backends (api.example.com, web.example.com)
- Single import listener routes by Host header
- Tests: correct routing, unknown host (502), alternating hosts, case-insensitive routing

```bash
./tests/nlink/run-multi-hop-http-test.sh
./tests/nlink/run-multi-hop-http-test.sh --wan-delay 50ms
```

## Options

| Flag | Default | Description |
|------|---------|-------------|
| `--wan-delay` | `30ms` | One-way WAN link delay |
| `--wan-loss` | `0%` | WAN packet loss percentage |
| `--skip-build` | — | Skip `cargo build --release` |

## Debugging

```bash
# Deploy without running tests
nlink-lab deploy tests/nlink/multi-hop-bridge.nll

# Open a shell in a node
nlink-lab shell multi-hop-bridge client

# Check bridge logs
nlink-lab exec multi-hop-bridge exporter -- journalctl -u zenoh-bridge-tcp

# Inspect lab status
nlink-lab status multi-hop-bridge

# Tear down
nlink-lab destroy multi-hop-bridge
```

## Environment Variables

- `NLINK_LAB` — Path to nlink-lab binary (default: `nlink-lab` from PATH)
