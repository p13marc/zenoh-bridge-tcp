//! TLS/SNI parser for extracting Server Name Indication from TLS ClientHello
//!
//! This module provides functionality to parse TLS handshake messages and extract
//! the SNI (Server Name Indication) hostname for routing purposes.

use anyhow::{anyhow, Result};
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::time::timeout;
use tracing::{debug, warn};

use crate::http_parser::normalize_dns;

/// Maximum size for TLS ClientHello message (16KB)
const MAX_TLS_HANDSHAKE_SIZE: usize = 16 * 1024;

/// Timeout for reading TLS ClientHello
const READ_TIMEOUT: Duration = Duration::from_secs(10);

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
///
/// # Returns
/// * `Ok(ParsedTlsClientHello)` - Successfully parsed ClientHello with SNI
/// * `Err` - If parsing fails, timeout occurs, or SNI is missing
pub async fn parse_tls_client_hello<R>(stream: &mut R) -> Result<ParsedTlsClientHello>
where
    R: AsyncReadExt + Unpin,
{
    // Buffer to accumulate the TLS record
    let mut buffer = Vec::with_capacity(4096);
    let mut temp_buf = vec![0u8; 4096];

    // Read with timeout to prevent hanging
    let read_result = timeout(READ_TIMEOUT, async {
        // Read TLS record header (5 bytes)
        while buffer.len() < 5 {
            let n = stream.read(&mut temp_buf).await?;
            if n == 0 {
                return Err(anyhow!("Connection closed before TLS record header"));
            }
            buffer.extend_from_slice(&temp_buf[..n]);
        }

        // Parse TLS record header
        let content_type = buffer[0];
        if content_type != TLS_HANDSHAKE {
            return Err(anyhow!(
                "Not a TLS handshake (content_type: 0x{:02x})",
                content_type
            ));
        }

        // Extract length (bytes 3-4, big-endian)
        let length = u16::from_be_bytes([buffer[3], buffer[4]]) as usize;

        if length > MAX_TLS_HANDSHAKE_SIZE {
            return Err(anyhow!(
                "TLS handshake too large: {} bytes (max: {})",
                length,
                MAX_TLS_HANDSHAKE_SIZE
            ));
        }

        // Read the rest of the handshake message (5 bytes header + length bytes)
        let total_size = 5 + length;
        while buffer.len() < total_size {
            if buffer.len() >= MAX_TLS_HANDSHAKE_SIZE {
                return Err(anyhow!("TLS handshake exceeds maximum size"));
            }

            let n = stream.read(&mut temp_buf).await?;
            if n == 0 {
                return Err(anyhow!("Connection closed before complete TLS handshake"));
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
        Err(_) => return Err(anyhow!("Timeout reading TLS ClientHello")),
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

/// Extract SNI hostname from TLS ClientHello buffer
///
/// Uses the tls-parser crate to parse the TLS handshake and extract SNI
fn extract_sni_from_client_hello(buffer: &[u8]) -> Result<String> {
    // Parse TLS plaintext record
    let (_, record) = tls_parser::parse_tls_plaintext(buffer)
        .map_err(|e| anyhow!("Failed to parse TLS record: {:?}", e))?;

    // Get the handshake messages
    for message in &record.msg {
        if let tls_parser::TlsMessage::Handshake(handshake) = message {
            if let tls_parser::TlsMessageHandshake::ClientHello(client_hello) = handshake {
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
                                            let hostname =
                                                String::from_utf8_lossy(name).to_string();
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
                return Err(anyhow!("TLS ClientHello missing SNI extension"));
            }
        }
    }

    Err(anyhow!("No ClientHello found in TLS record"))
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

    // Note: Full ClientHello parsing tests require valid TLS handshake data
    // which is complex to construct manually. Integration tests with real
    // TLS connections will validate the full parsing logic.
}
