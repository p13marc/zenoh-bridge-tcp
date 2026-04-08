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
    let result = parse_import_spec("/127.0.0.1:8080");
    assert!(result.is_ok());
    let (service, _) = result.unwrap();
    assert_eq!(service, "");
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
