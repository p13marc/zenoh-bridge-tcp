//! TLS/SNI parser for extracting Server Name Indication from TLS ClientHello
//!
//! This module provides functionality to parse TLS handshake messages and extract
//! the SNI (Server Name Indication) hostname for routing purposes.

use crate::config::BridgeConfig;
use crate::error::{BridgeError, Result};
use crate::http_parser::normalize_dns;
use tokio::io::AsyncReadExt;
use tokio::time::timeout;
use tracing::{debug, warn};

/// TLS Content Type: Handshake
const TLS_HANDSHAKE: u8 = 0x16;

/// TLS Handshake Type: ClientHello
const TLS_CLIENT_HELLO: u8 = 0x01;

/// Parsed TLS ClientHello information
#[derive(Debug, Clone)]
pub struct ParsedTlsClientHello {
    /// The normalized DNS name extracted from SNI
    pub dns: String,
    /// The complete buffered ClientHello that should be forwarded
    pub buffer: Vec<u8>,
}

/// Parse a TLS ClientHello from a TCP stream and extract SNI
///
/// This function:
/// 1. Reads the TLS record header
/// 2. Parses the ClientHello message
/// 3. Extracts the SNI extension
/// 4. Normalizes the hostname
/// 5. Returns both the hostname and the complete buffered handshake
///
/// # Arguments
/// * `stream` - A mutable reference to the TCP stream reader
/// * `config` - Bridge configuration with timeout and size limits
///
/// # Returns
/// * `Ok(ParsedTlsClientHello)` - Successfully parsed ClientHello with SNI
/// * `Err` - If parsing fails, timeout occurs, or SNI is missing
pub async fn parse_tls_client_hello_with_config<R>(
    stream: &mut R,
    config: &BridgeConfig,
) -> Result<ParsedTlsClientHello>
where
    R: AsyncReadExt + Unpin,
{
    // Buffer to accumulate the TLS record
    let mut buffer = Vec::with_capacity(4096);
    let mut temp_buf = vec![0u8; 4096];

    let max_handshake_size = config.max_header_size;
    let read_timeout = config.read_timeout;

    // Read with timeout to prevent hanging
    let read_result = timeout(read_timeout, async {
        // Read TLS record header (5 bytes)
        while buffer.len() < 5 {
            let n = stream.read(&mut temp_buf).await?;
            if n == 0 {
                return Err(BridgeError::TlsParse(
                    "Connection closed before TLS record header".to_string(),
                ));
            }
            buffer.extend_from_slice(&temp_buf[..n]);
        }

        // Parse TLS record header
        let content_type = buffer[0];
        if content_type != TLS_HANDSHAKE {
            return Err(BridgeError::TlsParse(format!(
                "Not a TLS handshake (content_type: 0x{:02x})",
                content_type
            )));
        }

        // Extract length (bytes 3-4, big-endian)
        let length = u16::from_be_bytes([buffer[3], buffer[4]]) as usize;

        // Read the rest of the handshake message (5 bytes header + length bytes)
        let total_size = 5 + length;
        if total_size > max_handshake_size {
            return Err(BridgeError::TlsParse(format!(
                "TLS handshake too large: {} bytes (max: {})",
                total_size, max_handshake_size
            )));
        }

        while buffer.len() < total_size {
            let n = stream.read(&mut temp_buf).await?;
            if n == 0 {
                return Err(BridgeError::TlsParse(
                    "Connection closed before complete TLS handshake".to_string(),
                ));
            }
            buffer.extend_from_slice(&temp_buf[..n]);
        }

        // Keep only the exact handshake record
        buffer.truncate(total_size);

        Ok(buffer)
    })
    .await;

    let buffer = match read_result {
        Ok(Ok(buf)) => buf,
        Ok(Err(e)) => return Err(e),
        Err(_) => return Err(BridgeError::Timeout("reading TLS ClientHello".to_string())),
    };

    // Parse the TLS handshake using tls-parser
    let dns = extract_sni_from_client_hello(&buffer)?;

    debug!(
        "Parsed TLS ClientHello with SNI: {} ({} bytes)",
        dns,
        buffer.len()
    );

    Ok(ParsedTlsClientHello {
        dns: normalize_dns(&dns),
        buffer,
    })
}

/// Parse a TLS ClientHello using default configuration
///
/// This is a convenience function that uses default timeout and size limits.
pub async fn parse_tls_client_hello<R>(stream: &mut R) -> Result<ParsedTlsClientHello>
where
    R: AsyncReadExt + Unpin,
{
    parse_tls_client_hello_with_config(stream, &BridgeConfig::default()).await
}

/// Validate an SNI hostname per RFC 6066 §3 and RFC 1035.
///
/// Requirements:
/// - Must be valid ASCII (RFC 6066: "byte string using ASCII encoding")
/// - Max 253 bytes (DNS hostname limit, RFC 1035)
/// - No trailing dot (RFC 6066)
/// - Each label: 1-63 bytes, alphanumeric + hyphens, no leading/trailing hyphen
fn validate_sni_hostname(raw: &[u8]) -> Result<String> {
    if !raw.is_ascii() {
        return Err(BridgeError::TlsParse(
            "SNI hostname contains non-ASCII bytes".to_string(),
        ));
    }

    let hostname = std::str::from_utf8(raw)
        .map_err(|_| BridgeError::TlsParse("SNI hostname is not valid UTF-8".to_string()))?;

    if hostname.is_empty() {
        return Err(BridgeError::TlsParse("SNI hostname is empty".to_string()));
    }

    if hostname.len() > 253 {
        return Err(BridgeError::TlsParse(format!(
            "SNI hostname too long: {} bytes (max 253)",
            hostname.len()
        )));
    }

    if hostname.ends_with('.') {
        return Err(BridgeError::TlsParse(
            "SNI hostname must not have trailing dot".to_string(),
        ));
    }

    for label in hostname.split('.') {
        if label.is_empty() || label.len() > 63 {
            return Err(BridgeError::TlsParse(format!(
                "SNI hostname label invalid length: '{}'",
                label
            )));
        }
        if !label
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        {
            return Err(BridgeError::TlsParse(format!(
                "SNI hostname label contains invalid characters: '{}'",
                label
            )));
        }
        if label.starts_with('-') || label.ends_with('-') {
            return Err(BridgeError::TlsParse(format!(
                "SNI hostname label must not start/end with hyphen: '{}'",
                label
            )));
        }
    }

    Ok(hostname.to_string())
}

/// Extract SNI hostname from TLS ClientHello buffer
///
/// Uses the tls-parser crate to parse the TLS handshake and extract SNI
fn extract_sni_from_client_hello(buffer: &[u8]) -> Result<String> {
    // Parse TLS plaintext record
    let (_, record) = tls_parser::parse_tls_plaintext(buffer)
        .map_err(|e| BridgeError::TlsParse(format!("Failed to parse TLS record: {:?}", e)))?;

    // Get the handshake messages
    for message in &record.msg {
        if let tls_parser::TlsMessage::Handshake(tls_parser::TlsMessageHandshake::ClientHello(
            client_hello,
        )) = message
        {
            // Look for SNI extension - ext is Option<&[u8]> containing raw extension data
            if let Some(ext_data) = client_hello.ext {
                // Parse all extensions from the raw bytes
                let mut remaining = ext_data;
                while !remaining.is_empty() {
                    match tls_parser::parse_tls_extension(remaining) {
                        Ok((rest, ext)) => {
                            if let tls_parser::TlsExtension::SNI(sni_list) = ext {
                                // Get the first hostname from SNI
                                for sni in sni_list {
                                    if let (tls_parser::SNIType::HostName, name) = sni {
                                        let hostname = validate_sni_hostname(name)?;
                                        debug!("Found SNI hostname: {}", hostname);
                                        return Ok(hostname);
                                    }
                                }
                            }
                            remaining = rest;
                        }
                        Err(_) => break,
                    }
                }
            }

            // No SNI extension found
            warn!("TLS ClientHello has no SNI extension");
            return Err(BridgeError::TlsParse(
                "TLS ClientHello missing SNI extension".to_string(),
            ));
        }
    }

    Err(BridgeError::TlsParse(
        "No ClientHello found in TLS record".to_string(),
    ))
}

/// Detect if the buffer starts with a TLS handshake
///
/// Returns true if the first bytes look like a TLS ClientHello
pub fn is_tls_handshake(buffer: &[u8]) -> bool {
    if buffer.len() < 6 {
        return false;
    }

    // Check for TLS handshake content type (0x16)
    if buffer[0] != TLS_HANDSHAKE {
        return false;
    }

    // Check TLS version (should be 3.x)
    if buffer[1] != 0x03 {
        return false;
    }

    // Check handshake type (should be ClientHello 0x01)
    // The handshake type is at byte 5 (after 5-byte record header)
    if buffer.len() > 5 && buffer[5] != TLS_CLIENT_HELLO {
        return false;
    }

    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_tls_handshake_valid() {
        // TLS 1.2 ClientHello
        let tls_12 = vec![
            0x16, // Handshake
            0x03, 0x03, // TLS 1.2
            0x00, 0x05, // Length: 5 bytes
            0x01, // ClientHello
            0x00, 0x00, 0x00, 0x00, // Placeholder
        ];
        assert!(is_tls_handshake(&tls_12));

        // TLS 1.3 ClientHello
        let tls_13 = vec![
            0x16, // Handshake
            0x03, 0x01, // TLS 1.0 (legacy)
            0x00, 0x05, // Length
            0x01, // ClientHello
            0x00, 0x00, 0x00, 0x00,
        ];
        assert!(is_tls_handshake(&tls_13));
    }

    #[test]
    fn test_is_tls_handshake_http() {
        // HTTP request
        let http = b"GET / HTTP/1.1\r\nHost: example.com\r\n\r\n";
        assert!(!is_tls_handshake(http));
    }

    #[test]
    fn test_is_tls_handshake_short_buffer() {
        let short = vec![0x16, 0x03];
        assert!(!is_tls_handshake(&short));
    }

    #[test]
    fn test_is_tls_handshake_wrong_content_type() {
        // Application Data (0x17) instead of Handshake (0x16)
        let wrong = vec![0x17, 0x03, 0x03, 0x00, 0x05, 0x01];
        assert!(!is_tls_handshake(&wrong));
    }

    #[test]
    fn test_is_tls_handshake_wrong_version() {
        // Invalid TLS version
        let wrong = vec![0x16, 0x02, 0x03, 0x00, 0x05, 0x01];
        assert!(!is_tls_handshake(&wrong));
    }

    #[test]
    fn test_is_tls_handshake_not_client_hello() {
        // ServerHello (0x02) instead of ClientHello (0x01)
        let wrong = vec![0x16, 0x03, 0x03, 0x00, 0x05, 0x02];
        assert!(!is_tls_handshake(&wrong));
    }

    /// Build a minimal TLS 1.2 ClientHello with an SNI extension.
    fn build_client_hello_with_sni(hostname: &str) -> Vec<u8> {
        let name_bytes = hostname.as_bytes();

        // SNI extension payload:
        //   extension_type(2) + extension_length(2) + sni_list_length(2) +
        //   name_type(1) + name_length(2) + name
        let sni_ext_len = 2 + 1 + 2 + name_bytes.len();
        let ext_len = sni_ext_len;
        let sni_ext: Vec<u8> = [
            &[0x00, 0x00],                                  // Extension type: SNI
            &((ext_len as u16).to_be_bytes())[..],          // Extension data length
            &(((ext_len - 2) as u16).to_be_bytes())[..],    // SNI list length
            &[0x00],                                        // Name type: host_name
            &((name_bytes.len() as u16).to_be_bytes())[..], // Name length
            name_bytes,                                     // The hostname
        ]
        .concat();

        let extensions_total_len = sni_ext.len();

        // ClientHello body (after handshake type + length):
        //   version(2) + random(32) + session_id_len(1)=0 +
        //   cipher_suites_len(2) + one suite(2) +
        //   compression_len(1) + null compression(1) +
        //   extensions_len(2) + extensions
        let client_hello_body: Vec<u8> = [
            &[0x03, 0x03],     // TLS 1.2
            &[0x00u8; 32][..], // Random (32 zero bytes)
            &[0x00],           // Session ID length: 0
            &[0x00, 0x02],     // Cipher suites length: 2
            &[0x00, 0xFF],     // Cipher suite: TLS_EMPTY_RENEGOTIATION_INFO_SCSV
            &[0x01, 0x00],     // Compression methods: 1, null
            &((extensions_total_len as u16).to_be_bytes())[..],
            &sni_ext,
        ]
        .concat();

        // Handshake message:
        //   type(1) + length(3) + body
        let hs_len = client_hello_body.len();
        let mut handshake = Vec::with_capacity(4 + hs_len);
        handshake.push(0x01); // ClientHello
        handshake.extend_from_slice(&[0x00, ((hs_len >> 8) & 0xFF) as u8, (hs_len & 0xFF) as u8]);
        handshake.extend_from_slice(&client_hello_body);

        // TLS record:
        //   content_type(1) + version(2) + length(2) + handshake
        let record_len = handshake.len();
        [
            &[0x16, 0x03, 0x01], // Handshake, TLS 1.0 (legacy)
            &((record_len as u16).to_be_bytes())[..],
            &handshake,
        ]
        .concat()
    }

    #[test]
    fn test_extract_sni_from_valid_client_hello() {
        let record = build_client_hello_with_sni("example.com");
        let sni = extract_sni_from_client_hello(&record);
        assert!(sni.is_ok());
        assert_eq!(sni.unwrap(), "example.com");
    }

    #[test]
    fn test_extract_sni_different_hostnames() {
        for hostname in &["api.test.com", "localhost", "my-service.internal"] {
            let record = build_client_hello_with_sni(hostname);
            let sni = extract_sni_from_client_hello(&record).unwrap();
            assert_eq!(sni, *hostname);
        }
    }

    #[test]
    fn test_extract_sni_from_non_tls_data() {
        let http = b"GET / HTTP/1.1\r\nHost: example.com\r\n\r\n";
        let result = extract_sni_from_client_hello(http);
        assert!(result.is_err());
    }

    // --- SNI hostname validation tests ---

    #[test]
    fn test_validate_sni_valid_hostnames() {
        assert_eq!(
            validate_sni_hostname(b"example.com").unwrap(),
            "example.com"
        );
        assert_eq!(
            validate_sni_hostname(b"api.test.com").unwrap(),
            "api.test.com"
        );
        assert_eq!(
            validate_sni_hostname(b"my-service.internal").unwrap(),
            "my-service.internal"
        );
        assert_eq!(validate_sni_hostname(b"localhost").unwrap(), "localhost");
        // Punycode A-label (internationalized domain)
        assert_eq!(
            validate_sni_hostname(b"xn--nxasmq6b.com").unwrap(),
            "xn--nxasmq6b.com"
        );
        // Single-char labels
        assert_eq!(validate_sni_hostname(b"a.b.c").unwrap(), "a.b.c");
    }

    #[test]
    fn test_validate_sni_empty() {
        assert!(validate_sni_hostname(b"").is_err());
    }

    #[test]
    fn test_validate_sni_too_long() {
        // 254 bytes = too long (max 253)
        let long =
            "a".repeat(63) + "." + &"b".repeat(63) + "." + &"c".repeat(63) + "." + &"d".repeat(62);
        assert_eq!(long.len(), 254);
        assert!(validate_sni_hostname(long.as_bytes()).is_err());

        // 253 bytes = OK
        let ok =
            "a".repeat(63) + "." + &"b".repeat(63) + "." + &"c".repeat(63) + "." + &"d".repeat(61);
        assert_eq!(ok.len(), 253);
        assert!(validate_sni_hostname(ok.as_bytes()).is_ok());
    }

    #[test]
    fn test_validate_sni_trailing_dot() {
        assert!(validate_sni_hostname(b"example.com.").is_err());
    }

    #[test]
    fn test_validate_sni_label_too_long() {
        let long_label = "a".repeat(64) + ".com";
        assert!(validate_sni_hostname(long_label.as_bytes()).is_err());

        let ok_label = "a".repeat(63) + ".com";
        assert!(validate_sni_hostname(ok_label.as_bytes()).is_ok());
    }

    #[test]
    fn test_validate_sni_empty_label() {
        // Double dot = empty label
        assert!(validate_sni_hostname(b"example..com").is_err());
    }

    #[test]
    fn test_validate_sni_hyphen_boundaries() {
        assert!(validate_sni_hostname(b"-example.com").is_err());
        assert!(validate_sni_hostname(b"example-.com").is_err());
        assert!(validate_sni_hostname(b"exam-ple.com").is_ok());
    }

    #[test]
    fn test_validate_sni_invalid_characters() {
        assert!(validate_sni_hostname(b"example.com:443").is_err()); // port
        assert!(validate_sni_hostname(b"example.com/path").is_err()); // path
        assert!(validate_sni_hostname(b"exam ple.com").is_err()); // space
        assert!(validate_sni_hostname(b"example_host.com").is_err()); // underscore
    }

    #[test]
    fn test_validate_sni_non_ascii() {
        assert!(validate_sni_hostname(&[0xC3, 0xA9, 0x2E, 0x63, 0x6F, 0x6D]).is_err()); // "é.com" in UTF-8
        assert!(validate_sni_hostname(b"example\x00.com").is_err()); // null byte
        assert!(validate_sni_hostname(&[0xFF, 0xFE]).is_err()); // garbage bytes
    }

    #[test]
    fn test_extract_sni_rejects_invalid_hostname_in_client_hello() {
        // Build a ClientHello with a hostname containing invalid characters
        let record = build_client_hello_with_sni("exam ple.com");
        let result = extract_sni_from_client_hello(&record);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("invalid characters")
        );
    }

    // --- Additional SNI validation tests ---

    #[test]
    fn test_validate_sni_numeric_labels() {
        assert_eq!(
            validate_sni_hostname(b"123.456.789").unwrap(),
            "123.456.789"
        );
    }

    #[test]
    fn test_validate_sni_single_character_hostname() {
        assert_eq!(validate_sni_hostname(b"x").unwrap(), "x");
    }

    #[test]
    fn test_validate_sni_max_label_length_boundary() {
        // Exactly 63 chars = OK
        let label = "a".repeat(63);
        assert!(validate_sni_hostname(label.as_bytes()).is_ok());
    }

    #[test]
    fn test_validate_sni_hyphen_only_label() {
        // A label that is just "-" should fail (leading and trailing hyphen)
        assert!(validate_sni_hostname(b"-.com").is_err());
    }

    #[test]
    fn test_validate_sni_leading_dot() {
        assert!(validate_sni_hostname(b".example.com").is_err());
    }

    #[test]
    fn test_validate_sni_multiple_hyphens_ok() {
        assert_eq!(
            validate_sni_hostname(b"a--b.example.com").unwrap(),
            "a--b.example.com"
        );
    }

    // --- is_tls_handshake edge cases ---

    #[test]
    fn test_is_tls_handshake_exact_minimum_bytes() {
        // Exactly 6 bytes: handshake record type, TLS 1.2, length, ClientHello
        assert!(is_tls_handshake(&[0x16, 0x03, 0x03, 0x00, 0x01, 0x01]));
    }

    #[test]
    fn test_is_tls_handshake_five_bytes_not_enough() {
        assert!(!is_tls_handshake(&[0x16, 0x03, 0x03, 0x00, 0x01]));
    }

    #[test]
    fn test_is_tls_handshake_ssl30() {
        // SSL 3.0 (version 0x03, 0x00) — still detected
        assert!(is_tls_handshake(&[0x16, 0x03, 0x00, 0x00, 0x01, 0x01]));
    }

    #[test]
    fn test_is_tls_handshake_alert_record_rejected() {
        // Record type 0x15 is TLS Alert, not handshake
        assert!(!is_tls_handshake(&[0x15, 0x03, 0x03, 0x00, 0x01, 0x01]));
    }

    // --- extract_sni_from_client_hello with various hostnames ---

    #[test]
    fn test_extract_sni_deeply_nested_hostname() {
        let hostname = "a.b.c.d.e.f.g.h.example.com";
        let record = build_client_hello_with_sni(hostname);
        let sni = extract_sni_from_client_hello(&record).unwrap();
        assert_eq!(sni, hostname);
    }

    #[test]
    fn test_extract_sni_short_hostname() {
        let record = build_client_hello_with_sni("a");
        let sni = extract_sni_from_client_hello(&record).unwrap();
        assert_eq!(sni, "a");
    }

    #[test]
    fn test_extract_sni_truncated_record_fails() {
        let record = build_client_hello_with_sni("example.com");
        // Truncate to just the TLS header
        let truncated = &record[..10];
        let result = extract_sni_from_client_hello(truncated);
        assert!(result.is_err());
    }

    #[test]
    fn test_extract_sni_empty_data() {
        let result = extract_sni_from_client_hello(b"");
        assert!(result.is_err());
    }

    // --- Validate error messages are informative ---

    #[test]
    fn test_validate_sni_error_messages() {
        let err = validate_sni_hostname(b"").unwrap_err().to_string();
        assert!(err.contains("empty"), "Error should mention empty: {}", err);

        let long = "a".repeat(254);
        let err = validate_sni_hostname(long.as_bytes())
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("253"),
            "Error should mention 253 limit: {}",
            err
        );

        let err = validate_sni_hostname(b"example.com.")
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("trailing dot"),
            "Error should mention trailing dot: {}",
            err
        );

        let err = validate_sni_hostname(b"-bad.com").unwrap_err().to_string();
        assert!(
            err.contains("hyphen"),
            "Error should mention hyphen: {}",
            err
        );
    }
}
