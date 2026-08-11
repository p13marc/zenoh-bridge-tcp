//! Three-or-more-bridge topologies (the "make sure 3 bridges connect" ask).
//!
//! Every test here runs at least three separate bridge PROCESSES on a private
//! Zenoh scouting domain (`common::ScoutDomain`), so discovery is deterministic
//! and the topologies cannot contend with other tests. Before this file the
//! suite topped out at two interacting bridges, and `BridgePair` could not even
//! express a third node.

mod common;

use anyhow::Result;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Send `payload` to a raw import listener and return the echo, retrying the
/// whole exchange until the multi-hop path is wired.
async fn echo(addr: std::net::SocketAddr, payload: &[u8]) -> Result<Vec<u8>> {
    common::echo_roundtrip(addr, payload, common::BACKEND_READY_TIMEOUT).await
}

/// A fan-out: ONE export backend reached through TWO independent import doors,
/// three processes in all (backend-export + door-B + door-C). Both doors must
/// serve concurrently, and killing one must not disturb the other's client.
#[tokio::test]
async fn two_import_doors_share_one_backend() -> Result<()> {
    let domain = common::ScoutDomain::new();
    let (backend, _echo) = common::start_probe_immune_backend(|mut s, first| async move {
        // Echo, then keep echoing for the held connection.
        let _ = s.write_all(&first).await;
        let mut buf = vec![0u8; 1024];
        while let Ok(n) = s.read(&mut buf).await {
            if n == 0 || s.write_all(&buf[..n]).await.is_err() {
                break;
            }
        }
    })
    .await;

    let service = common::unique_service_name("fanout");
    let _export = domain
        .bridge(&["--backend", &format!("{service}/{backend}")])
        .await;

    let port_b = common::PortGuard::new();
    let addr_b = port_b.release();
    let door_b_proc = domain
        .bridge(&["--listen", &format!("{service}/{addr_b},proto=raw")])
        .await;

    let port_c = common::PortGuard::new();
    let addr_c = port_c.release();
    let _door_c = domain
        .bridge(&["--listen", &format!("{service}/{addr_c},proto=raw")])
        .await;

    common::wait_for_port(addr_b, Duration::from_secs(10)).await?;
    common::wait_for_port(addr_c, Duration::from_secs(10)).await?;

    // Both doors reach the one backend.
    assert_eq!(echo(addr_b, b"via-B").await?, b"via-B");

    // Hold a live connection through door C, prove it works, then kill door B:
    // door C's connection must be unaffected (independent client_id keyspaces).
    let mut held = common::connected_raw_client(addr_c, b"hold-C\n", common::BACKEND_READY_TIMEOUT)
        .await
        .expect("door C never served");

    let mut door_b = door_b_proc;
    door_b.kill_and_wait().await;
    tokio::time::sleep(Duration::from_millis(300)).await;

    held.write_all(b"still-there\n").await?;
    let mut buf = vec![0u8; 32];
    let n = tokio::time::timeout(Duration::from_secs(10), held.read(&mut buf))
        .await
        .map_err(|_| anyhow::anyhow!("door C's held connection died with door B"))??;
    assert_eq!(&buf[..n], b"still-there\n");
    Ok(())
}

/// A relay node: ONE process that is both a `--listen` (front door) and a
/// `--backend` (exposing a local service), wired between a separate front
/// importer and a separate back exporter — three processes forming a chain
/// front-import -> relay -> back-export-backend.
#[tokio::test]
async fn relay_node_bridges_two_services() -> Result<()> {
    let domain = common::ScoutDomain::new();

    // The real backend the whole chain reaches.
    let (backend, _echo) = common::start_probe_immune_backend(|mut s, first| async move {
        let _ = s.write_all(&first).await;
    })
    .await;

    let back_svc = common::unique_service_name("back");
    let front_svc = common::unique_service_name("front");

    // Node A: exports the real backend onto `back_svc`.
    let _a = domain
        .bridge(&["--backend", &format!("{back_svc}/{backend}")])
        .await;

    // Node B (the relay): imports `back_svc` on a local port AND re-exports that
    // local port onto `front_svc`. One process, both roles.
    let relay_port = common::PortGuard::new();
    let relay_addr = relay_port.release();
    let _b = domain
        .bridge(&[
            "--listen",
            &format!("{back_svc}/{relay_addr},proto=raw"),
            "--backend",
            &format!("{front_svc}/{relay_addr}"),
        ])
        .await;

    // Node C: the front door onto `front_svc`.
    let front_port = common::PortGuard::new();
    let front_addr = front_port.release();
    let _c = domain
        .bridge(&["--listen", &format!("{front_svc}/{front_addr},proto=raw")])
        .await;

    common::wait_for_port(relay_addr, Duration::from_secs(10)).await?;
    common::wait_for_port(front_addr, Duration::from_secs(10)).await?;

    // A client at the front must reach the backend three hops away, byte-exact.
    assert_eq!(echo(front_addr, b"three-hops").await?, b"three-hops");
    Ok(())
}

/// A late joiner: a live export+import pair, then a SECOND import door joins
/// the same service and must immediately reach the backend — proving a third
/// bridge can attach to an already-running topology.
#[tokio::test]
async fn third_bridge_joins_a_live_pair() -> Result<()> {
    let domain = common::ScoutDomain::new();
    let (backend, _echo) = common::start_probe_immune_backend(|mut s, first| async move {
        let _ = s.write_all(&first).await;
    })
    .await;

    let service = common::unique_service_name("latejoin");
    let _export = domain
        .bridge(&["--backend", &format!("{service}/{backend}")])
        .await;

    let port1 = common::PortGuard::new();
    let addr1 = port1.release();
    let _door1 = domain
        .bridge(&["--listen", &format!("{service}/{addr1},proto=raw")])
        .await;
    common::wait_for_port(addr1, Duration::from_secs(10)).await?;
    assert_eq!(echo(addr1, b"first").await?, b"first");

    // Now a THIRD bridge joins the live topology.
    let port2 = common::PortGuard::new();
    let addr2 = port2.release();
    let _door2 = domain
        .bridge(&["--listen", &format!("{service}/{addr2},proto=raw")])
        .await;
    common::wait_for_port(addr2, Duration::from_secs(10)).await?;

    assert_eq!(echo(addr2, b"late").await?, b"late");
    // The original door still works too.
    assert_eq!(echo(addr1, b"again").await?, b"again");
    Ok(())
}
