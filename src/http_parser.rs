//! HTTP/1.x request parser for extracting Host headers
//!
//! This module provides functionality to parse HTTP/1.x requests and extract
//! the Host header for routing purposes. It includes DNS normalization to
//! ensure consistent routing behavior.

use crate::config::BridgeConfig;
use crate::error::{BridgeError, Result};
use tokio::io::AsyncReadExt;
use tokio::time::timeout;
use tracing::{debug, warn};

/// Parsed HTTP request information
#[derive(Debug, Clone)]
pub struct ParsedHttpRequest {
    /// The normalized DNS name extracted from the Host header
    pub dns: String,
    /// The complete buffered request that should be forwarded
    pub buffer: Vec<u8>,
}

/// Parse an HTTP request from a TCP stream and extract the Host header
///
/// This function:
/// 1. Reads the initial HTTP request with a timeout
/// 2. Parses the request to extract the Host header
/// 3. Normalizes the DNS name (lowercase, port stripping)
/// 4. Returns both the DNS and the complete buffered request
///
/// # Arguments
/// * `stream` - A mutable reference to the TCP stream reader
/// * `config` - Bridge configuration with timeout and size limits
///
/// # Returns
/// * `Ok(ParsedHttpRequest)` - Successfully parsed request with DNS and buffer
/// * `Err` - If parsing fails, timeout occurs, or Host header is missing
pub async fn parse_http_request_with_config<R>(
    stream: &mut R,
    config: &BridgeConfig,
) -> Result<ParsedHttpRequest>
where
    R: AsyncReadExt + Unpin,
{
    // Buffer to accumulate the request
    let mut buffer = Vec::with_capacity(4096);
    let mut temp_buf = vec![0u8; 4096];

    let max_header_size = config.max_header_size;
    let read_timeout = config.read_timeout;

    // Read with timeout to prevent hanging on slow clients
    let read_result = timeout(read_timeout, async {
        loop {
            // Try to parse what we have so far
            if let Some(parsed) = try_parse_request(&buffer)? {
                return Ok(parsed);
            }

            // Check if we've exceeded the maximum header size
            if buffer.len() >= max_header_size {
                return Err(BridgeError::HttpParse(format!(
                    "HTTP headers exceed maximum size of {} bytes",
                    max_header_size
                )));
            }

            // Read more data
            let n = stream.read(&mut temp_buf).await?;
            if n == 0 {
                return Err(BridgeError::HttpParse(
                    "Connection closed before complete HTTP request".to_string(),
                ));
            }

            buffer.extend_from_slice(&temp_buf[..n]);
        }
    })
    .await;

    match read_result {
        Ok(result) => result,
        Err(_) => Err(BridgeError::Timeout("reading HTTP request".to_string())),
    }
}

/// Parse an HTTP request using default configuration
///
/// This is a convenience function that uses default timeout and size limits.
pub async fn parse_http_request<R>(stream: &mut R) -> Result<ParsedHttpRequest>
where
    R: AsyncReadExt + Unpin,
{
    parse_http_request_with_config(stream, &BridgeConfig::default()).await
}

/// Try to parse the buffered data as an HTTP request
///
/// Returns `Ok(Some(ParsedHttpRequest))` if parsing succeeds,
/// `Ok(None)` if more data is needed,
/// or `Err` if parsing fails definitively.
fn try_parse_request(buffer: &[u8]) -> Result<Option<ParsedHttpRequest>> {
    let mut headers = [httparse::EMPTY_HEADER; 64];
    let mut req = httparse::Request::new(&mut headers);

    match req.parse(buffer) {
        Ok(httparse::Status::Complete(body_offset)) => {
            // Successfully parsed headers
            let dns = extract_and_normalize_host(&req, buffer)?;

            debug!(
                "Parsed HTTP request: {} {} (Host: {})",
                req.method.unwrap_or("?"),
                req.path.unwrap_or("?"),
                dns
            );

            Ok(Some(ParsedHttpRequest {
                dns,
                buffer: buffer[..body_offset].to_vec(),
            }))
        }
        Ok(httparse::Status::Partial) => {
            // Need more data
            Ok(None)
        }
        Err(e) => Err(BridgeError::HttpParse(format!("Invalid HTTP request: {}", e))),
    }
}

/// Extract the Host header and normalize it to a DNS name
///
/// This function handles:
/// - HTTP/1.1 Host header
/// - HTTP/1.0 absolute URIs
/// - Case normalization (lowercase)
/// - Port stripping (default ports 80/443)
fn extract_and_normalize_host(req: &httparse::Request, _buffer: &[u8]) -> Result<String> {
    // First, try to get Host from headers
    if let Some(host) = find_host_header(req) {
        let normalized = normalize_dns(host);
        if normalized.is_empty() {
            return Err(BridgeError::HttpParse("Host header is empty".to_string()));
        }
        return Ok(normalized);
    }

    // HTTP/1.0 might use absolute URI: GET http://example.com/path HTTP/1.0
    if let Some(path) = req.path
        && let Some(host) = extract_host_from_absolute_uri(path) {
            let normalized = normalize_dns(host);
            if normalized.is_empty() {
                return Err(BridgeError::HttpParse(
                    "Host is empty in absolute URI".to_string(),
                ));
            }
            return Ok(normalized);
        }

    // No Host header found
    warn!("HTTP request missing Host header");
    Err(BridgeError::HttpParse(
        "HTTP request missing Host header".to_string(),
    ))
}

/// Find the Host header in the parsed request
fn find_host_header<'a, 'b>(req: &'a httparse::Request<'a, 'b>) -> Option<&'b str> {
    for header in req.headers.iter() {
        if header.name.eq_ignore_ascii_case("host") {
            // Convert header value bytes to string
            if let Ok(value) = std::str::from_utf8(header.value) {
                let trimmed = value.trim();
                if trimmed.is_empty() {
                    return None;
                }
                return Some(trimmed);
            }
        }
    }
    None
}

/// Extract host from absolute URI (HTTP/1.0 style)
///
/// Example: "http://example.com:8080/path" -> Some("example.com:8080")
fn extract_host_from_absolute_uri(path: &str) -> Option<&str> {
    // Check if path starts with http:// or https://
    if let Some(rest) = path.strip_prefix("http://") {
        // Find the end of the host (either '/' or end of string)
        if let Some(slash_pos) = rest.find('/') {
            return Some(&rest[..slash_pos]);
        } else {
            return Some(rest);
        }
    } else if let Some(rest) = path.strip_prefix("https://") {
        if let Some(slash_pos) = rest.find('/') {
            return Some(&rest[..slash_pos]);
        } else {
            return Some(rest);
        }
    }
    None
}

/// Normalize a DNS name for consistent routing
///
/// This function:
/// 1. Converts to lowercase (DNS is case-insensitive)
/// 2. Strips default ports (80 for HTTP, 443 for HTTPS)
///
/// Examples:
/// - "Example.COM" -> "example.com"
/// - "example.com:80" -> "example.com"
/// - "example.com:443" -> "example.com"
/// - "example.com:8080" -> "example.com:8080"
pub fn normalize_dns(host: &str) -> String {
    let host = host.to_lowercase();

    // Strip default ports
    if host.ends_with(":80") {
        host[..host.len() - 3].to_string()
    } else if host.ends_with(":443") {
        host[..host.len() - 4].to_string()
    } else {
        host
    }
}

/// Generate an HTTP 400 Bad Request response
pub fn http_400_response() -> Vec<u8> {
    b"HTTP/1.1 400 Bad Request\r\n\
      Content-Type: text/plain\r\n\
      Content-Length: 37\r\n\
      Connection: close\r\n\
      \r\n\
      400 Bad Request: Missing Host header"
        .to_vec()
}

/// Generate an HTTP 502 Bad Gateway response
pub fn http_502_response(dns: &str) -> Vec<u8> {
    let body = format!("502 Bad Gateway: No backend available for {}", dns);
    let content_length = body.len();

    format!(
        "HTTP/1.1 502 Bad Gateway\r\n\
         Content-Type: text/plain\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\
         \r\n\
         {}",
        content_length, body
    )
    .into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_normalize_dns_lowercase() {
        assert_eq!(normalize_dns("Example.COM"), "example.com");
        assert_eq!(normalize_dns("API.Example.COM"), "api.example.com");
    }

    #[test]
    fn test_normalize_dns_strip_port_80() {
        assert_eq!(normalize_dns("example.com:80"), "example.com");
        assert_eq!(normalize_dns("Example.COM:80"), "example.com");
    }

    #[test]
    fn test_normalize_dns_strip_port_443() {
        assert_eq!(normalize_dns("example.com:443"), "example.com");
        assert_eq!(normalize_dns("Example.COM:443"), "example.com");
    }

    #[test]
    fn test_normalize_dns_keep_custom_port() {
        assert_eq!(normalize_dns("example.com:8080"), "example.com:8080");
        assert_eq!(normalize_dns("example.com:3000"), "example.com:3000");
    }

    #[test]
    fn test_normalize_dns_combined() {
        assert_eq!(normalize_dns("Example.COM:80"), "example.com");
        assert_eq!(normalize_dns("API.Example.COM:443"), "api.example.com");
        assert_eq!(
            normalize_dns("Dev.Example.COM:8080"),
            "dev.example.com:8080"
        );
    }

    #[test]
    fn test_extract_host_from_absolute_uri_http() {
        assert_eq!(
            extract_host_from_absolute_uri("http://example.com/path"),
            Some("example.com")
        );
        assert_eq!(
            extract_host_from_absolute_uri("http://example.com:8080/path"),
            Some("example.com:8080")
        );
        assert_eq!(
            extract_host_from_absolute_uri("http://example.com"),
            Some("example.com")
        );
    }

    #[test]
    fn test_extract_host_from_absolute_uri_https() {
        assert_eq!(
            extract_host_from_absolute_uri("https://example.com/path"),
            Some("example.com")
        );
        assert_eq!(
            extract_host_from_absolute_uri("https://example.com:443/path"),
            Some("example.com:443")
        );
    }

    #[test]
    fn test_extract_host_from_absolute_uri_relative() {
        assert_eq!(extract_host_from_absolute_uri("/path"), None);
        assert_eq!(extract_host_from_absolute_uri("/"), None);
    }

    #[test]
    fn test_find_host_header() {
        let mut headers = [httparse::EMPTY_HEADER; 4];
        headers[0] = httparse::Header {
            name: "Content-Type",
            value: b"text/plain",
        };
        headers[1] = httparse::Header {
            name: "Host",
            value: b"example.com",
        };
        headers[2] = httparse::Header {
            name: "User-Agent",
            value: b"test",
        };

        let mut req = httparse::Request::new(&mut headers[..3]);
        req.method = Some("GET");
        req.path = Some("/");
        req.version = Some(1);

        assert_eq!(find_host_header(&req), Some("example.com"));
    }

    #[test]
    fn test_find_host_header_case_insensitive() {
        let mut headers = [httparse::EMPTY_HEADER; 1];
        headers[0] = httparse::Header {
            name: "host",
            value: b"example.com",
        };

        let mut req = httparse::Request::new(&mut headers);
        req.method = Some("GET");
        req.path = Some("/");
        req.version = Some(1);

        assert_eq!(find_host_header(&req), Some("example.com"));
    }

    #[tokio::test]
    async fn test_parse_http_request_complete() {
        let request = b"GET / HTTP/1.1\r\nHost: example.com\r\n\r\n";
        let mut cursor = std::io::Cursor::new(request);

        let result = parse_http_request(&mut cursor).await;
        assert!(result.is_ok());

        let parsed = result.unwrap();
        assert_eq!(parsed.dns, "example.com");
        assert!(!parsed.buffer.is_empty());
    }

    #[tokio::test]
    async fn test_parse_http_request_with_port() {
        let request = b"GET / HTTP/1.1\r\nHost: example.com:8080\r\n\r\n";
        let mut cursor = std::io::Cursor::new(request);

        let result = parse_http_request(&mut cursor).await;
        assert!(result.is_ok());

        let parsed = result.unwrap();
        assert_eq!(parsed.dns, "example.com:8080");
    }

    #[tokio::test]
    async fn test_parse_http_request_normalize_port_80() {
        let request = b"GET / HTTP/1.1\r\nHost: example.com:80\r\n\r\n";
        let mut cursor = std::io::Cursor::new(request);

        let result = parse_http_request(&mut cursor).await;
        assert!(result.is_ok());

        let parsed = result.unwrap();
        assert_eq!(parsed.dns, "example.com");
    }

    #[tokio::test]
    async fn test_parse_http_request_uppercase_host() {
        let request = b"GET / HTTP/1.1\r\nHost: Example.COM\r\n\r\n";
        let mut cursor = std::io::Cursor::new(request);

        let result = parse_http_request(&mut cursor).await;
        assert!(result.is_ok());

        let parsed = result.unwrap();
        assert_eq!(parsed.dns, "example.com");
    }

    #[tokio::test]
    async fn test_parse_http_request_missing_host() {
        let request = b"GET / HTTP/1.1\r\nContent-Type: text/plain\r\n\r\n";
        let mut cursor = std::io::Cursor::new(request);

        let result = parse_http_request(&mut cursor).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Host header"));
    }

    #[tokio::test]
    async fn test_parse_http_request_absolute_uri() {
        let request = b"GET http://example.com/path HTTP/1.0\r\n\r\n";
        let mut cursor = std::io::Cursor::new(request);

        let result = parse_http_request(&mut cursor).await;
        assert!(result.is_ok());

        let parsed = result.unwrap();
        assert_eq!(parsed.dns, "example.com");
    }

    #[tokio::test]
    async fn test_parse_http_request_post_with_body() {
        let request = b"POST /api HTTP/1.1\r\nHost: api.example.com\r\nContent-Length: 13\r\n\r\nHello, World!";
        let mut cursor = std::io::Cursor::new(request);

        let result = parse_http_request(&mut cursor).await;
        assert!(result.is_ok());

        let parsed = result.unwrap();
        assert_eq!(parsed.dns, "api.example.com");
        // Buffer should only contain headers, not body
        assert!(parsed.buffer.ends_with(b"\r\n\r\n"));
    }

    #[test]
    fn test_http_400_response() {
        let response = http_400_response();
        let response_str = String::from_utf8_lossy(&response);
        assert!(response_str.contains("400 Bad Request"));
        assert!(response_str.contains("Missing Host header"));
    }

    #[test]
    fn test_http_502_response() {
        let response = http_502_response("example.com");
        let response_str = String::from_utf8_lossy(&response);
        assert!(response_str.contains("502 Bad Gateway"));
        assert!(response_str.contains("example.com"));
    }
}
