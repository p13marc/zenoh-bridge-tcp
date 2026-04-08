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
/// Maximum Content-Length value accepted (1 GiB).
/// Responses claiming to be larger are rejected at the parsing layer.
const MAX_CONTENT_LENGTH: usize = 1024 * 1024 * 1024;

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

            // Scan for Transfer-Encoding and Content-Length in a single pass
            let mut has_chunked = false;
            let mut content_lengths: Vec<usize> = Vec::new();

            for header in response.headers.iter() {
                if header.name.eq_ignore_ascii_case("transfer-encoding")
                    && let Ok(value) = std::str::from_utf8(header.value)
                    && value.to_lowercase().contains("chunked")
                {
                    has_chunked = true;
                } else if header.name.eq_ignore_ascii_case("content-length")
                    && let Ok(value) = std::str::from_utf8(header.value)
                    && let Ok(len) = value.trim().parse::<usize>()
                {
                    content_lengths.push(len);
                }
            }

            // RFC 7230 §3.3.3: reject if both Transfer-Encoding and Content-Length present
            if has_chunked && !content_lengths.is_empty() {
                return Err(BridgeError::HttpParse(
                    "Invalid response: both Transfer-Encoding and Content-Length present"
                        .to_string(),
                ));
            }

            if has_chunked {
                return Ok(Some((header_len, ResponseBodyFraming::Chunked)));
            }

            // RFC 7230 §3.3.3: reject multiple Content-Length with differing values
            if content_lengths.len() > 1
                && !content_lengths.windows(2).all(|w| w[0] == w[1])
            {
                return Err(BridgeError::HttpParse(
                    "Invalid response: multiple Content-Length headers with differing values"
                        .to_string(),
                ));
            }

            if let Some(&len) = content_lengths.first() {
                if len > MAX_CONTENT_LENGTH {
                    return Err(BridgeError::HttpParse(format!(
                        "Content-Length {} exceeds maximum allowed ({})",
                        len, MAX_CONTENT_LENGTH
                    )));
                }
                return Ok(Some((header_len, ResponseBodyFraming::ContentLength(len))));
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
            if header.name.eq_ignore_ascii_case("connection")
                && let Ok(value) = std::str::from_utf8(header.value)
                && value.to_lowercase().contains("close")
            {
                return true;
            }
        }
    }
    false
}

/// Parse a chunked transfer encoding body to find the end.
///
/// Returns `Some(total_bytes_consumed)` if the final chunk (`0\r\n\r\n`) is found,
/// `None` if more data is needed.
/// Maximum single chunk size accepted (256 MiB).
/// Chunks claiming to be larger are rejected to prevent overflow and DoS.
const MAX_CHUNK_SIZE: usize = 256 * 1024 * 1024;

pub fn find_chunked_body_end(body: &[u8]) -> Option<usize> {
    let mut pos = 0;

    loop {
        // Find end of chunk size line
        let line_end = find_crlf(&body[pos..])?;
        let chunk_size_str = std::str::from_utf8(&body[pos..pos + line_end]).ok()?;

        // Parse chunk size (hex), ignore extensions after ';'
        let size_part = chunk_size_str.split(';').next()?;
        let chunk_size = usize::from_str_radix(size_part.trim(), 16).ok()?;

        // Reject absurdly large chunk sizes to prevent overflow and DoS
        if chunk_size > MAX_CHUNK_SIZE {
            return None;
        }

        pos += line_end + 2; // Skip past chunk-size line and CRLF

        if chunk_size == 0 {
            // Terminal chunk. Expect trailing CRLF.
            if body.len() >= pos + 2 && body[pos] == b'\r' && body[pos + 1] == b'\n' {
                return Some(pos + 2);
            }
            return None; // Need more data for trailing CRLF
        }

        // Skip chunk data + trailing CRLF (overflow-safe)
        let chunk_end = pos.checked_add(chunk_size)?.checked_add(2)?;
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
        let response = b"HTTP/1.1 200 OK\r\nConnection: close\r\nContent-Length: 2\r\n\r\nOK";
        assert!(is_connection_close(response));
    }

    #[test]
    fn test_connection_keep_alive() {
        let response = b"HTTP/1.1 200 OK\r\nConnection: keep-alive\r\nContent-Length: 2\r\n\r\nOK";
        assert!(!is_connection_close(response));
    }

    #[test]
    fn test_connection_close_case_insensitive() {
        let response = b"HTTP/1.1 200 OK\r\nconnection: Close\r\nContent-Length: 2\r\n\r\nOK";
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

    // --- Security boundary tests (RFC 9112 compliance) ---

    #[test]
    fn test_reject_te_and_cl_together() {
        let response =
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nContent-Length: 10\r\n\r\n";
        let result = parse_response_headers(response);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("Transfer-Encoding"));
    }

    #[test]
    fn test_reject_cl_then_te_order() {
        // Same rejection regardless of header order
        let response =
            b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\nTransfer-Encoding: chunked\r\n\r\n";
        assert!(parse_response_headers(response).is_err());
    }

    #[test]
    fn test_reject_differing_content_lengths() {
        let response =
            b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\nContent-Length: 20\r\n\r\n";
        assert!(parse_response_headers(response).is_err());
    }

    #[test]
    fn test_accept_identical_content_lengths() {
        let response =
            b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\nContent-Length: 10\r\n\r\n";
        let (_, framing) = parse_response_headers(response).unwrap().unwrap();
        assert_eq!(framing, ResponseBodyFraming::ContentLength(10));
    }

    #[test]
    fn test_content_length_at_max_boundary() {
        let max = MAX_CONTENT_LENGTH;
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n",
            max
        );
        let (_, framing) = parse_response_headers(response.as_bytes()).unwrap().unwrap();
        assert_eq!(framing, ResponseBodyFraming::ContentLength(max));
    }

    #[test]
    fn test_content_length_above_max_rejected() {
        let over = MAX_CONTENT_LENGTH + 1;
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n",
            over
        );
        assert!(parse_response_headers(response.as_bytes()).is_err());
    }

    #[test]
    fn test_chunk_size_at_max_boundary() {
        // Chunk size exactly at MAX_CHUNK_SIZE — should be accepted (returns None for need-more-data)
        let hex = format!("{:X}\r\n", MAX_CHUNK_SIZE);
        let result = find_chunked_body_end(hex.as_bytes());
        assert_eq!(result, None); // needs data, but size was accepted
    }

    #[test]
    fn test_chunk_size_above_max_rejected() {
        // Chunk size above MAX_CHUNK_SIZE — should return None immediately
        let hex = format!("{:X}\r\n", MAX_CHUNK_SIZE + 1);
        let result = find_chunked_body_end(hex.as_bytes());
        assert_eq!(result, None);
    }

    #[test]
    fn test_te_chunked_without_cl_accepted() {
        // TE alone is fine
        let response = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n";
        let (_, framing) = parse_response_headers(response).unwrap().unwrap();
        assert_eq!(framing, ResponseBodyFraming::Chunked);
    }

    #[test]
    fn test_te_non_chunked_is_until_close() {
        // TE: gzip (not chunked) → falls through to UntilClose
        let response = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: gzip\r\n\r\n";
        let (_, framing) = parse_response_headers(response).unwrap().unwrap();
        assert_eq!(framing, ResponseBodyFraming::UntilClose);
    }

    #[test]
    fn test_content_length_zero() {
        let response = b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n";
        let (_, framing) = parse_response_headers(response).unwrap().unwrap();
        assert_eq!(framing, ResponseBodyFraming::ContentLength(0));
    }
}
