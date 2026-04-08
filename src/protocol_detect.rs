//! Protocol auto-detection by peeking at the first bytes of a connection.

use crate::tls_parser::is_tls_handshake;

/// Detected protocol for an incoming connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DetectedProtocol {
    /// TLS/HTTPS - starts with 0x16 0x03 (TLS handshake)
    Tls,
    /// HTTP - starts with a known HTTP method
    Http,
    /// Raw TCP - anything else
    RawTcp,
}

/// Known HTTP method prefixes.
/// WebSocket is detected as Http first, then distinguished during HTTP parsing
/// by the presence of the `Upgrade: websocket` header.
const HTTP_METHODS: &[&[u8]] = &[
    b"GET ",
    b"POST ",
    b"PUT ",
    b"DELETE ",
    b"HEAD ",
    b"OPTIONS ",
    b"PATCH ",
    b"CONNECT ",
    b"TRACE ",
];

/// Detect the protocol from the first bytes of a connection.
///
/// Requires at least 8 bytes for reliable detection. With fewer bytes,
/// falls back to raw TCP if no match is found.
pub fn detect_protocol(peek_buf: &[u8]) -> DetectedProtocol {
    if peek_buf.is_empty() {
        return DetectedProtocol::RawTcp;
    }

    if is_tls_handshake(peek_buf) {
        return DetectedProtocol::Tls;
    }

    for method in HTTP_METHODS {
        if peek_buf.len() >= method.len() && peek_buf[..method.len()] == **method {
            return DetectedProtocol::Http;
        }
    }

    DetectedProtocol::RawTcp
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_detect_tls() {
        // TLS 1.2 ClientHello
        let buf = [0x16, 0x03, 0x03, 0x00, 0x05, 0x01];
        assert_eq!(detect_protocol(&buf), DetectedProtocol::Tls);

        // TLS 1.3 (legacy header says 1.0)
        let buf = [0x16, 0x03, 0x01, 0x00, 0x05, 0x01];
        assert_eq!(detect_protocol(&buf), DetectedProtocol::Tls);
    }

    #[test]
    fn test_detect_http_methods() {
        assert_eq!(
            detect_protocol(b"GET / HTTP/1.1\r\n"),
            DetectedProtocol::Http
        );
        assert_eq!(
            detect_protocol(b"POST /api HTTP/1.1\r\n"),
            DetectedProtocol::Http
        );
        assert_eq!(
            detect_protocol(b"PUT /data HTTP/1.1\r\n"),
            DetectedProtocol::Http
        );
        assert_eq!(
            detect_protocol(b"DELETE /item HTTP/1.1\r\n"),
            DetectedProtocol::Http
        );
        assert_eq!(
            detect_protocol(b"HEAD / HTTP/1.1\r\n"),
            DetectedProtocol::Http
        );
        assert_eq!(
            detect_protocol(b"OPTIONS * HTTP/1.1\r\n"),
            DetectedProtocol::Http
        );
        assert_eq!(
            detect_protocol(b"PATCH /item HTTP/1.1\r\n"),
            DetectedProtocol::Http
        );
        assert_eq!(
            detect_protocol(b"CONNECT host:443 HTTP/1.1\r\n"),
            DetectedProtocol::Http
        );
        assert_eq!(
            detect_protocol(b"TRACE / HTTP/1.1\r\n"),
            DetectedProtocol::Http
        );
    }

    #[test]
    fn test_detect_raw_tcp() {
        assert_eq!(
            detect_protocol(b"\x00\x01\x02\x03"),
            DetectedProtocol::RawTcp
        );
        assert_eq!(detect_protocol(b"HELLO server"), DetectedProtocol::RawTcp);
        assert_eq!(
            detect_protocol(b"SSH-2.0-OpenSSH"),
            DetectedProtocol::RawTcp
        );
    }

    #[test]
    fn test_detect_empty_buffer() {
        assert_eq!(detect_protocol(b""), DetectedProtocol::RawTcp);
    }

    #[test]
    fn test_detect_short_buffer() {
        // Just one byte
        assert_eq!(detect_protocol(b"\x16"), DetectedProtocol::RawTcp);
        // Two bytes
        assert_eq!(detect_protocol(&[0x16, 0x03]), DetectedProtocol::RawTcp);
        // Five bytes - still not enough for TLS
        assert_eq!(
            detect_protocol(&[0x16, 0x03, 0x03, 0x00, 0x05]),
            DetectedProtocol::RawTcp
        );
        // Six bytes with ClientHello - detected as TLS
        assert_eq!(
            detect_protocol(&[0x16, 0x03, 0x03, 0x00, 0x05, 0x01]),
            DetectedProtocol::Tls
        );
    }

    #[test]
    fn test_detect_almost_http() {
        assert_eq!(detect_protocol(b"GE"), DetectedProtocol::RawTcp);
        assert_eq!(detect_protocol(b"GET"), DetectedProtocol::RawTcp);
        assert_eq!(detect_protocol(b"GET "), DetectedProtocol::Http);
    }

    // --- Case sensitivity: HTTP methods must be uppercase ---

    #[test]
    fn test_detect_lowercase_http_methods_as_raw_tcp() {
        assert_eq!(detect_protocol(b"get / HTTP/1.1"), DetectedProtocol::RawTcp);
        assert_eq!(detect_protocol(b"post /api"), DetectedProtocol::RawTcp);
        assert_eq!(detect_protocol(b"Put /data"), DetectedProtocol::RawTcp);
    }

    // --- TLS version variants ---

    #[test]
    fn test_detect_tls_version_variants() {
        // TLS 1.0
        assert_eq!(
            detect_protocol(&[0x16, 0x03, 0x01, 0x00, 0x05, 0x01]),
            DetectedProtocol::Tls
        );
        // TLS 1.1
        assert_eq!(
            detect_protocol(&[0x16, 0x03, 0x02, 0x00, 0x05, 0x01]),
            DetectedProtocol::Tls
        );
        // TLS 1.2
        assert_eq!(
            detect_protocol(&[0x16, 0x03, 0x03, 0x00, 0x05, 0x01]),
            DetectedProtocol::Tls
        );
    }

    // --- Not TLS: wrong record type or version ---

    #[test]
    fn test_detect_not_tls_wrong_record_type() {
        // 0x17 is application data, not handshake
        assert_eq!(
            detect_protocol(&[0x17, 0x03, 0x03, 0x00, 0x05, 0x01]),
            DetectedProtocol::RawTcp
        );
    }

    #[test]
    fn test_detect_not_tls_wrong_major_version() {
        // Major version 0x04 is not TLS
        assert_eq!(
            detect_protocol(&[0x16, 0x04, 0x03, 0x00, 0x05, 0x01]),
            DetectedProtocol::RawTcp
        );
    }

    #[test]
    fn test_detect_not_tls_not_client_hello() {
        // Record type 0x16, TLS 1.2, but msg type 0x02 (ServerHello) not ClientHello
        assert_eq!(
            detect_protocol(&[0x16, 0x03, 0x03, 0x00, 0x05, 0x02]),
            DetectedProtocol::RawTcp
        );
    }

    // --- Binary data that could be confused ---

    #[test]
    fn test_detect_binary_starting_with_0x16() {
        // Only 2 bytes — not enough for TLS detection
        assert_eq!(detect_protocol(&[0x16, 0x03]), DetectedProtocol::RawTcp);
    }

    // --- Protocol enum properties ---

    #[test]
    fn test_detected_protocol_debug_and_clone() {
        let p = DetectedProtocol::Http;
        let cloned = p.clone();
        assert_eq!(p, cloned);
        assert_eq!(format!("{:?}", p), "Http");
    }

    #[test]
    fn test_detected_protocol_all_variants() {
        let variants = [
            DetectedProtocol::Tls,
            DetectedProtocol::Http,
            DetectedProtocol::RawTcp,
        ];
        for (i, a) in variants.iter().enumerate() {
            for (j, b) in variants.iter().enumerate() {
                if i == j {
                    assert_eq!(a, b);
                } else {
                    assert_ne!(a, b);
                }
            }
        }
    }

    // --- Real-world-like payloads ---

    #[test]
    fn test_detect_http_with_full_request() {
        let request = b"POST /api/v2/data HTTP/1.1\r\nHost: api.example.com\r\nContent-Length: 42\r\n\r\n";
        assert_eq!(detect_protocol(request), DetectedProtocol::Http);
    }

    #[test]
    fn test_detect_ssh_protocol() {
        assert_eq!(
            detect_protocol(b"SSH-2.0-OpenSSH_8.9"),
            DetectedProtocol::RawTcp
        );
    }

    #[test]
    fn test_detect_smtp_greeting() {
        assert_eq!(
            detect_protocol(b"220 mail.example.com ESMTP"),
            DetectedProtocol::RawTcp
        );
    }

    #[test]
    fn test_detect_single_byte() {
        assert_eq!(detect_protocol(&[0x00]), DetectedProtocol::RawTcp);
        assert_eq!(detect_protocol(&[0xFF]), DetectedProtocol::RawTcp);
        assert_eq!(detect_protocol(&[b'G']), DetectedProtocol::RawTcp);
    }
}
