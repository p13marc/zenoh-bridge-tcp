//! TCP backend dialing for the export side.
//!
//! Just the dial: retry policy, per-attempt bound, transport wrapping. The
//! caller (`bridge::spawn_client`) owns tracking, cancellation, and failure
//! signalling, and runs this **inside** the per-client task so a slow dial can
//! never stall the liveliness loop.

use crate::config::BridgeConfig;
use crate::transport::{TcpReader, TcpWriter};
use anyhow::Result;
use backon::{ExponentialBuilder, Retryable};
use std::net::SocketAddr;
use std::time::Duration;
use tokio::net::TcpStream;
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tracing::{info, warn};

/// Dial a TCP backend with retries, each attempt bounded by
/// `config.connect_timeout` (backon bounds attempts and inter-attempt delay,
/// not attempt duration — a blackholed address would otherwise run each
/// attempt to the OS SYN timeout).
pub(super) async fn dial(
    backend_addr: SocketAddr,
    client_id: &str,
    config: &BridgeConfig,
) -> Result<(TcpReader<OwnedReadHalf>, TcpWriter<OwnedWriteHalf>)> {
    let connect_timeout = config.connect_timeout;
    let client_id_for_log = client_id.to_string();
    let stream = (|| async {
        tokio::time::timeout(connect_timeout, TcpStream::connect(backend_addr))
            .await
            .map_err(|_| {
                std::io::Error::new(std::io::ErrorKind::TimedOut, "backend connect timed out")
            })?
    })
    .retry(
        ExponentialBuilder::default()
            .with_min_delay(Duration::from_millis(100))
            .with_max_delay(Duration::from_secs(5))
            .with_max_times(5),
    )
    .notify(move |err, dur| {
        warn!(
            client_id = %client_id_for_log,
            backend = %backend_addr,
            error = %err,
            retry_in = ?dur,
            "Backend connection failed, retrying"
        );
    })
    .await?;

    info!(backend = %backend_addr, "Backend connection established");
    let (r, w) = stream.into_split();
    Ok((TcpReader::new(r, config.buffer_size), TcpWriter::new(w)))
}
