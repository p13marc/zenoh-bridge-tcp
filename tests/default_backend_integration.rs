//! Integration tests for the default (plain) backend serving host-routed traffic.
//!
//! A `--backend 'svc/addr'` with no `@host` is the service's catch-all: it
//! registers `{service}/available` and listeners route to it any connection
//! whose hostname no `@host` backend claims. These tests pin the token
//! registration, the fallback on every listener plane, `@host` precedence,
//! and the fast-502 when no backend of any kind is announced.

mod common;

use common::unique_service_name;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;
use zenoh::config::Config;

/// Collect the key expressions of all live tokens matching `key`.
async fn live_tokens(session: &zenoh::Session, key: &str) -> Vec<String> {
    let replies = session.liveliness().get(key).await.unwrap();
    let mut keys = Vec::new();
    while let Ok(Ok(reply)) =
        tokio::time::timeout(Duration::from_secs(2), replies.recv_async()).await
    {
        if let Ok(sample) = reply.into_result() {
            keys.push(sample.key_expr().as_str().to_string());
        }
    }
    keys
}

/// A plain backend declares the 2-segment default token `{service}/available`;
/// an `@host` backend declares only the 3-segment `{service}/{host}/available`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_plain_backend_declares_default_token() {
    let _ = tracing_subscriber::fmt::try_init();
    let shutdown_token = CancellationToken::new();
    let config = Arc::new(zenoh_bridge_tcp::config::BridgeConfig::default());

    let service = unique_service_name("defback_token");
    let session1 = Arc::new(zenoh::open(Config::default()).await.unwrap());
    let session2 = Arc::new(zenoh::open(Config::default()).await.unwrap());

    // Plain export — the backend address does not need to be reachable for
    // token declaration (connections are lazy).
    let export_spec = format!("{}/127.0.0.1:1", service);
    let session1_clone = session1.clone();
    let token_clone = shutdown_token.child_token();
    let config_clone = config.clone();
    let export_task = tokio::spawn(async move {
        let _ = zenoh_bridge_tcp::export::run_export_mode(
            session1_clone,
            &export_spec,
            config_clone,
            token_clone,
        )
        .await;
    });

    sleep(Duration::from_secs(1)).await;

    let default_key = format!("{}/available", service);
    let tokens = live_tokens(&session2, &default_key).await;
    assert_eq!(
        tokens,
        vec![default_key.clone()],
        "plain backend must declare the service-level default token"
    );

    // No host-scoped token should exist for a plain backend.
    let host_tokens = live_tokens(&session2, &format!("{}/*/available", service)).await;
    assert!(
        host_tokens.is_empty(),
        "plain backend must not declare host-scoped availability, got {host_tokens:?}"
    );

    shutdown_token.cancel();
    let _ = export_task.await;

    // An @host export declares only the 3-segment token.
    let service = unique_service_name("defback_token_host");
    let shutdown_token = CancellationToken::new();
    let export_spec = format!("{}/api.example.com/127.0.0.1:1", service);
    let session1_clone = session1.clone();
    let token_clone = shutdown_token.child_token();
    let config_clone = config.clone();
    let export_task = tokio::spawn(async move {
        let _ = zenoh_bridge_tcp::export::run_http_export_mode(
            session1_clone,
            &export_spec,
            config_clone,
            token_clone,
        )
        .await;
    });

    sleep(Duration::from_secs(1)).await;

    let host_key = format!("{}/api.example.com/available", service);
    let tokens = live_tokens(&session2, &host_key).await;
    assert_eq!(
        tokens,
        vec![host_key],
        "@host backend must declare the host-scoped token"
    );
    let default_tokens = live_tokens(&session2, &format!("{}/available", service)).await;
    assert!(
        default_tokens.is_empty(),
        "@host backend must not declare the default token, got {default_tokens:?}"
    );

    shutdown_token.cancel();
    let _ = export_task.await;
}
