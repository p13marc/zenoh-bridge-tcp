use super::*;

#[test]
fn test_parse_export_spec_valid() {
    let result = parse_export_spec("myservice/127.0.0.1:8080");
    assert!(result.is_ok());
    let (service, addr) = result.unwrap();
    assert_eq!(service, "myservice");
    assert_eq!(addr.to_string(), "127.0.0.1:8080");
}

#[test]
fn test_parse_export_spec_invalid_format() {
    let result = parse_export_spec("invalid");
    assert!(result.is_err());
}

#[test]
fn test_parse_export_spec_invalid_addr() {
    let result = parse_export_spec("myservice/invalid:addr");
    assert!(result.is_err());
}

#[test]
fn test_parse_export_spec_too_many_parts() {
    let result = parse_export_spec("service/addr/extra");
    assert!(result.is_err());
}

#[test]
fn test_parse_http_export_spec_valid() {
    let result = parse_http_export_spec("http-service/api.example.com/127.0.0.1:8080");
    assert!(result.is_ok());
    let (service, dns, addr) = result.unwrap();
    assert_eq!(service, "http-service");
    assert_eq!(dns, "api.example.com");
    assert_eq!(addr.to_string(), "127.0.0.1:8080");
}

#[test]
fn test_parse_http_export_spec_dns_normalization() {
    let result = parse_http_export_spec("http-service/Example.COM:80/127.0.0.1:8080");
    assert!(result.is_ok());
    let (service, dns, addr) = result.unwrap();
    assert_eq!(service, "http-service");
    assert_eq!(dns, "example.com"); // Normalized: lowercase + port 80 stripped
    assert_eq!(addr.to_string(), "127.0.0.1:8080");
}

#[test]
fn test_parse_http_export_spec_invalid_format() {
    let result = parse_http_export_spec("invalid");
    assert!(result.is_err());
}

#[test]
fn test_parse_http_export_spec_missing_dns() {
    let result = parse_http_export_spec("service/127.0.0.1:8080");
    assert!(result.is_err());
}

#[test]
fn test_parse_http_export_spec_invalid_addr() {
    let result = parse_http_export_spec("service/example.com/invalid:addr");
    assert!(result.is_err());
}

#[test]
fn test_parse_ws_export_spec_valid() {
    let result = parse_ws_export_spec("myws/ws://127.0.0.1:9000");
    assert!(result.is_ok());
    let (service, url) = result.unwrap();
    assert_eq!(service, "myws");
    assert_eq!(url, "ws://127.0.0.1:9000");
}

#[test]
fn test_parse_ws_export_spec_wss() {
    let result = parse_ws_export_spec("secure-ws/wss://example.com:443/path");
    assert!(result.is_ok());
    let (service, url) = result.unwrap();
    assert_eq!(service, "secure-ws");
    assert_eq!(url, "wss://example.com:443/path");
}

#[test]
fn test_parse_ws_export_spec_invalid_no_slash() {
    let result = parse_ws_export_spec("invalid");
    assert!(result.is_err());
}

#[test]
fn test_parse_ws_export_spec_invalid_not_ws() {
    let result = parse_ws_export_spec("myws/http://example.com");
    assert!(result.is_err());
}

#[test]
fn test_parse_ws_export_spec_empty_service() {
    let result = parse_ws_export_spec("/ws://example.com");
    assert!(result.is_err());
}

#[test]
fn test_parse_export_spec_empty_service_name() {
    let result = parse_export_spec("/127.0.0.1:8080");
    // Empty service name should succeed at parsing but is an edge case
    assert!(result.is_ok());
    let (service, _) = result.unwrap();
    assert_eq!(service, "");
}

#[test]
fn test_parse_export_spec_empty_string() {
    let result = parse_export_spec("");
    assert!(result.is_err());
}

#[test]
fn test_parse_export_spec_nested_service_name() {
    // "my/nested/service/127.0.0.1:8080" has 4 parts, should fail
    let result = parse_export_spec("my/nested/service/127.0.0.1:8080");
    assert!(
        result.is_err(),
        "Nested service names should be rejected by spec parser"
    );
}

#[test]
fn test_parse_export_spec_with_wildcards() {
    // Zenoh wildcards in service names: parsing succeeds but usage would be invalid
    let result = parse_export_spec("my*service/127.0.0.1:8080");
    assert!(
        result.is_ok(),
        "Parser accepts wildcards (validation is at Zenoh level)"
    );
}

#[test]
fn test_parse_http_export_spec_dns_port_443_stripped() {
    let result = parse_http_export_spec("svc/example.com:443/127.0.0.1:8080");
    assert!(result.is_ok());
    let (_, dns, _) = result.unwrap();
    assert_eq!(dns, "example.com", "Port 443 should be stripped from DNS");
}

#[test]
fn test_parse_http_export_spec_dns_custom_port_kept() {
    let result = parse_http_export_spec("svc/example.com:8443/127.0.0.1:8080");
    assert!(result.is_ok());
    let (_, dns, _) = result.unwrap();
    assert_eq!(dns, "example.com:8443", "Non-standard ports should be kept");
}

#[test]
fn test_parse_ws_export_spec_with_path() {
    let result = parse_ws_export_spec("myws/ws://127.0.0.1:9000/path/to/ws");
    assert!(result.is_ok());
    let (service, url) = result.unwrap();
    assert_eq!(service, "myws");
    assert_eq!(url, "ws://127.0.0.1:9000/path/to/ws");
}

#[tokio::test]
async fn test_cancellation_token_propagation() {
    use std::time::Duration;
    use tokio_util::sync::CancellationToken;

    let parent = CancellationToken::new();
    let child = parent.child_token();

    let task = tokio::spawn({
        let child = child.clone();
        async move {
            child.cancelled().await;
            true
        }
    });

    // Child should not be cancelled yet
    assert!(!child.is_cancelled());

    // Cancel parent
    parent.cancel();

    // Child task should complete
    let result = tokio::time::timeout(Duration::from_secs(1), task).await;
    assert!(result.is_ok());
    assert!(result.unwrap().unwrap());
}

// --- IPv6 address tests ---

#[test]
fn test_parse_export_spec_ipv6_loopback() {
    let result = parse_export_spec("svc/[::1]:8080");
    assert!(result.is_ok());
    let (service, addr) = result.unwrap();
    assert_eq!(service, "svc");
    assert_eq!(addr.to_string(), "[::1]:8080");
}

#[test]
fn test_parse_export_spec_ipv6_all_interfaces() {
    let result = parse_export_spec("svc/[::]:8080");
    assert!(result.is_ok());
    let (_, addr) = result.unwrap();
    assert_eq!(addr.to_string(), "[::]:8080");
}

#[test]
fn test_parse_http_export_spec_ipv6_backend() {
    let result = parse_http_export_spec("svc/api.example.com/[::1]:8080");
    assert!(result.is_ok());
    let (_, dns, addr) = result.unwrap();
    assert_eq!(dns, "api.example.com");
    assert_eq!(addr.to_string(), "[::1]:8080");
}

// --- WebSocket spec edge cases ---

#[test]
fn test_parse_ws_export_spec_wss_with_path() {
    let result = parse_ws_export_spec("myws/wss://example.com:443/ws/v2");
    assert!(result.is_ok());
    let (service, url) = result.unwrap();
    assert_eq!(service, "myws");
    assert_eq!(url, "wss://example.com:443/ws/v2");
}

#[test]
fn test_parse_ws_export_spec_plain_url_rejected() {
    let result = parse_ws_export_spec("svc/tcp://127.0.0.1:9000");
    assert!(result.is_err());
}

#[test]
fn test_parse_ws_export_spec_ftp_rejected() {
    let result = parse_ws_export_spec("svc/ftp://127.0.0.1:21");
    assert!(result.is_err());
}

// --- DNS normalization in HTTP export ---

#[test]
fn test_parse_http_export_spec_dns_mixed_case() {
    let result = parse_http_export_spec("svc/API.Example.COM/127.0.0.1:8080");
    assert!(result.is_ok());
    let (_, dns, _) = result.unwrap();
    assert_eq!(dns, "api.example.com");
}

#[test]
fn test_parse_http_export_spec_dns_with_subdomain() {
    let result = parse_http_export_spec("svc/deep.sub.domain.example.com/127.0.0.1:8080");
    assert!(result.is_ok());
    let (_, dns, _) = result.unwrap();
    assert_eq!(dns, "deep.sub.domain.example.com");
}
