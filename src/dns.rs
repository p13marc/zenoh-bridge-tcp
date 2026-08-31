//! DNS-name normalization for Zenoh routing keys.
//!
//! A `Host` header, TLS SNI name, or h2 `:authority` is normalized into the
//! canonical form used inside key expressions such as
//! `{service}/{dns}/tx/{client_id}`. Both the import routing paths and the
//! export side (`export::mod`, `spec`) depend on this being stable and
//! identical, so it lives in its own module rather than inside a parser that
//! may be swapped out.
//!
//! Routing keys are **host-only**: no port, no IPv6 brackets. A browser puts
//! the listener's port into `Host` (`api.local:8080`), SNI structurally cannot
//! carry a port at all, and `--backend @host` has no meaningful place for one —
//! the bare host is the single form every extraction path agrees on. (Until
//! 0.10 only the default 80/443 were collapsed, so the same vhost routed via
//! HTTPS/SNI but 502'd via plain HTTP on any non-default port.)

/// Normalize a DNS name for consistent routing.
///
/// This function:
/// 1. Converts to lowercase (DNS is case-insensitive)
/// 2. Strips IPv6 brackets (`[::1]` and `::1` must be the same key)
/// 3. Strips any `:port` (routing keys are host-only)
///
/// Examples:
/// - `"Example.COM"` -> `"example.com"`
/// - `"example.com:80"` -> `"example.com"`
/// - `"example.com:8080"` -> `"example.com"`
/// - `"[2001:db8::1]:9090"` -> `"2001:db8::1"`
/// - `"2001:db8::1"` -> `"2001:db8::1"`
pub fn normalize_dns(host: &str) -> String {
    let host = host.to_lowercase();

    // Bracketed IPv6 (`[v6]` or `[v6]:port`): the key is the bare literal.
    // Only the well-formed shape is unwrapped — the inner part must look like
    // an IPv6 literal (contains ':') and the tail must be empty or a valid
    // `:port`. Anything else (`[::1]garbage`, `[::1]:99999`, `[not.v6]`) is
    // returned as-is so the validators reject the '[' instead of this
    // function silently stripping what it does not understand.
    if let Some(rest) = host.strip_prefix('[')
        && let Some((inner, tail)) = rest.split_once(']')
        && inner.contains(':')
        && (tail.is_empty()
            || tail
                .strip_prefix(':')
                .is_some_and(|p| p.parse::<u16>().is_ok()))
    {
        return inner.to_string();
    }

    // IPv6 without brackets: multiple colons means it's an IPv6 address, not host:port
    let colon_count = host.chars().filter(|&c| c == ':').count();
    if colon_count > 1 {
        return host;
    }

    // Strip a trailing `:port` (only if it actually parses as one — a
    // non-numeric tail is left alone for the validators to reject).
    if let Some((name, port_str)) = host.split_once(':')
        && port_str.parse::<u16>().is_ok()
    {
        return name.to_string();
    }
    host
}

/// Whether `host` (as written in a spec) carries an explicit `:port`.
///
/// Used by the spec parsers to *reject* it with an actionable error instead of
/// silently stripping it: `@a.local:8443` and `@a.local:9443` would otherwise
/// merge into one backend key behind the operator's back.
pub fn has_explicit_port(host: &str) -> bool {
    if let Some(rest) = host.strip_prefix('[') {
        // Bracketed IPv6: a port can only follow the closing bracket.
        return rest
            .split_once(']')
            .and_then(|(_, tail)| tail.strip_prefix(':'))
            .is_some_and(|p| p.parse::<u16>().is_ok());
    }
    // A single colon with a valid port tail; more colons means bare IPv6.
    host.chars().filter(|&c| c == ':').count() == 1
        && host
            .split_once(':')
            .is_some_and(|(_, p)| p.parse::<u16>().is_ok())
}

/// Charset a routing key may use as a Zenoh key-expression segment.
///
/// Anything else — `*` and `$*` (live wildcards), `/` (segment injection),
/// `?`, `#`, whitespace, non-ASCII — would let a hostile `Host`/SNI reach
/// keyexpr machinery it must never steer. `:` is allowed solely for bare IPv6
/// literals. This is the single source of truth shared by the spec parsers
/// (operator side) and [`routing_key`] (client side); the two must stay
/// identical or registration and matching diverge.
pub fn is_valid_key_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ':')
}

/// Normalize and validate a **client-derived** host (Host header, SNI, h2
/// `:authority`) into a routing key. `Err` means the request is unroutable and
/// the caller should answer 400 (or close, where no HTTP is expressible).
pub fn routing_key(host: &str) -> anyhow::Result<String> {
    let dns = normalize_dns(host);
    if dns.is_empty() {
        return Err(anyhow::anyhow!("empty host"));
    }
    // A single colon surviving normalization is a port normalize_dns could
    // not parse (`host:99999`, trailing `host:`) — no hostname contains ':'
    // and every IPv6 literal has at least two. Without this check such tails
    // minted colon-bearing keys nothing could ever register.
    if dns.chars().filter(|&c| c == ':').count() == 1 {
        return Err(anyhow::anyhow!(
            "host carries an unparseable port or stray ':'"
        ));
    }
    if let Some(c) = dns.chars().find(|&c| !is_valid_key_char(c)) {
        return Err(anyhow::anyhow!(
            "host contains an invalid character {c:?} \
             (allowed: alphanumerics, '-', '_', '.', ':')"
        ));
    }
    Ok(dns)
}

/// Normalize and validate an **operator-written** host (`--backend '@host'`,
/// `--http-export 'svc/dns/…'`) into the key it registers.
///
/// Same rules as [`routing_key`] plus one difference: a client's explicit
/// `:port` is silently *stripped* (a browser puts the listener port into
/// `Host`), while an operator's is *rejected* with an actionable error —
/// silently stripping would merge `@a:8443` and `@a:9443` behind their back.
/// The single shared implementation is what keeps registration and matching
/// identical; the spec parsers must not roll their own.
pub fn spec_host_key(host: &str) -> anyhow::Result<String> {
    if has_explicit_port(host) {
        return Err(anyhow::anyhow!(
            "host '{host}' carries a port — routing keys are host-only \
             (a client's 'Host: {host}' routes by the bare name); drop the port"
        ));
    }
    routing_key(host)
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
    fn test_normalize_dns_strip_any_port() {
        // The original 0.9 bug: a browser sends `Host: api.local:8080` on a
        // non-default-port listener; the key must still be the bare host.
        assert_eq!(normalize_dns("example.com:8080"), "example.com");
        assert_eq!(normalize_dns("example.com:3000"), "example.com");
    }

    #[test]
    fn test_normalize_dns_combined() {
        assert_eq!(normalize_dns("Example.COM:80"), "example.com");
        assert_eq!(normalize_dns("API.Example.COM:443"), "api.example.com");
        assert_eq!(normalize_dns("Dev.Example.COM:8080"), "dev.example.com");
    }

    #[test]
    fn test_normalize_dns_numeric_port_parsing() {
        // Any valid u16 tail is a port; a non-numeric tail is not.
        assert_eq!(normalize_dns("host:80"), "host");
        assert_eq!(normalize_dns("host:443"), "host");
        assert_eq!(normalize_dns("host:8080"), "host");
        assert_eq!(normalize_dns("host:180"), "host");
        assert_eq!(normalize_dns("host:4430"), "host");
        // Out of u16 range: not a port, left for the validators.
        assert_eq!(normalize_dns("host:99999"), "host:99999");
        // No port at all
        assert_eq!(normalize_dns("example.com"), "example.com");
        // IPv6 bracket notation: brackets and port both stripped
        assert_eq!(normalize_dns("[::1]"), "::1");
        assert_eq!(normalize_dns("[::1]:80"), "::1");
        assert_eq!(normalize_dns("[::1]:8080"), "::1");
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
        assert_eq!(normalize_dns(":8080"), "");
    }

    #[test]
    fn test_normalize_dns_unicode_passthrough() {
        // Unicode is lowercased but otherwise passed through.
        //
        // NOTE: no routing path reaches keys with raw Unicode any more —
        // flowscope's `RequestHead::authority()` ASCII-folds and rejects
        // non-ASCII authorities upstream (F3), and both `routing_key` and the
        // spec parsers reject non-ASCII via `is_valid_key_char`. The helper
        // itself stays permissive: it normalizes, the validators decide.
        assert_eq!(normalize_dns("MÜNCHEN.de"), "münchen.de");
    }

    #[test]
    fn test_normalize_dns_only_port() {
        assert_eq!(normalize_dns(":443"), "");
    }

    #[test]
    fn test_normalize_dns_ipv6_bracket_port_443() {
        assert_eq!(normalize_dns("[::1]:443"), "::1");
    }

    #[test]
    fn test_normalize_dns_ipv6_bracket_custom_port() {
        assert_eq!(normalize_dns("[2001:db8::1]:9090"), "2001:db8::1");
    }

    #[test]
    fn test_has_explicit_port() {
        assert!(has_explicit_port("example.com:8080"));
        assert!(has_explicit_port("example.com:80"));
        assert!(has_explicit_port("[::1]:8080"));
        assert!(!has_explicit_port("example.com"));
        assert!(!has_explicit_port("[::1]"));
        assert!(!has_explicit_port("::1"));
        assert!(!has_explicit_port("2001:db8::1"));
        assert!(!has_explicit_port("example.com:notaport"));
        assert!(!has_explicit_port("example.com:99999"));
    }

    #[test]
    fn test_routing_key_valid() {
        assert_eq!(routing_key("API.Local:8080").unwrap(), "api.local");
        assert_eq!(routing_key("api.local").unwrap(), "api.local");
        assert_eq!(routing_key("[::1]:443").unwrap(), "::1");
        assert_eq!(routing_key("2001:db8::1").unwrap(), "2001:db8::1");
        assert_eq!(
            routing_key("my-api_v2.example.com").unwrap(),
            "my-api_v2.example.com"
        );
    }

    #[test]
    fn test_unparseable_ports_do_not_escape_validation() {
        // A ':' tail that fails u16 parse must not survive into a key —
        // 'host:99999' would mint a colon-bearing key nothing can register.
        assert!(routing_key("api.local:99999").is_err());
        assert!(routing_key("api.local:").is_err());
        assert!(routing_key("api.local:80o0").is_err());
        // Malformed bracket forms keep their '[' through normalize and are
        // rejected by the charset, never silently stripped to the inner part.
        assert_eq!(normalize_dns("[::1]:99999"), "[::1]:99999");
        assert_eq!(normalize_dns("[::1]garbage"), "[::1]garbage");
        assert_eq!(normalize_dns("[api.internal]"), "[api.internal]");
        assert!(routing_key("[::1]:99999").is_err());
        assert!(routing_key("[api.internal]").is_err());
        // Operator side: valid port -> the actionable "drop the port" error;
        // garbage port -> still an error, never a silently-registered key.
        assert!(
            spec_host_key("a.local:8443")
                .unwrap_err()
                .to_string()
                .contains("drop the port")
        );
        assert!(spec_host_key("a.local:99999").is_err());
        assert!(spec_host_key("[::1]:80o0").is_err());
        assert!(spec_host_key("").is_err());
        // The happy paths are unchanged.
        assert_eq!(spec_host_key("API.Local").unwrap(), "api.local");
        assert_eq!(spec_host_key("[::1]").unwrap(), "::1");
        assert_eq!(spec_host_key("2001:db8::1").unwrap(), "2001:db8::1");
    }

    #[test]
    fn test_routing_key_rejects_metacharacters() {
        // Each of these would otherwise reach Zenoh keyexpr machinery:
        // wildcards match every backend's liveliness token, '/' injects
        // key segments.
        for bad in [
            "*",
            "**",
            "$*",
            "a/b",
            "a b",
            "a?b",
            "a#b",
            "svc/../other",
            "",
        ] {
            assert!(routing_key(bad).is_err(), "expected rejection of {bad:?}");
        }
        // Non-ASCII (defense in depth behind flowscope's F3).
        assert!(routing_key("münchen.de").is_err());
    }
}
