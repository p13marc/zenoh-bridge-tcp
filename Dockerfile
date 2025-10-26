# Multi-stage build for Zenoh TCP Bridge
FROM rust:1.75-slim as builder

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

# Build the application
RUN cargo build --release

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

# Expose default TCP port
EXPOSE 7447

# Set environment variables
ENV RUST_LOG=info
ENV RUST_BACKTRACE=1

# Default command
ENTRYPOINT ["zenoh-bridge-tcp"]
CMD ["--tcp-listen", "0.0.0.0:7447", "--pub-key", "tcp/data/tx", "--sub-key", "tcp/data/rx", "--mode", "peer"]
