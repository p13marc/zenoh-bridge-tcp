//! DNS-name normalization for Zenoh routing keys.
//!
//! A `Host` header or TLS SNI name is normalized into the canonical form used
//! inside key expressions such as `{service}/{dns}/tx/{client_id}`. Both the
//! import routing paths and the export side (`export::mod`) depend on this being
//! stable and identical, so it lives in its own module rather than inside a
//! parser that may be swapped out.

/// Normalize a DNS name for consistent routing.
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

    // IPv6 without brackets: multiple colons means it's an IPv6 address, not host:port
    let colon_count = host.chars().filter(|&c| c == ':').count();
    if colon_count > 1 && !host.starts_with('[') {
        return host;
    }

    // Strip default ports using proper port parsing
    if let Some(colon_pos) = host.rfind(':') {
        let port_str = &host[colon_pos + 1..];
        if let Ok(port) = port_str.parse::<u16>()
            && (port == 80 || port == 443)
        {
            return host[..colon_pos].to_string();
        }
    }
    host
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
    fn test_normalize_dns_numeric_port_parsing() {
        // Ensure only actual port 80/443 are stripped
        assert_eq!(normalize_dns("host:80"), "host");
        assert_eq!(normalize_dns("host:443"), "host");
        assert_eq!(normalize_dns("host:8080"), "host:8080");
        assert_eq!(normalize_dns("host:180"), "host:180");
        assert_eq!(normalize_dns("host:4430"), "host:4430");
        // No port at all
        assert_eq!(normalize_dns("example.com"), "example.com");
        // IPv6 with port (bracket notation)
        assert_eq!(normalize_dns("[::1]:80"), "[::1]");
        assert_eq!(normalize_dns("[::1]:8080"), "[::1]:8080");
        // IPv6 without brackets: must not strip address octets as "port"
        assert_eq!(normalize_dns("::1"), "::1");
        assert_eq!(normalize_dns("2001:db8::1"), "2001:db8::1");
        assert_eq!(normalize_dns("::ffff:127.0.0.1"), "::ffff:127.0.0.1");
    }

    #[test]
    fn test_normalize_dns_empty_string() {
        assert_eq!(normalize_dns(""), "");
    }

    #[test]
    fn test_normalize_dns_port_only() {
        assert_eq!(normalize_dns(":80"), "");
        assert_eq!(normalize_dns(":8080"), ":8080");
    }

    #[test]
    fn test_normalize_dns_unicode_passthrough() {
        // Unicode is lowercased but otherwise passed through.
        //
        // NOTE: the import HTTP routing path no longer reaches this with raw
        // Unicode — `RequestHead::authority()` (flowscope) ASCII-folds and
        // rejects non-ASCII authorities upstream (F3). This helper keeps its
        // permissive behavior for the export side, where the DNS label comes
        // from an operator-provided spec, not an attacker-controlled Host.
        assert_eq!(normalize_dns("MÜNCHEN.de"), "münchen.de");
    }

    #[test]
    fn test_normalize_dns_only_port() {
        assert_eq!(normalize_dns(":443"), "");
    }

    #[test]
    fn test_normalize_dns_ipv6_bracket_port_443() {
        assert_eq!(normalize_dns("[::1]:443"), "[::1]");
    }

    #[test]
    fn test_normalize_dns_ipv6_bracket_custom_port() {
        assert_eq!(normalize_dns("[2001:db8::1]:9090"), "[2001:db8::1]:9090");
    }
}
