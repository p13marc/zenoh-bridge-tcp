//! Canned HTTP/1.1 error responses written directly to a client socket when the
//! bridge cannot proxy a request (bad request, no backend, gateway timeout).
//!
//! These are plain byte templates with no parsing dependency, kept separate from
//! the request/response parsers so they survive the flowscope adoption.

/// Generate an HTTP 400 Bad Request response.
pub fn http_400_response() -> Vec<u8> {
    let body = "400 Bad Request: Missing Host header";
    format!(
        "HTTP/1.1 400 Bad Request\r\n\
         Content-Type: text/plain\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\
         \r\n\
         {}",
        body.len(),
        body
    )
    .into_bytes()
}

/// Generate an HTTP 502 Bad Gateway response.
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

/// Generate an HTTP 504 Gateway Timeout response.
pub fn http_504_response() -> Vec<u8> {
    let body = "504 Gateway Timeout";
    format!(
        "HTTP/1.1 504 Gateway Timeout\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    )
    .into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_http_400_response() {
        let response = http_400_response();
        let response_str = String::from_utf8_lossy(&response);
        assert!(response_str.contains("400 Bad Request"));
        assert!(response_str.contains("Missing Host header"));

        // Verify proper HTTP formatting: no leading spaces in headers
        let parts: Vec<&str> = response_str.split("\r\n").collect();
        for (i, part) in parts.iter().enumerate() {
            if i == 0 {
                assert!(
                    part.starts_with("HTTP/1.1"),
                    "Status line should start with HTTP/1.1"
                );
            } else if !part.is_empty() {
                assert!(
                    !part.starts_with(' '),
                    "HTTP header/body should not start with space: {:?}",
                    part
                );
            }
        }

        // Verify Content-Length matches actual body
        let header_end = response_str
            .find("\r\n\r\n")
            .expect("should have header terminator");
        let body = &response_str[header_end + 4..];
        let expected_cl = format!("Content-Length: {}\r\n", body.len());
        assert!(
            response_str.contains(&expected_cl),
            "Content-Length should match body size. Body len: {}, response: {:?}",
            body.len(),
            response_str
        );
    }

    #[test]
    fn test_http_502_response() {
        let response = http_502_response("example.com");
        let response_str = String::from_utf8_lossy(&response);
        assert!(response_str.contains("502 Bad Gateway"));
        assert!(response_str.contains("example.com"));

        // Verify proper HTTP formatting: no leading spaces in headers
        let parts: Vec<&str> = response_str.split("\r\n").collect();
        for (i, part) in parts.iter().enumerate() {
            if i == 0 {
                assert!(
                    part.starts_with("HTTP/1.1"),
                    "Status line should start with HTTP/1.1"
                );
            } else if !part.is_empty() {
                assert!(
                    !part.starts_with(' '),
                    "HTTP header/body should not start with space: {:?}",
                    part
                );
            }
        }

        // Verify Content-Length matches actual body
        let header_end = response_str
            .find("\r\n\r\n")
            .expect("should have header terminator");
        let body = &response_str[header_end + 4..];
        let expected_cl = format!("Content-Length: {}\r\n", body.len());
        assert!(
            response_str.contains(&expected_cl),
            "Content-Length should match body size. Body len: {}, response: {:?}",
            body.len(),
            response_str
        );
    }

    #[test]
    fn test_http_504_response() {
        let response = http_504_response();
        let s = String::from_utf8_lossy(&response);
        assert!(s.contains("504 Gateway Timeout"));

        // Verify Content-Length matches actual body
        let body_start = s.find("\r\n\r\n").unwrap() + 4;
        let body = &s[body_start..];
        let cl_start = s.find("Content-Length: ").unwrap() + 16;
        let cl_end = s[cl_start..].find("\r\n").unwrap() + cl_start;
        let content_length: usize = s[cl_start..cl_end].parse().unwrap();
        assert_eq!(content_length, body.len());
    }

    #[test]
    fn test_http_502_response_various_dns() {
        for dns in &["a.com", "very-long-subdomain.deep.nested.example.com", "x"] {
            let response = http_502_response(dns);
            let s = String::from_utf8_lossy(&response);
            let header_end = s.find("\r\n\r\n").unwrap() + 4;
            let body = &s[header_end..];
            let cl_start = s.find("Content-Length: ").unwrap() + 16;
            let cl_end = s[cl_start..].find("\r\n").unwrap() + cl_start;
            let cl: usize = s[cl_start..cl_end].parse().unwrap();
            assert_eq!(cl, body.len(), "Content-Length mismatch for dns='{}'", dns);
        }
    }
}
