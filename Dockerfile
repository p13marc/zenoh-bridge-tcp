# Multi-stage build for Zenoh TCP Bridge
FROM rust:1.97-slim as builder

# Install build dependencies
RUN apt-get update && apt-get install -y \
    pkg-config \
    libssl-dev \
    && rm -rf /var/lib/apt/lists/*

# Create app directory
WORKDIR /usr/src/zenoh-bridge-tcp

# Copy manifests
COPY Cargo.toml Cargo.lock ./

# Copy source code
COPY src ./src

# Build with tls-termination so the image can terminate TLS (a --listen with
# cert=/key=; HTTPS / terminated h2/gRPC). rustls uses the ring backend — no
# extra system deps.
RUN cargo build --release --features tls-termination

# Runtime stage
FROM debian:bookworm-slim

# Install runtime dependencies
RUN apt-get update && apt-get install -y \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# Create non-root user
RUN useradd -m -u 1000 -s /bin/bash zenoh

# Copy the binary from builder
COPY --from=builder /usr/src/zenoh-bridge-tcp/target/release/zenoh-bridge-tcp /usr/local/bin/

# Switch to non-root user
USER zenoh

# The bridge listens on whatever ports the --listen specs request, and
# optionally serves /healthz + /readyz + /metrics on
# --metrics-addr; publish those at run time (e.g. -p) rather than pinning one here.

# Deliberately no RUST_LOG: it takes precedence over --log-level, so baking it
# in here made that flag a silent no-op in every container. `info` is already
# the default; set RUST_LOG at run time when you want per-module filters.
ENV RUST_BACKTRACE=1

# No usable default run configuration exists (specs are deployment-specific), so
# the default command prints help; supply real --listen/--backend specs at run
# time, e.g. `docker run <image> --listen svc/0.0.0.0:8080`.
ENTRYPOINT ["zenoh-bridge-tcp"]
CMD ["--help"]
