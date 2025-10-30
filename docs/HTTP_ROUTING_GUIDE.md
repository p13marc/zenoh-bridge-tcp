# HTTP/HTTPS Routing Guide

Complete guide to using DNS-based routing for HTTP and HTTPS services with the Zenoh TCP Bridge.

## Table of Contents

- [Overview](#overview)
- [Quick Start](#quick-start)
- [How It Works](#how-it-works)
- [HTTP Routing](#http-routing)
- [HTTPS Routing](#https-routing)
- [DNS Normalization](#dns-normalization)
- [Configuration Examples](#configuration-examples)
- [Best Practices](#best-practices)
- [Troubleshooting](#troubleshooting)
- [Advanced Topics](#advanced-topics)

## Overview

The HTTP/HTTPS routing feature enables DNS-based service discovery and routing, allowing a single TCP listener to route traffic to multiple backends based on the hostname in HTTP requests or TLS connections.

### Key Features

- **DNS-Based Routing**: Route traffic by hostname to different backends
- **Protocol Detection**: Automatically detects HTTP vs HTTPS
- **No TLS Termination**: HTTPS traffic passes through encrypted
- **DNS Normalization**: Case-insensitive, port-aware routing
- **Dynamic Backends**: Add/remove backends without restarting
- **Service Discovery**: Automatic backend availability detection

### Use Cases

- Multi-tenant SaaS applications
- API gateways with domain-based routing
- Development/staging environment separation
- Virtual host routing
- Hybrid cloud deployments
- Microservices routing by domain

## Quick Start

### Basic HTTP Routing

```bash
# Terminal 1: Start HTTP backend
python3 -m http.server 8001

# Terminal 2: Export backend for api.example.com
zenoh-bridge-tcp --http-export 'http-service/api.example.com/127.0.0.1:8001'

# Terminal 3: Import (single listener)
zenoh-bridge-tcp --http-import 'http-service/0.0.0.0:8080'

# Terminal 4: Test
curl -H "Host: api.example.com" http://127.0.0.1:8080/
```

### Basic HTTPS Routing

```bash
# Terminal 1: Start HTTPS backend with certificate for api.example.com
# (Use your HTTPS server here)

# Terminal 2: Export HTTPS backend
zenoh-bridge-tcp --http-export 'https-service/api.example.com/127.0.0.1:8443'

# Terminal 3: Import
zenoh-bridge-tcp --http-import 'https-service/0.0.0.0:443'

# Terminal 4: Test
curl https://api.example.com/ --resolve api.example.com:443:127.0.0.1 --insecure
```

## How It Works

### Architecture

```
┌─────────────┐
│ HTTP Client │
│             │
│ Host: api...│
└──────┬──────┘
       │ HTTP Request
       ↓
┌──────────────────────────┐
│   Import Bridge          │
│   (Listener: :8080)      │
│                          │
│  1. Parse Host header    │
│  2. Normalize DNS        │
│  3. Check availability   │
└──────────┬───────────────┘
           │ Zenoh
           │ Key: service/api.example.com/tx/client_1
           ↓
┌──────────────────────────┐
│   Export Bridge          │
│   (DNS: api.example.com) │
│                          │
│  1. Monitor liveliness   │
│  2. Connect to backend   │
└──────────┬───────────────┘
           │
           ↓
┌──────────────────────────┐
│   Backend Server         │
│   (192.168.1.10:8000)    │
└──────────────────────────┘
```

### Routing Flow

1. **Client connects** to import bridge listener
2. **Protocol detection**: Bridge peeks at first bytes
   - Starts with `GET`, `POST`, etc. → HTTP
   - Starts with `0x16` (TLS Handshake) → HTTPS
3. **DNS extraction**:
   - HTTP: Parse `Host` header
   - HTTPS: Extract SNI from TLS ClientHello
4. **DNS normalization**: Convert to lowercase, strip default ports
5. **Backend discovery**: Query Zenoh for available backends
6. **Routing**: Forward via DNS-specific Zenoh keys
7. **Data forwarding**: Bidirectional streaming between client and backend

### Zenoh Key Structure

```
Traditional TCP Mode:
  {service}/tx/{client_id}
  {service}/rx/{client_id}
  {service}/clients/{client_id}

HTTP/HTTPS Mode:
  {service}/{dns}/tx/{client_id}
  {service}/{dns}/rx/{client_id}
  {service}/{dns}/clients/{client_id}
  {service}/{dns}/available           # Backend availability signal

Example:
  http-service/api.example.com/tx/client_1
  http-service/api.example.com/rx/client_1
  http-service/api.example.com/clients/client_1
  http-service/api.example.com/available
```

## HTTP Routing

### Host Header Parsing

The bridge parses the HTTP `Host` header to determine routing:

```http
GET /api/users HTTP/1.1
Host: api.example.com
User-Agent: curl/7.68.0
Accept: */*
```

From this request, the bridge extracts `api.example.com` and routes to the backend registered for that DNS name.

### HTTP/1.0 Support

The bridge also supports HTTP/1.0 absolute URIs:

```http
GET http://api.example.com/api/users HTTP/1.0
```

### Export Specification

```bash
--http-export 'service_name/dns/backend_addr'
```

**Format breakdown**:
- `service_name`: Logical service group (e.g., "http-service")
- `dns`: DNS name for this backend (e.g., "api.example.com")
- `backend_addr`: Backend address and port (e.g., "192.168.1.10:8000")

**Example**:
```bash
zenoh-bridge-tcp --http-export 'http-service/api.example.com/127.0.0.1:8000'
```

### Import Specification

```bash
--http-import 'service_name/listen_addr'
```

**Format breakdown**:
- `service_name`: Same service group as exports
- `listen_addr`: Address and port to listen on (e.g., "0.0.0.0:8080")

**Example**:
```bash
zenoh-bridge-tcp --http-import 'http-service/0.0.0.0:8080'
```

### Error Responses

**Missing Host Header** (HTTP 400):
```http
HTTP/1.1 400 Bad Request
Content-Type: text/plain
Content-Length: 37
Connection: close

400 Bad Request: Missing Host header
```

**Backend Unavailable** (HTTP 502):
```http
HTTP/1.1 502 Bad Gateway
Content-Type: text/plain
Content-Length: 54
Connection: close

502 Bad Gateway: No backend available for example.com
```

## HTTPS Routing

### SNI Extraction

HTTPS routing uses Server Name Indication (SNI) from the TLS ClientHello:

```
TLS ClientHello Structure:
┌─────────────────────────────┐
│ Record Header (5 bytes)     │
│  - Type: 0x16 (Handshake)   │
│  - Version: TLS 1.x         │
│  - Length                    │
├─────────────────────────────┤
│ Handshake Message           │
│  - Type: 0x01 (ClientHello) │
│  - Client Version           │
│  - Random                    │
│  - Session ID               │
│  - Cipher Suites            │
│  - Compression Methods      │
│  - Extensions               │
│    ├─ SNI Extension (0x0000)│
│    │  └─ Hostname           │ ← Extracted here
│    └─ Other Extensions      │
└─────────────────────────────┘
```

### End-to-End Encryption

**Important**: The bridge NEVER decrypts HTTPS traffic. It only reads the plaintext TLS ClientHello to extract the SNI, then forwards all data (including the ClientHello) unchanged to the backend.

```
Client ←[TLS encrypted]→ Bridge ←[TLS encrypted]→ Backend
                         ↑
                         Only reads SNI from ClientHello
                         (which is plaintext by design)
```

### HTTPS Export

Same format as HTTP export:

```bash
zenoh-bridge-tcp --http-export 'https-service/api.example.com/192.168.1.10:8443'
```

**Note**: The backend must have a valid TLS certificate for the DNS name.

### HTTPS Import

Same format as HTTP import:

```bash
zenoh-bridge-tcp --http-import 'https-service/0.0.0.0:443'
```

The bridge automatically detects HTTPS connections and extracts SNI.

### Error Handling

**Missing SNI Extension**:
- Connection is closed immediately
- No error message can be sent (TLS handshake hasn't completed)
- Client sees "Connection reset" or similar

**Backend Unavailable**:
- Connection is closed immediately
- Cannot send HTTP error response (not HTTP yet)
- Client sees connection failure

### Testing HTTPS Locally

When testing HTTPS with local servers, you need to resolve DNS to localhost:

```bash
# Using curl with --resolve
curl https://api.example.com/ \
  --resolve api.example.com:443:127.0.0.1 \
  --insecure

# Using /etc/hosts (add this line)
127.0.0.1  api.example.com

# Then just:
curl https://api.example.com/ --insecure
```

**Note**: `--insecure` is needed for self-signed certificates in testing.

## DNS Normalization

DNS names are normalized for consistent routing:

### Case Normalization

DNS is case-insensitive, so the bridge converts all DNS names to lowercase:

```
Input:                    Normalized:
API.EXAMPLE.COM      →    api.example.com
Api.Example.Com      →    api.example.com
api.example.com      →    api.example.com
```

### Port Normalization

Default ports are stripped for cleaner routing:

```
Input:                    Normalized:
example.com:80       →    example.com        (HTTP default)
example.com:443      →    example.com        (HTTPS default)
example.com:8080     →    example.com:8080   (custom port kept)
example.com:8443     →    example.com:8443   (custom port kept)
```

### Combined Normalization

Both transformations are applied:

```
Input:                    Normalized:
API.EXAMPLE.COM:80   →    api.example.com
Example.Com:443      →    example.com
API.EXAMPLE.COM:8080 →    api.example.com:8080
```

### Why Normalization Matters

Without normalization, these would be treated as different backends:
- `api.example.com`
- `API.EXAMPLE.COM`
- `api.example.com:80`

With normalization, they all route to the same backend.

## Configuration Examples

### Example 1: Simple Single Backend

```bash
# Backend
zenoh-bridge-tcp --http-export 'web/example.com/127.0.0.1:8000'

# Frontend
zenoh-bridge-tcp --http-import 'web/0.0.0.0:8080'

# Test
curl -H "Host: example.com" http://localhost:8080/
```

### Example 2: Multiple Backends, One Listener

```bash
# API backend
zenoh-bridge-tcp --http-export 'service/api.example.com/192.168.1.10:8000'

# Web backend
zenoh-bridge-tcp --http-export 'service/web.example.com/192.168.1.20:8000'

# Admin backend
zenoh-bridge-tcp --http-export 'service/admin.example.com/192.168.1.30:8000'

# Single listener routes to all
zenoh-bridge-tcp --http-import 'service/0.0.0.0:8080'

# Test
curl -H "Host: api.example.com" http://localhost:8080/
curl -H "Host: web.example.com" http://localhost:8080/
curl -H "Host: admin.example.com" http://localhost:8080/
```

### Example 3: Multi-Tenant SaaS

```bash
# Tenant A backend
zenoh-bridge-tcp --http-export 'saas/tenant-a.myapp.com/192.168.1.100:8000'

# Tenant B backend
zenoh-bridge-tcp --http-export 'saas/tenant-b.myapp.com/192.168.1.101:8000'

# Tenant C backend
zenoh-bridge-tcp --http-export 'saas/tenant-c.myapp.com/192.168.1.102:8000'

# Single public endpoint
zenoh-bridge-tcp --http-import 'saas/0.0.0.0:443'

# Each tenant automatically routed to their backend
# tenant-a.myapp.com → 192.168.1.100
# tenant-b.myapp.com → 192.168.1.101
# tenant-c.myapp.com → 192.168.1.102
```

### Example 4: Development vs Production

```bash
# Production backend
zenoh-bridge-tcp --http-export 'app/api.example.com/192.168.1.10:8000'

# Development backend
zenoh-bridge-tcp --http-export 'app/api-dev.example.com/192.168.1.20:8000'

# Staging backend
zenoh-bridge-tcp --http-export 'app/api-staging.example.com/192.168.1.30:8000'

# Single gateway
zenoh-bridge-tcp --http-import 'app/0.0.0.0:8080'

# Route to different environments by hostname
curl -H "Host: api.example.com" http://localhost:8080/        # → Production
curl -H "Host: api-dev.example.com" http://localhost:8080/    # → Development
curl -H "Host: api-staging.example.com" http://localhost:8080/ # → Staging
```

### Example 5: Mixed HTTP and HTTPS

```bash
# HTTP backend (port 8000)
zenoh-bridge-tcp --http-export 'mixed/api.example.com/192.168.1.10:8000'

# HTTPS backend (port 8443)
zenoh-bridge-tcp --http-export 'secure/api.example.com/192.168.1.10:8443'

# HTTP listener
zenoh-bridge-tcp --http-import 'mixed/0.0.0.0:80'

# HTTPS listener
zenoh-bridge-tcp --http-import 'secure/0.0.0.0:443'

# Both protocols work
curl http://api.example.com/         # → HTTP backend
curl https://api.example.com/        # → HTTPS backend
```

### Example 6: Using Zenoh Router

```bash
# Terminal 1: Start Zenoh router
zenohd

# Terminal 2: Export side (connects to router)
zenoh-bridge-tcp \
  --http-export 'service/api.example.com/192.168.1.10:8000' \
  --mode client \
  --connect tcp/localhost:7447

# Terminal 3: Import side (different machine, connects to router)
zenoh-bridge-tcp \
  --http-import 'service/0.0.0.0:8080' \
  --mode client \
  --connect tcp/router-host:7447

# Now clients can connect from anywhere
curl -H "Host: api.example.com" http://import-host:8080/
```

## Best Practices

### 1. Use Descriptive Service Names

```bash
# Good: Descriptive and meaningful
--http-export 'customer-api/api.example.com/...'
--http-export 'web-frontend/www.example.com/...'

# Bad: Generic and unclear
--http-export 'service1/api.example.com/...'
--http-export 's/www.example.com/...'
```

### 2. Group Related Services

```bash
# Group by application
--http-export 'myapp-api/api.myapp.com/...'
--http-export 'myapp-web/www.myapp.com/...'
--http-export 'myapp-admin/admin.myapp.com/...'
```

### 3. Use Consistent Naming

```bash
# Consistent naming scheme
service-environment-component

Examples:
--http-export 'app-prod-api/api.example.com/...'
--http-export 'app-prod-web/www.example.com/...'
--http-export 'app-dev-api/api-dev.example.com/...'
```

### 4. Monitor Backend Availability

The bridge automatically detects backend availability using Zenoh liveliness tokens. When a backend becomes unavailable:

- HTTP: Returns 502 Bad Gateway
- HTTPS: Closes connection
- Logs warning with DNS name

### 5. Handle DNS Properly

**For Testing**:
```bash
# Use /etc/hosts or curl --resolve
curl --resolve api.example.com:8080:127.0.0.1 http://api.example.com:8080/
```

**For Production**:
- Use real DNS records
- Or use /etc/hosts on all clients
- Or use a local DNS server

### 6. Secure HTTPS Properly

- Use valid certificates (not self-signed in production)
- The bridge does NOT validate certificates
- Clients validate certificates (as they should)
- Backend must have certificate for the DNS name

### 7. Use Logging for Debugging

```bash
# Debug level logging
RUST_LOG=debug zenoh-bridge-tcp --http-import 'service/0.0.0.0:8080'

# Module-specific logging
RUST_LOG=zenoh_bridge_tcp=debug zenoh-bridge-tcp --http-import '...'

# Trace level (very verbose)
RUST_LOG=trace zenoh-bridge-tcp --http-import '...'
```

### 8. Plan for Scale

Each client connection:
- Uses ~1 file descriptor
- Allocates ~64KB buffer
- Creates Zenoh pub/sub channels

Monitor system resources:
```bash
# Check open file descriptors
lsof -p <pid> | wc -l

# Check memory usage
ps aux | grep zenoh-bridge-tcp
```

## Troubleshooting

### Issue: Connection Refused

**Symptoms**: Client cannot connect to listener

**Causes**:
- Import bridge not running
- Wrong port number
- Firewall blocking port

**Solutions**:
```bash
# Check if bridge is listening
netstat -tulpn | grep 8080

# Check logs
RUST_LOG=info zenoh-bridge-tcp --http-import 'service/0.0.0.0:8080'

# Try binding to localhost only
zenoh-bridge-tcp --http-import 'service/127.0.0.1:8080'
```

### Issue: 502 Bad Gateway (HTTP)

**Symptoms**: HTTP 502 response

**Causes**:
- Export bridge not running
- Wrong DNS name in export
- Backend server not running
- DNS mismatch

**Solutions**:
```bash
# Check export bridge is running
ps aux | grep zenoh-bridge-tcp

# Verify DNS name matches
# Export: --http-export 'service/api.example.com/...'
# Request: curl -H "Host: api.example.com" ...

# Check backend is running
curl http://192.168.1.10:8000/  # Direct to backend

# Check logs on both sides
RUST_LOG=debug zenoh-bridge-tcp --http-export '...'
RUST_LOG=debug zenoh-bridge-tcp --http-import '...'
```

### Issue: Connection Closed (HTTPS)

**Symptoms**: HTTPS connection closes immediately

**Causes**:
- Missing SNI (client not sending SNI)
- Export bridge not running for that DNS
- Backend not running

**Solutions**:
```bash
# Verify SNI is sent (use openssl)
openssl s_client -connect localhost:443 -servername api.example.com

# Check logs for SNI detection
RUST_LOG=debug zenoh-bridge-tcp --http-import 'service/0.0.0.0:443'

# Verify export is running
ps aux | grep "http-export.*api.example.com"
```

### Issue: Wrong Backend

**Symptoms**: Traffic goes to wrong backend

**Causes**:
- DNS normalization issue
- Wrong Host header
- DNS name typo

**Solutions**:
```bash
# Check exact DNS being sent
curl -v -H "Host: api.example.com" http://localhost:8080/

# Check DNS normalization
# These should all route to same backend:
curl -H "Host: api.example.com" ...
curl -H "Host: API.EXAMPLE.COM" ...
curl -H "Host: api.example.com:80" ...

# Check export configuration
zenoh-bridge-tcp --http-export 'service/api.example.com/...'
                                         ^^^^^^^^^^^^^^^^
                                         Must match (after normalization)
```

### Issue: Slow Connections

**Symptoms**: High latency on connections

**Causes**:
- Backend is slow
- Network latency
- Zenoh network issues
- Too many connections

**Solutions**:
```bash
# Test direct backend latency
time curl http://192.168.1.10:8000/

# Test import bridge latency
time curl -H "Host: api.example.com" http://localhost:8080/

# Check Zenoh peer discovery
RUST_LOG=zenoh=debug zenoh-bridge-tcp --http-import '...'

# Monitor connections
watch -n 1 'netstat -an | grep :8080 | wc -l'
```

### Issue: Certificate Errors (HTTPS)

**Symptoms**: Certificate validation failures

**Causes**:
- Backend certificate doesn't match SNI
- Self-signed certificate in production
- Expired certificate

**Solutions**:
```bash
# Test backend certificate directly
openssl s_client -connect 192.168.1.10:8443 -servername api.example.com

# Verify certificate DNS names
openssl x509 -in cert.pem -text -noout | grep DNS

# For testing, use --insecure
curl https://api.example.com/ --insecure

# For production, use valid certificates
# - Let's Encrypt
# - Commercial CA
# - Internal CA with client trust
```

## Advanced Topics

### Multiple Zenoh Sessions

Run multiple import/export bridges with different services:

```bash
# HTTP services on port 80
zenoh-bridge-tcp \
  --http-import 'http-service/0.0.0.0:80' \
  --http-export 'http-service/www.example.com/192.168.1.10:8000' \
  --http-export 'http-service/api.example.com/192.168.1.20:8000'

# HTTPS services on port 443
zenoh-bridge-tcp \
  --http-import 'https-service/0.0.0.0:443' \
  --http-export 'https-service/www.example.com/192.168.1.10:8443' \
  --http-export 'https-service/api.example.com/192.168.1.20:8443'

# Legacy TCP services
zenoh-bridge-tcp \
  --import 'database/0.0.0.0:5432' \
  --export 'database/192.168.1.30:5432'
```

### Custom Zenoh Configuration

Use a config file for advanced Zenoh settings:

```json5
// zenoh-config.json5
{
  mode: "peer",
  connect: {
    endpoints: ["tcp/10.0.0.1:7447"]
  },
  listen: {
    endpoints: ["tcp/0.0.0.0:7447"]
  },
  scouting: {
    multicast: {
      enabled: true
    }
  }
}
```

```bash
zenoh-bridge-tcp \
  --config zenoh-config.json5 \
  --http-import 'service/0.0.0.0:8080' \
  --http-export 'service/api.example.com/192.168.1.10:8000'
```

### Monitoring and Metrics

Monitor bridge activity:

```bash
# Connection count
netstat -an | grep :8080 | grep ESTABLISHED | wc -l

# Traffic volume
iftop -i eth0

# System resources
htop

# Zenoh-specific logging
RUST_LOG=zenoh_bridge_tcp=info zenoh-bridge-tcp --http-import '...'
```

### Security Considerations

**DNS-Based Routing Security**:
- Anyone who can set the Host header can access any backend
- Use additional authentication at the application layer
- Consider network segmentation
- Use firewall rules to restrict backend access

**TLS Passthrough Security**:
- Bridge never sees decrypted data
- End-to-end encryption maintained
- No certificate management on bridge
- Backend validates client certificates (if using mTLS)

**Recommended Setup**:
```
Internet → Firewall → Import Bridge → Zenoh → Export Bridge → Backend
                ↓                                              ↓
         Accepts all                              Validate auth,
         DNS names                               enforce policies
```

### Performance Tuning

**System Limits**:
```bash
# Increase file descriptor limit
ulimit -n 65536

# Increase TCP buffer sizes
sysctl -w net.core.rmem_max=134217728
sysctl -w net.core.wmem_max=134217728
```

**Zenoh Tuning**:
```json5
{
  transport: {
    unicast: {
      max_sessions: 10000,
      max_links: 4
    }
  }
}
```

### High Availability

Run multiple import bridges for redundancy:

```bash
# Import bridge 1
zenoh-bridge-tcp --http-import 'service/0.0.0.0:8080'

# Import bridge 2 (different port or machine)
zenoh-bridge-tcp --http-import 'service/0.0.0.0:8081'

# Use load balancer in front
# HAProxy, Nginx, etc. can balance between :8080 and :8081
```

Run multiple export bridges for the same DNS:

```bash
# Backend 1
zenoh-bridge-tcp --http-export 'service/api.example.com/192.168.1.10:8000'

# Backend 2 (same DNS)
zenoh-bridge-tcp --http-export 'service/api.example.com/192.168.1.20:8000'

# Currently: First one to register wins
# Future: Load balancing between multiple backends
```

## Summary

The HTTP/HTTPS routing feature provides powerful DNS-based service discovery and routing capabilities:

✅ Single listener routes to multiple backends  
✅ Automatic protocol detection (HTTP vs HTTPS)  
✅ No TLS termination (end-to-end encryption)  
✅ DNS normalization (case-insensitive, port-aware)  
✅ Dynamic backend registration  
✅ Comprehensive error handling  

For more information:
- [README.md](../README.md) - General bridge documentation
- [TODO.md](../TODO.md) - Feature roadmap and implementation status
- [PHASE_B_SUMMARY.md](../PHASE_B_SUMMARY.md) - Technical implementation details