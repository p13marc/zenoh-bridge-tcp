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
    b"GET ", b"POST ", b"PUT ", b"DELETE ", b"HEAD ", b"OPTIONS ", b"PATCH ", b"CONNECT ",
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
        assert_eq!(
            detect_protocol(b"HELLO server"),
            DetectedProtocol::RawTcp
        );
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
}
