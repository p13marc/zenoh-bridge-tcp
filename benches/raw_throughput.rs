//! Raw TCP relay throughput benchmark (issue #45).
//!
//! Measures the bridge's opaque byte-relay throughput — the path used to tunnel
//! rsync/scp/SSH — against a direct TCP connection to the same echo backend, so
//! the bridge's added overhead is visible as MB/s and a percentage of direct TCP.
//!
//! Run with:  cargo bench --bench raw_throughput
//!
//! Each iteration streams `size` bytes to an echo server and reads them back,
//! writing and reading concurrently on the two halves of a persistent connection
//! (a bulk transfer that wrote everything before reading would self-deadlock once
//! buffers fill — real bulk transfer interleaves).

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::runtime::Runtime;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use zenoh::config::Config;
use zenoh_bridge_tcp::config::BridgeConfig;

/// Echo server: every connection echoes reads back until closed.
async fn start_echo_server() -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                break;
            };
            tokio::spawn(async move {
                let mut buf = vec![0u8; 256 * 1024];
                loop {
                    match stream.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            if stream.write_all(&buf[..n]).await.is_err() {
                                break;
                            }
                        }
                    }
                }
            });
        }
    });
    addr
}

/// Start a raw TCP export+import bridge pair, returning the import listen addr.
async fn start_bridge(backend: SocketAddr, token: &CancellationToken) -> SocketAddr {
    let config = Arc::new(BridgeConfig::default());
    let service = format!("bench_{}", uuid::Uuid::new_v4().as_simple());

    let export_session = Arc::new(zenoh::open(Config::default()).await.unwrap());
    let import_session = Arc::new(zenoh::open(Config::default()).await.unwrap());

    let s = export_session.clone();
    let spec = format!("{}/{}", service, backend);
    let cfg = config.clone();
    let t = token.child_token();
    tokio::spawn(async move {
        let _ = zenoh_bridge_tcp::export::run_export_mode(s, &spec, cfg, t).await;
    });
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let import_addr = l.local_addr().unwrap();
    drop(l);
    let s = import_session.clone();
    let spec = format!("{}/{}", service, import_addr);
    let t = token.child_token();
    tokio::spawn(async move {
        let _ = zenoh_bridge_tcp::import::run_import_mode(s, &spec, config, t).await;
    });

    // Leak the sessions for the process lifetime of the benchmark.
    std::mem::forget(export_session);
    std::mem::forget(import_session);
    import_addr
}

/// Persistent split connection guarded for reuse across benchmark iterations.
type Conn = Arc<Mutex<(OwnedReadHalf, OwnedWriteHalf)>>;

async fn connect(addr: SocketAddr) -> Conn {
    let (r, w) = TcpStream::connect(addr).await.unwrap().into_split();
    Arc::new(Mutex::new((r, w)))
}

/// One bulk round trip: write `payload`, read the same number of bytes back.
async fn round_trip(conn: &Conn, payload: &[u8]) {
    let mut guard = conn.lock().await;
    let (r, w) = &mut *guard;
    let mut out = vec![0u8; payload.len()];
    let (wr, rd) = tokio::join!(w.write_all(payload), r.read_exact(&mut out));
    wr.unwrap();
    rd.unwrap();
}

fn bench_throughput(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();
    let token = CancellationToken::new();

    let backend = rt.block_on(start_echo_server());
    let import_addr = rt.block_on(start_bridge(backend, &token));

    // Warm up the bridge path (liveliness propagation) with a small round trip.
    let bridged = rt.block_on(async {
        let conn = connect(import_addr).await;
        // Retry until the backend is wired.
        let payload = vec![0u8; 64];
        for _ in 0..100 {
            let mut g = conn.lock().await;
            let (r, w) = &mut *g;
            let mut out = vec![0u8; payload.len()];
            if w.write_all(&payload).await.is_ok()
                && tokio::time::timeout(
                    std::time::Duration::from_millis(500),
                    r.read_exact(&mut out),
                )
                .await
                .is_ok()
            {
                break;
            }
            drop(g);
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        conn
    });
    let direct = rt.block_on(connect(backend));

    let mut group = c.benchmark_group("raw_relay_round_trip");
    for size in [64 * 1024usize, 256 * 1024, 1024 * 1024] {
        let payload = vec![0xABu8; size];
        group.throughput(Throughput::Bytes(size as u64));

        group.bench_with_input(BenchmarkId::new("bridge", size), &payload, |b, p| {
            b.to_async(&rt).iter(|| round_trip(&bridged, p));
        });
        group.bench_with_input(BenchmarkId::new("direct_tcp", size), &payload, |b, p| {
            b.to_async(&rt).iter(|| round_trip(&direct, p));
        });
    }
    group.finish();

    token.cancel();
}

criterion_group!(benches, bench_throughput);
criterion_main!(benches);
