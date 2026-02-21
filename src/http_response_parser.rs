//! HTTP/1.x response parser for detecting response boundaries.
//!
//! Used by the bidirectional HTTP mode to know when a response is
//! complete so the next request can be routed independently.

use crate::error::{BridgeError, Result};

/// Information about a parsed HTTP response's body framing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResponseBodyFraming {
    /// Response has Content-Length; body is exactly this many bytes.
    ContentLength(usize),
    /// Response uses chunked transfer encoding; body ends at 0-length chunk.
    Chunked,
    /// Response has no body (204 No Content, 304 Not Modified, 1xx, HEAD response).
    NoBody,
    /// Response body extends until connection close (no Content-Length, not chunked).
    /// This terminates the persistent connection.
    UntilClose,
}

/// Parse HTTP response headers and determine body framing.
///
/// Returns the header size (bytes consumed) and the body framing mode.
/// Returns `Ok(None)` if the headers are incomplete (need more data).
pub fn parse_response_headers(buffer: &[u8]) -> Result<Option<(usize, ResponseBodyFraming)>> {
    let mut headers = [httparse::EMPTY_HEADER; 64];
    let mut response = httparse::Response::new(&mut headers);

    match response.parse(buffer) {
        Ok(httparse::Status::Complete(header_len)) => {
            let status = response.code.unwrap_or(200);

            // 1xx, 204, 304 have no body
            if status < 200 || status == 204 || status == 304 {
                return Ok(Some((header_len, ResponseBodyFraming::NoBody)));
            }

            // Check Transfer-Encoding
            for header in response.headers.iter() {
                if header.name.eq_ignore_ascii_case("transfer-encoding") {
                    if let Ok(value) = std::str::from_utf8(header.value) {
                        if value.to_lowercase().contains("chunked") {
                            return Ok(Some((header_len, ResponseBodyFraming::Chunked)));
                        }
                    }
                }
            }

            // Check Content-Length
            for header in response.headers.iter() {
                if header.name.eq_ignore_ascii_case("content-length") {
                    if let Ok(value) = std::str::from_utf8(header.value) {
                        if let Ok(len) = value.trim().parse::<usize>() {
                            return Ok(Some((header_len, ResponseBodyFraming::ContentLength(len))));
                        }
                    }
                }
            }

            // No Content-Length and not chunked: body until close
            Ok(Some((header_len, ResponseBodyFraming::UntilClose)))
        }
        Ok(httparse::Status::Partial) => Ok(None),
        Err(e) => Err(BridgeError::HttpParse(format!(
            "Invalid HTTP response: {}",
            e
        ))),
    }
}

/// Check if a response header contains "Connection: close".
pub fn is_connection_close(buffer: &[u8]) -> bool {
    let mut headers = [httparse::EMPTY_HEADER; 64];
    let mut response = httparse::Response::new(&mut headers);

    if let Ok(httparse::Status::Complete(_)) = response.parse(buffer) {
        for header in response.headers.iter() {
            if header.name.eq_ignore_ascii_case("connection") {
                if let Ok(value) = std::str::from_utf8(header.value) {
                    if value.to_lowercase().contains("close") {
                        return true;
                    }
                }
            }
        }
    }
    false
}

/// Parse a chunked transfer encoding body to find the end.
///
/// Returns `Some(total_bytes_consumed)` if the final chunk (`0\r\n\r\n`) is found,
/// `None` if more data is needed.
pub fn find_chunked_body_end(body: &[u8]) -> Option<usize> {
    let mut pos = 0;

    loop {
        // Find end of chunk size line
        let line_end = find_crlf(&body[pos..])?;
        let chunk_size_str = std::str::from_utf8(&body[pos..pos + line_end]).ok()?;

        // Parse chunk size (hex), ignore extensions after ';'
        let size_part = chunk_size_str.split(';').next()?;
        let chunk_size = usize::from_str_radix(size_part.trim(), 16).ok()?;

        pos += line_end + 2; // Skip past chunk-size line and CRLF

        if chunk_size == 0 {
            // Terminal chunk. Expect trailing CRLF.
            if body.len() >= pos + 2 && body[pos] == b'\r' && body[pos + 1] == b'\n' {
                return Some(pos + 2);
            }
            return None; // Need more data for trailing CRLF
        }

        // Skip chunk data + trailing CRLF
        let chunk_end = pos + chunk_size + 2;
        if body.len() < chunk_end {
            return None; // Need more data
        }
        pos = chunk_end;
    }
}

/// Find the position of \r\n in buffer. Returns offset of \r.
fn find_crlf(buf: &[u8]) -> Option<usize> {
    buf.windows(2).position(|w| w == b"\r\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_content_length_response() {
        let response = b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nHello";
        let result = parse_response_headers(response);
        assert!(result.is_ok());
        let (header_len, framing) = result.unwrap().unwrap();
        assert_eq!(framing, ResponseBodyFraming::ContentLength(5));
        assert!(header_len > 0);
    }

    #[test]
    fn test_parse_chunked_response() {
        let response = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n";
        let result = parse_response_headers(response);
        assert!(result.is_ok());
        let (_, framing) = result.unwrap().unwrap();
        assert_eq!(framing, ResponseBodyFraming::Chunked);
    }

    #[test]
    fn test_parse_204_no_body() {
        let response = b"HTTP/1.1 204 No Content\r\n\r\n";
        let result = parse_response_headers(response);
        assert!(result.is_ok());
        let (_, framing) = result.unwrap().unwrap();
        assert_eq!(framing, ResponseBodyFraming::NoBody);
    }

    #[test]
    fn test_parse_304_no_body() {
        let response = b"HTTP/1.1 304 Not Modified\r\n\r\n";
        let result = parse_response_headers(response);
        let (_, framing) = result.unwrap().unwrap();
        assert_eq!(framing, ResponseBodyFraming::NoBody);
    }

    #[test]
    fn test_parse_1xx_no_body() {
        let response = b"HTTP/1.1 100 Continue\r\n\r\n";
        let result = parse_response_headers(response);
        let (_, framing) = result.unwrap().unwrap();
        assert_eq!(framing, ResponseBodyFraming::NoBody);
    }

    #[test]
    fn test_parse_no_content_length_until_close() {
        let response = b"HTTP/1.1 200 OK\r\nContent-Type: text/html\r\n\r\n";
        let result = parse_response_headers(response);
        let (_, framing) = result.unwrap().unwrap();
        assert_eq!(framing, ResponseBodyFraming::UntilClose);
    }

    #[test]
    fn test_parse_partial_response() {
        let response = b"HTTP/1.1 200 OK\r\nContent-Len";
        let result = parse_response_headers(response);
        assert!(result.is_ok());
        assert!(result.unwrap().is_none());
    }

    #[test]
    fn test_connection_close_detected() {
        let response =
            b"HTTP/1.1 200 OK\r\nConnection: close\r\nContent-Length: 2\r\n\r\nOK";
        assert!(is_connection_close(response));
    }

    #[test]
    fn test_connection_keep_alive() {
        let response =
            b"HTTP/1.1 200 OK\r\nConnection: keep-alive\r\nContent-Length: 2\r\n\r\nOK";
        assert!(!is_connection_close(response));
    }

    #[test]
    fn test_connection_close_case_insensitive() {
        let response =
            b"HTTP/1.1 200 OK\r\nconnection: Close\r\nContent-Length: 2\r\n\r\nOK";
        assert!(is_connection_close(response));
    }

    #[test]
    fn test_find_chunked_end_simple() {
        let body = b"5\r\nHello\r\n0\r\n\r\n";
        assert_eq!(find_chunked_body_end(body), Some(body.len()));
    }

    #[test]
    fn test_find_chunked_end_multiple_chunks() {
        let body = b"5\r\nHello\r\n6\r\n World\r\n0\r\n\r\n";
        assert_eq!(find_chunked_body_end(body), Some(body.len()));
    }

    #[test]
    fn test_find_chunked_end_incomplete() {
        let body = b"5\r\nHello\r\n";
        assert_eq!(find_chunked_body_end(body), None);
    }

    #[test]
    fn test_find_chunked_end_with_extensions() {
        // Chunk extensions (;ext=val) are allowed but ignored
        let body = b"5;ext=val\r\nHello\r\n0\r\n\r\n";
        assert_eq!(find_chunked_body_end(body), Some(body.len()));
    }

    #[test]
    fn test_find_chunked_end_empty() {
        let body = b"0\r\n\r\n";
        assert_eq!(find_chunked_body_end(body), Some(5));
    }

    #[test]
    fn test_find_chunked_end_missing_terminal_crlf() {
        let body = b"5\r\nHello\r\n0\r\n";
        assert_eq!(find_chunked_body_end(body), None);
    }
}
