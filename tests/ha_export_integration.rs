//! A3: active/standby election between two exporters of one service.
//!
//! Before the election, two exporters BOTH served every client: requests
//! executed on both backends and both response streams interleaved into the
//! client socket. These tests pin the elected behaviour end to end with two
//! real bridge processes.

mod common;

use anyhow::Result;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::time::timeout;

/// A tagged echo backend: replies `<tag>:<first-chunk>` once per connection
/// and counts the connections it served.
async fn tagged_backend(
    tag: &'static str,
) -> (
    std::net::SocketAddr,
    Arc<AtomicUsize>,
    tokio::task::JoinHandle<()>,
) {
    let served = Arc::new(AtomicUsize::new(0));
    let served_cb = served.clone();
    let (addr, handle) = common::start_probe_immune_backend(move |mut stream, first| {
        let served = served_cb.clone();
        async move {
            served.fetch_add(1, Ordering::SeqCst);
            let mut reply = format!("{tag}:").into_bytes();
            reply.extend_from_slice(&first);
            let _ = stream.write_all(&reply).await;
        }
    })
    .await;
    (addr, served, handle)
}

/// One request through the import; returns the winning backend's tag.
async fn tagged_roundtrip(import_addr: std::net::SocketAddr, budget: Duration) -> Result<String> {
    let reply = common::echo_roundtrip(import_addr, b"ping", budget).await?;
    let text = String::from_utf8_lossy(&reply).to_string();
    let tag = text
        .split(':')
        .next()
        .filter(|t| !t.is_empty())
        .ok_or_else(|| anyhow::anyhow!("untagged reply: {text:?}"))?
        .to_string();
    // A single, exact reply — interleaving from two backends would corrupt it.
    anyhow::ensure!(
        text == format!("{tag}:ping"),
        "reply is not a single clean copy: {text:?}"
    );
    Ok(tag)
}

/// Two exporters, one service: exactly ONE serves — every request answered by
/// a single backend with a single clean copy, and the standby's backend never
/// sees a connection.
#[tokio::test]
async fn two_exporters_one_service_single_copy() -> Result<()> {
    let (addr_a, served_a, _ba) = tagged_backend("A").await;
    let (addr_b, served_b, _bb) = tagged_backend("B").await;

    let service = common::unique_service_name("hasvc");
    // Start A strictly first so it is deterministically the elder claimant.
    let _export_a =
        common::BridgeProcess::new(&["--backend", &format!("{service}/{addr_a}")]).await;
    tokio::time::sleep(Duration::from_millis(700)).await;
    let _export_b =
        common::BridgeProcess::new(&["--backend", &format!("{service}/{addr_b}")]).await;
    tokio::time::sleep(Duration::from_millis(300)).await;

    let import_port = common::PortGuard::new();
    let import_addr = import_port.release();
    let _import =
        common::BridgeProcess::new(&["--listen", &format!("{service}/{import_addr},proto=raw")])
            .await;
    common::wait_for_port(import_addr, Duration::from_secs(10)).await?;

    // Ten requests: all single-copy, all from the SAME exporter.
    let mut tags = std::collections::BTreeSet::new();
    for _ in 0..10 {
        tags.insert(tagged_roundtrip(import_addr, common::BACKEND_READY_TIMEOUT).await?);
    }
    anyhow::ensure!(
        tags.len() == 1,
        "requests were served by multiple exporters: {tags:?}"
    );
    anyhow::ensure!(
        tags.contains("A"),
        "the elder exporter (A) must be the active one, served by {tags:?}"
    );

    // The standby's backend must never have been dialed into service.
    anyhow::ensure!(
        served_b.load(Ordering::SeqCst) == 0,
        "the standby exporter's backend served {} connection(s)",
        served_b.load(Ordering::SeqCst)
    );
    anyhow::ensure!(served_a.load(Ordering::SeqCst) >= 10);
    Ok(())
}

/// Failover: kill the active exporter; the standby must take over and serve
/// subsequent requests.
#[tokio::test]
async fn standby_takes_over_when_active_dies() -> Result<()> {
    let (addr_a, _served_a, _ba) = tagged_backend("A").await;
    let (addr_b, served_b, _bb) = tagged_backend("B").await;

    let service = common::unique_service_name("hafail");
    let mut export_a =
        common::BridgeProcess::new(&["--backend", &format!("{service}/{addr_a}")]).await;
    tokio::time::sleep(Duration::from_millis(700)).await;
    let _export_b =
        common::BridgeProcess::new(&["--backend", &format!("{service}/{addr_b}")]).await;
    tokio::time::sleep(Duration::from_millis(300)).await;

    let import_port = common::PortGuard::new();
    let import_addr = import_port.release();
    let _import =
        common::BridgeProcess::new(&["--listen", &format!("{service}/{import_addr},proto=raw")])
            .await;
    common::wait_for_port(import_addr, Duration::from_secs(10)).await?;

    // A serves first.
    let tag = tagged_roundtrip(import_addr, common::BACKEND_READY_TIMEOUT).await?;
    anyhow::ensure!(tag == "A", "expected the elder exporter first, got {tag}");

    // Kill the active. The standby must observe the claim Delete and take over.
    export_a.kill_and_wait().await;

    let takeover = common::retry_client(
        || async {
            let tag = tagged_roundtrip(import_addr, Duration::from_secs(8))
                .await
                .map_err(std::io::Error::other)?;
            if tag == "B" {
                Ok(tag)
            } else {
                Err(std::io::Error::other(format!("still served by {tag}")))
            }
        },
        Duration::from_secs(30),
        "failover to the standby exporter",
    )
    .await?;
    anyhow::ensure!(takeover == "B");
    anyhow::ensure!(served_b.load(Ordering::SeqCst) >= 1);

    // And it keeps serving.
    let tag = tagged_roundtrip(import_addr, common::BACKEND_READY_TIMEOUT).await?;
    anyhow::ensure!(tag == "B", "post-failover request served by {tag}");
    Ok(())
}
