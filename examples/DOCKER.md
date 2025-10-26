# Docker Deployment Guide

This guide explains how to build and run the Zenoh TCP Bridge using Docker and Docker Compose.

## Table of Contents

- [Quick Start](#quick-start)
- [Building the Image](#building-the-image)
- [Running with Docker](#running-with-docker)
- [Running with Docker Compose](#running-with-docker-compose)
- [Configuration Profiles](#configuration-profiles)
- [Networking](#networking)
- [Examples](#examples)
- [Troubleshooting](#troubleshooting)

## Quick Start

The fastest way to get started:

```bash
# Build and run with default configuration
docker-compose up -d bridge

# Test the connection
echo "Hello Zenoh!" | nc localhost 7447
```

## Building the Image

### Build the Docker image manually:

```bash
docker build -t zenoh-bridge-tcp:latest .
```

### Build with Docker Compose:

```bash
docker-compose build
```

The build process uses a multi-stage Dockerfile to minimize the final image size.

## Running with Docker

### Basic Usage

Run a single bridge instance:

```bash
docker run -d \
  --name zenoh-bridge-tcp \
  -p 7447:7447 \
  zenoh-bridge-tcp:latest
```

### Custom Configuration

Override the default configuration with command-line arguments:

```bash
docker run -d \
  --name zenoh-bridge-tcp \
  -p 8080:8080 \
  zenoh-bridge-tcp:latest \
    --tcp-listen 0.0.0.0:8080 \
    --pub-key custom/topic/in \
    --sub-key custom/topic/out \
    --mode peer
```

### Enable Debug Logging

```bash
docker run -d \
  --name zenoh-bridge-tcp \
  -p 7447:7447 \
  -e RUST_LOG=debug \
  zenoh-bridge-tcp:latest
```

### Interactive Mode (Foreground)

Run in foreground to see logs directly:

```bash
docker run --rm \
  --name zenoh-bridge-tcp \
  -p 7447:7447 \
  zenoh-bridge-tcp:latest
```

## Running with Docker Compose

Docker Compose makes it easy to run complex configurations.

### Single Bridge

Start a single bridge instance:

```bash
docker-compose up -d bridge
```

View logs:

```bash
docker-compose logs -f bridge
```

Stop the bridge:

```bash
docker-compose down
```

### Multi-Bridge Setup

Run two bridges that can communicate through Zenoh:

```bash
docker-compose --profile multi-bridge up -d
```

This starts:
- **bridge-a** on port 8001 (publishes to `service/a/tx`, subscribes to `service/b/tx`)
- **bridge-b** on port 8002 (publishes to `service/b/tx`, subscribes to `service/a/tx`)

Test the connection:

```bash
# Terminal 1: Connect to bridge-a
nc localhost 8001

# Terminal 2: Connect to bridge-b
nc localhost 8002

# Messages typed in Terminal 1 will appear in Terminal 2 and vice versa!
```

### With Zenoh Router

Run bridges with a dedicated Zenoh router:

```bash
docker-compose --profile with-router up -d
```

This starts:
- **zenoh-router**: Central Zenoh router
- **bridge-client**: TCP bridge in client mode, connected to the router

## Configuration Profiles

Docker Compose supports multiple profiles for different scenarios:

| Profile | Description | Command |
|---------|-------------|---------|
| (default) | Single bridge | `docker-compose up -d bridge` |
| `multi-bridge` | Two bridges for bidirectional test | `docker-compose --profile multi-bridge up -d` |
| `with-router` | Bridge + Zenoh router | `docker-compose --profile with-router up -d` |

## Networking

### Bridge Network

By default, all containers use a Docker bridge network named `zenoh-net`. This allows them to communicate with each other using container names as hostnames.

### Port Mapping

The default port mappings are:

- `7447:7447` - Single bridge
- `8001:8001` - Bridge A (multi-bridge profile)
- `8002:8002` - Bridge B (multi-bridge profile)
- `9000:9000` - Client bridge (with-router profile)

### Custom Network

To use a custom network:

```yaml
networks:
  zenoh-net:
    external: true
    name: my-custom-network
```

## Examples

### Example 1: Basic Test

```bash
# Start the bridge
docker-compose up -d bridge

# Connect and send data
echo "Test message" | nc localhost 7447

# View logs to see the data was received
docker-compose logs bridge
```

### Example 2: Python Client with Docker Bridge

```bash
# Start the bridge
docker-compose up -d bridge

# Run Python client
python3 test_client.py localhost 7447
```

### Example 3: Bridge Communication Test

```bash
# Start multi-bridge setup
docker-compose --profile multi-bridge up -d

# In one terminal, connect to bridge A
nc localhost 8001

# In another terminal, connect to bridge B
nc localhost 8002

# Type messages - they'll go through Zenoh between bridges!
```

### Example 4: Monitor Zenoh Traffic

If you have Zenoh CLI tools installed on the host:

```bash
# Start the bridge
docker-compose up -d bridge

# Subscribe to all topics
zenoh subscribe -k '**'

# In another terminal, send data via TCP
echo "Monitoring test" | nc localhost 7447

# You should see the data appear in the zenoh subscriber
```

### Example 5: Custom Configuration with Environment Variables

Create a `.env` file:

```env
RUST_LOG=debug
TCP_PORT=9090
PUB_KEY=my/custom/pub
SUB_KEY=my/custom/sub
```

Modify `docker-compose.yml` to use these variables:

```yaml
services:
  bridge:
    build: .
    ports:
      - "${TCP_PORT:-7447}:${TCP_PORT:-7447}"
    environment:
      - RUST_LOG=${RUST_LOG:-info}
    command:
      - --tcp-listen
      - 0.0.0.0:${TCP_PORT:-7447}
      - --pub-key
      - ${PUB_KEY:-tcp/data/tx}
      - --sub-key
      - ${SUB_KEY:-tcp/data/rx}
```

### Example 6: Production Deployment with Restart Policies

For production, ensure containers restart automatically:

```bash
docker run -d \
  --name zenoh-bridge-tcp \
  --restart unless-stopped \
  -p 7447:7447 \
  -e RUST_LOG=info \
  zenoh-bridge-tcp:latest
```

Or with Docker Compose (already configured):

```yaml
restart: unless-stopped
```

## Troubleshooting

### Container won't start

Check logs:

```bash
docker-compose logs bridge
```

Or for a specific container:

```bash
docker logs zenoh-bridge-tcp
```

### Port already in use

```bash
# Check what's using the port
sudo netstat -tlnp | grep 7447

# Or use a different port
docker run -d -p 8888:7447 zenoh-bridge-tcp:latest
```

### Can't connect to bridge

1. Verify the container is running:
   ```bash
   docker ps | grep zenoh-bridge-tcp
   ```

2. Check if the port is exposed:
   ```bash
   docker port zenoh-bridge-tcp
   ```

3. Test from inside the container:
   ```bash
   docker exec -it zenoh-bridge-tcp sh
   # Inside container:
   # nc localhost 7447
   ```

### Bridges can't communicate

1. Ensure they're on the same Docker network:
   ```bash
   docker network inspect zenoh-net
   ```

2. Check if containers can ping each other:
   ```bash
   docker exec bridge-a ping bridge-b
   ```

3. Verify Zenoh keys are correctly configured (pub/sub keys should match appropriately)

### High CPU usage

1. Check log level (debug logging can be intensive):
   ```bash
   docker exec zenoh-bridge-tcp printenv RUST_LOG
   ```

2. Monitor resource usage:
   ```bash
   docker stats zenoh-bridge-tcp
   ```

### Memory issues

Set memory limits in docker-compose.yml:

```yaml
services:
  bridge:
    build: .
    deploy:
      resources:
        limits:
          memory: 512M
        reservations:
          memory: 256M
```

## Health Checks

Add a health check to your deployment:

```yaml
services:
  bridge:
    build: .
    healthcheck:
      test: ["CMD", "sh", "-c", "nc -z localhost 7447 || exit 1"]
      interval: 30s
      timeout: 10s
      retries: 3
      start_period: 10s
```

## Volume Mounts

If you need to persist logs or configuration:

```yaml
services:
  bridge:
    build: .
    volumes:
      - ./logs:/var/log/zenoh-bridge
      - ./config:/etc/zenoh-bridge
```

## Security Best Practices

1. **Run as non-root user** (already configured in Dockerfile)
2. **Use read-only root filesystem** (if possible):
   ```yaml
   security_opt:
     - no-new-privileges:true
   read_only: true
   ```

3. **Limit capabilities**:
   ```yaml
   cap_drop:
     - ALL
   cap_add:
     - NET_BIND_SERVICE  # Only if binding to ports < 1024
   ```

4. **Use secrets for sensitive data**:
   ```yaml
   secrets:
     - zenoh_config
   ```

## Performance Tuning

### Increase buffer sizes

Modify the source code buffer size if needed, rebuild:

```rust
let mut buffer = vec![0u8; 8192];  // Increased from 4096
```

### Network optimization

For high-throughput scenarios:

```yaml
services:
  bridge:
    build: .
    sysctls:
      - net.core.rmem_max=26214400
      - net.core.wmem_max=26214400
```

## Monitoring

### Prometheus metrics (future enhancement)

To add Prometheus metrics, you could extend the bridge with a metrics endpoint:

```bash
docker run -d \
  -p 7447:7447 \
  -p 9090:9090 \
  zenoh-bridge-tcp:latest
```

### Log aggregation

Use Docker's logging drivers:

```yaml
services:
  bridge:
    logging:
      driver: "json-file"
      options:
        max-size: "10m"
        max-file: "3"
```

Or forward to a logging service:

```yaml
logging:
  driver: "syslog"
  options:
    syslog-address: "tcp://logserver:514"
```

## Additional Resources

- [Docker Documentation](https://docs.docker.com/)
- [Docker Compose Documentation](https://docs.docker.com/compose/)
- [Zenoh Documentation](https://zenoh.io/docs/)
- [Main README](../README.md)
- [Testing Guide](../test_with_zenoh.sh)

## Support

For issues specific to Docker deployment, check:
1. Container logs: `docker-compose logs`
2. Container status: `docker-compose ps`
3. Network status: `docker network inspect zenoh-net`
4. Resource usage: `docker stats`
