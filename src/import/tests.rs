use super::*;

#[test]
fn test_parse_import_spec_valid() {
    let result = parse_import_spec("myservice/127.0.0.1:8080");
    assert!(result.is_ok());
    let (service, addr) = result.unwrap();
    assert_eq!(service, "myservice");
    assert_eq!(addr.to_string(), "127.0.0.1:8080");
}

#[test]
fn test_parse_import_spec_invalid_format() {
    let result = parse_import_spec("invalid");
    assert!(result.is_err());
}

#[test]
fn test_parse_import_spec_invalid_addr() {
    let result = parse_import_spec("myservice/invalid:addr");
    assert!(result.is_err());
}

#[test]
fn test_parse_import_spec_too_many_parts() {
    let result = parse_import_spec("service/addr/extra");
    assert!(result.is_err());
}

#[test]
fn test_parse_import_spec_empty_service_name() {
    // Empty and wildcard service names are now rejected (F2).
    assert!(parse_import_spec("/127.0.0.1:8080").is_err());
    assert!(parse_import_spec("*/127.0.0.1:8080").is_err());
}

#[test]
fn test_parse_import_spec_empty_string() {
    let result = parse_import_spec("");
    assert!(result.is_err());
}

#[test]
fn test_parse_import_spec_nested_service_name() {
    let result = parse_import_spec("my/nested/service/127.0.0.1:8080");
    assert!(
        result.is_err(),
        "Nested service names should be rejected by spec parser"
    );
}

#[test]
fn test_parse_import_spec_ipv4_all_interfaces() {
    let result = parse_import_spec("myservice/0.0.0.0:8080");
    assert!(result.is_ok());
    let (_, addr) = result.unwrap();
    assert_eq!(addr.to_string(), "0.0.0.0:8080");
}

#[test]
fn test_client_ids_are_unique() {
    let id1 = format!("client_{}", uuid::Uuid::new_v4().as_simple());
    let id2 = format!("client_{}", uuid::Uuid::new_v4().as_simple());
    assert_ne!(id1, id2);
    // Verify format is valid for Zenoh key expressions (no slashes, wildcards)
    assert!(!id1.contains('/'));
    assert!(!id1.contains('*'));
    assert!(!id1.contains('?'));
}

// --- IPv6 import specs ---

#[test]
fn test_parse_import_spec_ipv6_loopback() {
    let result = parse_import_spec("svc/[::1]:8080");
    assert!(result.is_ok());
    let (_, addr) = result.unwrap();
    assert_eq!(addr.to_string(), "[::1]:8080");
}

#[test]
fn test_parse_import_spec_ipv6_all_interfaces() {
    let result = parse_import_spec("svc/[::]:8080");
    assert!(result.is_ok());
    let (_, addr) = result.unwrap();
    assert_eq!(addr.to_string(), "[::]:8080");
}

// --- Edge cases ---

#[test]
fn test_parse_import_spec_high_port() {
    let result = parse_import_spec("svc/127.0.0.1:65535");
    assert!(result.is_ok());
    let (_, addr) = result.unwrap();
    assert_eq!(addr.port(), 65535);
}

#[test]
fn test_parse_import_spec_port_zero() {
    let result = parse_import_spec("svc/127.0.0.1:0");
    assert!(result.is_ok());
    let (_, addr) = result.unwrap();
    assert_eq!(addr.port(), 0);
}

#[test]
fn test_parse_import_spec_slash_only() {
    let result = parse_import_spec("/");
    // Second part is empty -> invalid address
    assert!(result.is_err());
}

// --- drain_tasks tests ---

#[tokio::test]
async fn test_drain_tasks_empty_set() {
    let mut tasks = JoinSet::new();
    drain_tasks(&mut tasks, "test-svc", Duration::from_secs(1)).await;
    assert!(tasks.is_empty());
}

#[tokio::test]
async fn test_drain_tasks_all_complete() {
    let mut tasks = JoinSet::new();
    tasks.spawn(async {});
    tasks.spawn(async {});
    drain_tasks(&mut tasks, "test-svc", Duration::from_secs(1)).await;
    assert!(tasks.is_empty());
}

#[tokio::test]
async fn test_drain_tasks_timeout_aborts() {
    let mut tasks = JoinSet::new();
    tasks.spawn(async {
        // Task that never completes on its own
        tokio::time::sleep(Duration::from_secs(60)).await;
    });
    // Very short timeout
    drain_tasks(&mut tasks, "test-svc", Duration::from_millis(50)).await;
    // After abort_all, we need to reap the aborted tasks
    while tasks.join_next().await.is_some() {}
    assert!(tasks.is_empty());
}

/// The backend resolver's precedence: an @host token wins, the service-level
/// default token catches the rest, neither refuses. Also pins the non-collision
/// between a host literally named "available" (3-segment token) and the
/// 2-segment default token.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_resolve_backend_precedence() {
    use super::connection::{BackendRoute, resolve_backend};

    /// Retry until the resolver returns `want` — token visibility across two
    /// peer sessions depends on scouting/propagation, which has no fixed bound.
    async fn assert_resolves(
        session: &zenoh::Session,
        svc: &str,
        dns: Option<&str>,
        config: &crate::config::BridgeConfig,
        want: BackendRoute,
    ) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            let got = resolve_backend(session, svc, dns, config).await.unwrap();
            if got == want {
                return;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "resolve_backend({svc}, {dns:?}) stuck at {got:?}, wanted {want:?}"
            );
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }

    let declarer = zenoh::open(zenoh::Config::default()).await.unwrap();
    let resolver = zenoh::open(zenoh::Config::default()).await.unwrap();
    let config = crate::config::BridgeConfig::default();
    let svc = format!("resolver_test_{}", uuid::Uuid::new_v4().as_simple());

    // Nothing declared: refuse, with or without a hostname.
    assert_resolves(&resolver, &svc, Some("h.test"), &config, BackendRoute::Unavailable).await;
    assert_resolves(&resolver, &svc, None, &config, BackendRoute::Unavailable).await;

    // Default token only: everything routes to the default backend.
    let _default_token = declarer
        .liveliness()
        .declare_token(format!("{svc}/available"))
        .await
        .unwrap();
    assert_resolves(&resolver, &svc, Some("h.test"), &config, BackendRoute::Default).await;
    assert_resolves(&resolver, &svc, None, &config, BackendRoute::Default).await;

    // Host token added: that host resolves Host, others still Default.
    let _host_token = declarer
        .liveliness()
        .declare_token(format!("{svc}/h.test/available"))
        .await
        .unwrap();
    assert_resolves(&resolver, &svc, Some("h.test"), &config, BackendRoute::Host).await;
    assert_resolves(&resolver, &svc, Some("other.test"), &config, BackendRoute::Default).await;

    // A host literally named "available" declares the 3-segment
    // {svc}/available/available and must not be confused with the default.
    let svc2 = format!("resolver_test_{}", uuid::Uuid::new_v4().as_simple());
    let _avail_host_token = declarer
        .liveliness()
        .declare_token(format!("{svc2}/available/available"))
        .await
        .unwrap();
    assert_resolves(&resolver, &svc2, Some("available"), &config, BackendRoute::Host).await;
    // No default token for svc2: an unmatched host refuses. The positive
    // probe above proves the sessions see svc2's tokens, so this cannot pass
    // by mere non-discovery.
    assert_resolves(&resolver, &svc2, Some("other.test"), &config, BackendRoute::Unavailable)
        .await;
}
