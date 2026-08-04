use super::{CancellationSender, ExportBackend};
use crate::config::{BridgeConfig, ReliabilityMode};
use anyhow::Result;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex, mpsc};
use tokio_util::sync::CancellationToken;
use tracing::{Instrument, debug, error, info, warn};
use zenoh::Session;
use zenoh::key_expr::KeyExpr;
use zenoh::qos::CongestionControl;
use zenoh_ext::{
    AdvancedPublisherBuilderExt, AdvancedSubscriberBuilderExt, CacheConfig, HistoryConfig,
    MissDetectionConfig, RecoveryConfig,
};

/// Unified export loop for TCP, HTTP, and WebSocket backends
///
/// This function handles the liveliness monitoring loop shared by all export modes.
/// The `backend` parameter determines how client connections are established.
pub(super) async fn run_export_loop(
    session: Arc<Session>,
    service_name: &str,
    backend: ExportBackend,
    config: Arc<BridgeConfig>,
    shutdown_token: CancellationToken,
) -> Result<()> {
    let dns_suffix = match &backend {
        ExportBackend::Tcp { dns_suffix, .. } => dns_suffix.clone(),
        ExportBackend::WebSocket(_) => None,
    };

    let mode = match &backend {
        ExportBackend::Tcp {
            dns_suffix: Some(_),
            ..
        } => "http_export",
        ExportBackend::Tcp { .. } => "export",
        ExportBackend::WebSocket(_) => "ws_export",
    };

    info!(
        mode = mode,
        service = %service_name,
        "Starting export bridge"
    );

    // Monitor client liveliness to create/destroy connections
    let liveliness_key = if let Some(ref dns) = dns_suffix {
        format!("{}/{}/clients/*", service_name, dns)
    } else {
        format!("{}/clients/*", service_name)
    };

    // Declare service availability liveliness token for HTTP mode
    let _service_liveliness = if let Some(ref dns) = dns_suffix {
        let service_key = format!("{}/{}/available", service_name, dns);
        let token = session
            .liveliness()
            .declare_token(&service_key)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to declare service liveliness: {}", e))?;
        debug!(service_key = %service_key, "Declared service availability");
        Some(token)
    } else {
        None
    };

    let liveliness_subscriber = session
        .liveliness()
        .declare_subscriber(&liveliness_key)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to subscribe to liveliness: {}", e))?;

    info!(liveliness_key = %liveliness_key, "Export bridge ready");

    // Track connection tasks and cancellation senders per client ID
    let cancellation_senders: Arc<Mutex<HashMap<String, CancellationSender>>> =
        Arc::new(Mutex::new(HashMap::new()));

    // Query existing clients that connected before this export started
    let existing_clients = session
        .liveliness()
        .get(&liveliness_key)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to query existing clients: {}", e))?;

    while let Ok(reply) = existing_clients.recv_async().await {
        if let Ok(sample) = reply.into_result() {
            let key = sample.key_expr().as_str();
            if let Some(client_id) = key.rsplit('/').next()
                && !client_id.is_empty()
            {
                let client_id = client_id.to_string();
                info!(client_id = %client_id, "Found existing client, connecting");
                dispatch_client_connect(
                    &session,
                    service_name,
                    &backend,
                    &client_id,
                    &cancellation_senders,
                    &config,
                )
                .await;
            }
        }
    }

    // Main loop: monitor liveliness and create/destroy connections
    loop {
        tokio::select! {
            result = liveliness_subscriber.recv_async() => {
                match result {
                    Ok(sample) => {
                        let key = sample.key_expr().as_str();
                        if let Some(client_id) = key.rsplit('/').next()
                            && !client_id.is_empty()
                        {
                            let client_id = client_id.to_string();

                            match sample.kind() {
                                zenoh::sample::SampleKind::Put => {
                                    dispatch_client_connect(
                                        &session,
                                        service_name,
                                        &backend,
                                        &client_id,
                                        &cancellation_senders,
                                        &config,
                                    )
                                    .await;
                                }
                                zenoh::sample::SampleKind::Delete => {
                                    handle_client_disconnect(&client_id, &cancellation_senders, config.drain_timeout).await;
                                }
                            }
                        }
                    }
                    Err(e) => {
                        warn!("Liveliness subscriber error (continuing): {:?}", e);
                        continue;
                    }
                }
            }
            _ = shutdown_token.cancelled() => {
                info!(service = %service_name, "Export bridge shutting down");
                // Collect senders and handles, then release the lock before awaiting
                let entries: Vec<(String, CancellationSender)> =
                    cancellation_senders.lock().await.drain().collect();

                // Send cancellation signals
                for (client_id, (tx, _)) in &entries {
                    let _ = tx.send(()).await;
                    debug!(client_id = %client_id, "Sent shutdown to client bridge");
                }

                // Wait for all task handles to drain
                for (client_id, (_, handle)) in entries {
                    match tokio::time::timeout(config.drain_timeout, handle).await {
                        Ok(Ok(())) => debug!(client_id = %client_id, "Client bridge drained"),
                        Ok(Err(e)) => warn!(client_id = %client_id, error = %e, "Client bridge task error during drain"),
                        Err(_) => warn!(client_id = %client_id, "Client bridge drain timeout"),
                    }
                }
                break;
            }
        }
    }

    // Explicitly undeclare liveliness subscriber
    if let Err(e) = liveliness_subscriber.undeclare().await {
        debug!(service = %service_name, "Error undeclaring liveliness subscriber: {:?}", e);
    }

    info!(service = %service_name, "Export bridge stopped");
    Ok(())
}

/// Dispatch a client connection to the appropriate backend handler
async fn dispatch_client_connect(
    session: &Arc<Session>,
    service_name: &str,
    backend: &ExportBackend,
    client_id: &str,
    cancellation_senders: &Arc<Mutex<HashMap<String, CancellationSender>>>,
    config: &Arc<BridgeConfig>,
) {
    match backend {
        ExportBackend::Tcp { addr, dns_suffix } => {
            super::tcp::handle_client_connect(
                session,
                service_name,
                *addr,
                client_id,
                cancellation_senders,
                dns_suffix.as_deref(),
                config,
            )
            .await;
        }
        ExportBackend::WebSocket(ws_url) => {
            super::ws::handle_ws_client_connect(
                session,
                service_name,
                ws_url,
                client_id,
                cancellation_senders,
                config,
            )
            .await;
        }
    }
}

/// How one direction of a client bridge ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DirectionEnd {
    /// Clean directional EOF / half-close.
    Eof,
    /// Torn down because the connection was reset (external teardown, sample
    /// miss, or the peer direction's error) — no error on this direction itself.
    Reset,
    /// This direction hit a hard error (read/write/publish/subscriber failure).
    Error,
}

/// The observable outcome of a whole client connection (C3 #18).
///
/// Returned by `handle_client_bridge` so callers — and a future metrics layer
/// (#43) — can count how connections end instead of only seeing `error!` logs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ConnectionOutcome {
    /// Both directions reached a clean EOF.
    Completed,
    /// The connection was reset without a direction erroring (external teardown
    /// or an unrecoverable sample miss).
    Reset,
    /// At least one direction failed with an error, or a direction task did not
    /// join cleanly (panic/abort).
    Failed,
}

/// Fold the two directional ends into a connection outcome. `None` means that
/// direction's task failed to join (panic/abort), which counts as a failure.
fn fold_outcome(b2z: Option<DirectionEnd>, z2b: Option<DirectionEnd>) -> ConnectionOutcome {
    let ends = [b2z, z2b];
    if ends
        .iter()
        .any(|e| matches!(e, None | Some(DirectionEnd::Error)))
    {
        ConnectionOutcome::Failed
    } else if ends.iter().any(|e| matches!(e, Some(DirectionEnd::Reset))) {
        ConnectionOutcome::Reset
    } else {
        ConnectionOutcome::Completed
    }
}

/// Handle the bridge logic for a single client connection.
///
/// Generic over `TransportReader`/`TransportWriter` so the same function
/// serves both TCP and WebSocket export paths.
///
/// Returns the [`ConnectionOutcome`] so the spawning task can log/count how the
/// connection ended (C3 #18).
#[allow(clippy::too_many_arguments)]
pub(super) async fn handle_client_bridge<R, W>(
    session: Arc<Session>,
    service_name: String,
    client_id: String,
    mut backend_reader: R,
    mut backend_writer: W,
    mut cancel_rx: mpsc::Receiver<()>,
    dns_suffix: Option<&str>,
    config: Arc<BridgeConfig>,
) -> Result<ConnectionOutcome>
where
    R: crate::transport::TransportReader,
    W: crate::transport::TransportWriter,
{
    let dns_part = dns_suffix.map(|d| format!("/{}", d)).unwrap_or_default();

    // Single abort token for the whole connection. A clean directional EOF ends
    // only its own direction (a half-close); a hard error, external teardown, an
    // unrecoverable sample miss, or reception backpressure (D2) trips this token
    // to reset both directions.
    let conn_cancel = CancellationToken::new();

    // D2: drain the client's TX subscriber through a bounded, non-blocking channel
    // so a slow backend writer cannot fill the default FIFO handler and block the
    // shared session's reception thread (head-of-line-blocking every other client).
    let (rx_callback, rx_channel_rx) = crate::backpressure::rx_channel(
        config.rx_channel_capacity,
        config.reliability,
        conn_cancel.clone(),
        client_id.clone(),
    );

    // Subscribe to messages from this specific client using AdvancedSubscriber
    // This enables late publisher detection and recovery of missed samples
    let sub_key = format!("{}{}/tx/{}", service_name, dns_part, client_id);
    let subscriber = session
        .declare_subscriber(&sub_key)
        .callback(rx_callback)
        .history(HistoryConfig::default().detect_late_publishers())
        .recovery(RecoveryConfig::default().periodic_queries(config.heartbeat_interval))
        .subscriber_detection()
        .await
        .map_err(|e| anyhow::anyhow!("Failed to subscribe: {:?}", e))?;

    info!(
        "Client {} subscribed to {} with late publisher detection",
        client_id, sub_key
    );

    // Declare AdvancedPublisher with cache and publisher detection for RX channel
    // This allows the import bridge to detect when we're ready and recover any missed samples
    let pub_key_str = format!("{}{}/rx/{}", service_name, dns_part, client_id);
    let pub_key: KeyExpr<'static> = pub_key_str
        .clone()
        .try_into()
        .map_err(|e| anyhow::anyhow!("Invalid key expression: {}", e))?;
    let publisher_builder = session
        .declare_publisher(pub_key.clone())
        .cache(CacheConfig::default().max_samples(config.cache_size))
        .sample_miss_detection(MissDetectionConfig::default().heartbeat(config.heartbeat_interval))
        .publisher_detection();
    // Stream reliability: block on a full TX queue instead of Zenoh's default
    // `Drop`, which would silently drop payload bytes and corrupt the stream.
    let publisher = match config.reliability {
        ReliabilityMode::Stream => publisher_builder.congestion_control(CongestionControl::Block),
        ReliabilityMode::Telemetry => publisher_builder,
    }
    .await
    .map_err(|e| anyhow::anyhow!("Failed to declare publisher: {}", e))?;

    debug!(
        "Client {}: Declared AdvancedPublisher on {} with cache",
        client_id, pub_key_str
    );

    let client_id_for_reader = client_id.clone();
    let client_id_for_writer = client_id.clone();

    // Metrics (G7): count this connection for the service; the guard decrements
    // the active gauge on every exit path. `svc` is resolved once and cloned into
    // each direction for lock-free per-chunk byte accounting.
    let conn_metrics = crate::metrics::conn_start(&service_name);
    let svc_b2z = conn_metrics.counters();
    let svc_z2b = conn_metrics.counters();

    // Stream reliability: an unrecoverable sample miss means the byte stream has a
    // gap that cannot be delivered faithfully. Reset the connection rather than
    // hand corrupted bytes to the backend. The listener runs in the background for
    // the subscriber's lifetime.
    if config.reliability == ReliabilityMode::Stream {
        let miss_cancel = conn_cancel.clone();
        let miss_client = client_id.clone();
        subscriber
            .sample_miss_listener()
            .callback(move |miss| {
                warn!(
                    "Client {}: unrecoverable sample miss ({} sample(s)) — resetting connection",
                    miss_client,
                    miss.nb()
                );
                miss_cancel.cancel();
            })
            .background()
            .await
            .map_err(|e| anyhow::anyhow!("Failed to register sample-miss listener: {:?}", e))?;
    }

    // Direction: backend -> Zenoh. A clean EOF publishes the empty EOF marker so
    // the import half-closes the client and ends this direction only. A read or
    // publish error resets the whole connection.
    let b2z_cancel = conn_cancel.clone();
    let backend_to_zenoh_handle = tokio::spawn(async move {
        let end = loop {
            tokio::select! {
                result = backend_reader.read_data() => {
                    match result {
                        Ok(data) if data.is_empty() => {
                            debug!("Backend half-close -> EOF for client {}", client_id_for_reader);
                            let _ = publisher.put(Vec::<u8>::new()).await;
                            break DirectionEnd::Eof;
                        }
                        Ok(data) => {
                            // Zero-copy: `data` is `Bytes`, published via Zenoh's
                            // `From<bytes::Bytes>` without a further copy.
                            svc_b2z.add_down(data.len());
                            if let Err(e) = publisher.put(data).await {
                                error!("Failed to publish for client {}: {:?}", client_id_for_reader, e);
                                b2z_cancel.cancel();
                                break DirectionEnd::Error;
                            }
                        }
                        Err(e) => {
                            error!("Backend read error for client {}: {:?}", client_id_for_reader, e);
                            // C1: emit EOF so the import side doesn't hang, then reset.
                            let _ = publisher.put(Vec::<u8>::new()).await;
                            b2z_cancel.cancel();
                            break DirectionEnd::Error;
                        }
                    }
                }
                _ = b2z_cancel.cancelled() => break DirectionEnd::Reset,
            }
        };
        // Return the publisher so it (and its cache) stays alive until the whole
        // connection ends, letting a late-joining import subscriber recover.
        (publisher, end)
    });

    // Direction: Zenoh -> backend. An empty payload is the client's half-close;
    // propagate it as a FIN on the backend's write side and end this direction only.
    // Samples arrive via the bounded backpressure channel (D2), not the subscriber
    // directly; the subscriber is kept alive here so its callback keeps delivering.
    let z2b_cancel = conn_cancel.clone();
    let mut rx = rx_channel_rx;
    let zenoh_to_backend_handle = tokio::spawn(async move {
        let end = loop {
            tokio::select! {
                maybe_sample = rx.recv() => {
                    match maybe_sample {
                        Some(sample) => {
                            let payload = sample.payload().to_bytes().to_vec();
                            if payload.is_empty() {
                                debug!("Client {}: client half-close -> FIN to backend", client_id_for_writer);
                                let _ = backend_writer.send_eof().await;
                                break DirectionEnd::Eof;
                            }
                            svc_z2b.add_up(payload.len());
                            if let Err(e) = backend_writer.write_data(&payload).await {
                                error!("Failed to write to backend for client {}: {:?}", client_id_for_writer, e);
                                z2b_cancel.cancel();
                                break DirectionEnd::Error;
                            }
                        }
                        None => {
                            // Reception channel closed (subscriber gone) — reset.
                            z2b_cancel.cancel();
                            break DirectionEnd::Error;
                        }
                    }
                }
                _ = z2b_cancel.cancelled() => {
                    let _ = backend_writer.shutdown().await;
                    break DirectionEnd::Reset;
                }
            }
        };
        // Return the subscriber; the coordinator undeclares it after both ends.
        (subscriber, end)
    });

    // External teardown (liveliness delete / duplicate connect) resets the connection.
    let ext_cancel = conn_cancel.clone();
    let ext_task = tokio::spawn(async move {
        let _ = cancel_rx.recv().await;
        ext_cancel.cancel();
    });

    // Wait for BOTH directions. Each ends on its own EOF (half-close) or when the
    // connection is reset. A healthy half-open connection has no artificial timeout;
    // once reset, a watchdog gives the tasks the drain budget and then aborts them.
    let drain_timeout = config.drain_timeout;
    let watchdog_cancel = conn_cancel.clone();
    let b2z_abort = backend_to_zenoh_handle.abort_handle();
    let z2b_abort = zenoh_to_backend_handle.abort_handle();
    let watchdog = tokio::spawn(async move {
        watchdog_cancel.cancelled().await;
        tokio::time::sleep(drain_timeout).await;
        b2z_abort.abort();
        z2b_abort.abort();
    });

    let (b2z_res, z2b_res) = tokio::join!(backend_to_zenoh_handle, zenoh_to_backend_handle);
    watchdog.abort();
    ext_task.abort();
    let _ = ext_task.await;

    // Undeclare the entities and capture each direction's end. A join error
    // (panic/abort past the drain deadline) counts as a failed direction.
    let b2z_end = match b2z_res {
        Ok((publisher, end)) => {
            let _ = publisher.undeclare().await;
            Some(end)
        }
        Err(e) => {
            warn!(client_id = %client_id, error = %e, "backend->zenoh task did not join cleanly");
            None
        }
    };
    let z2b_end = match z2b_res {
        Ok((subscriber, end)) => {
            let _ = subscriber.undeclare().await;
            Some(end)
        }
        Err(e) => {
            warn!(client_id = %client_id, error = %e, "zenoh->backend task did not join cleanly");
            None
        }
    };

    let outcome = fold_outcome(b2z_end, z2b_end);
    conn_metrics.set_outcome(match outcome {
        ConnectionOutcome::Completed => crate::metrics::ConnOutcome::Completed,
        ConnectionOutcome::Reset => crate::metrics::ConnOutcome::Reset,
        ConnectionOutcome::Failed => crate::metrics::ConnOutcome::Failed,
    });
    info!(client_id = %client_id, ?outcome, "Connection handler stopped");

    Ok(outcome)
}

/// Remove `client_id` from the map only if its entry is still the one owned by
/// `own_tx`. Identity is checked with `mpsc::Sender::same_channel`, so a
/// duplicate-connect that replaced the entry with a fresh channel is never
/// clobbered by the previous connection's self-cleanup (D1 #21).
///
/// Returns `true` if an entry was removed.
fn remove_if_current(
    map: &mut HashMap<String, CancellationSender>,
    client_id: &str,
    own_tx: &mpsc::Sender<()>,
) -> bool {
    if map
        .get(client_id)
        .is_some_and(|(tx, _)| tx.same_channel(own_tx))
    {
        map.remove(client_id);
        true
    } else {
        false
    }
}

/// Spawn the per-client bridge task and record it in the cancellation map.
///
/// Shared by the TCP and WebSocket export paths. Cancels any existing connection
/// for `client_id` first, then spawns the new bridge over the given transport
/// halves.
///
/// D1 (#21): the spawned task removes its **own** map entry when it completes
/// naturally, so entries no longer live forever after a backend-first close. The
/// removal is guarded by `mpsc::Sender::same_channel`, so a later duplicate
/// connect that replaced the entry is never clobbered. The insert is performed
/// under the same lock the self-removal must acquire, so a fast-completing task
/// cannot race ahead of its own insert and leave a stale entry behind.
#[allow(clippy::too_many_arguments)]
pub(super) async fn spawn_and_track<R, W>(
    session: Arc<Session>,
    service_name: String,
    client_id: String,
    reader: R,
    writer: W,
    dns_suffix: Option<String>,
    config: Arc<BridgeConfig>,
    cancellation_senders: &Arc<Mutex<HashMap<String, CancellationSender>>>,
    span: tracing::Span,
) where
    R: crate::transport::TransportReader,
    W: crate::transport::TransportWriter,
{
    // Cancel any existing connection for this client ID before spawning the new
    // one. Take the old entry out under the lock, then drain it without holding
    // the lock (a drain can take up to drain_timeout).
    let existing = cancellation_senders.lock().await.remove(&client_id);
    if let Some((old_cancel_tx, old_handle)) = existing {
        warn!(client_id = %client_id, "Client already has active connection, cancelling old one");
        let _ = old_cancel_tx.send(()).await;
        let _ = tokio::time::timeout(config.drain_timeout, old_handle).await;
    }

    let (cancel_tx, cancel_rx) = mpsc::channel::<()>(1);
    let self_remove_tx = cancel_tx.clone();
    let map = cancellation_senders.clone();
    let client_id_task = client_id.clone();

    // Hold the map lock across spawn + insert. The task's self-removal acquires
    // this same lock, so it cannot run before its entry is recorded.
    let mut guard = cancellation_senders.lock().await;
    let main_handle = tokio::spawn(
        async move {
            match handle_client_bridge(
                session,
                service_name,
                client_id_task.clone(),
                reader,
                writer,
                cancel_rx,
                dns_suffix.as_deref(),
                config,
            )
            .await
            {
                Ok(ConnectionOutcome::Completed) => {}
                Ok(outcome) => warn!(?outcome, "Client bridge ended non-cleanly"),
                Err(e) => error!(error = %e, "Client bridge error"),
            }
            // D1: free our own entry on natural completion, but only if it is
            // still ours (guards against a duplicate-connect replacement).
            if remove_if_current(&mut *map.lock().await, &client_id_task, &self_remove_tx) {
                debug!(client_id = %client_id_task, "Freed client map entry on completion");
            }
        }
        .instrument(span),
    );
    guard.insert(client_id, (cancel_tx, main_handle));
}

/// Handle a client disconnection event
pub(super) async fn handle_client_disconnect(
    client_id: &str,
    cancellation_senders: &Arc<Mutex<HashMap<String, CancellationSender>>>,
    drain_timeout: Duration,
) {
    info!("Client disconnected: {}", client_id);

    // Send cancellation signal and wait for task to complete
    if let Some((cancel_tx, task_handle)) = cancellation_senders.lock().await.remove(client_id) {
        // Send cancellation signal (ignore error if receiver already dropped)
        let _ = cancel_tx.send(()).await;
        info!(
            "  Sent shutdown signal to backend connection for: {}",
            client_id
        );

        // Wait for the task to drain and complete with a timeout
        match tokio::time::timeout(drain_timeout, task_handle).await {
            Ok(Ok(())) => {
                info!("  Backend connection drained and closed for: {}", client_id);
            }
            Ok(Err(e)) => {
                warn!(
                    "  Backend connection task error during drain for {}: {:?}",
                    client_id, e
                );
            }
            Err(_) => {
                warn!("  Drain timeout for backend connection: {}", client_id);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A dummy completed task handle to pair with a channel in the map.
    fn dummy_handle() -> tokio::task::JoinHandle<()> {
        tokio::spawn(async {})
    }

    #[tokio::test]
    async fn remove_if_current_frees_own_entry() {
        // D1: a connection that completes naturally frees its own map entry.
        let mut map: HashMap<String, CancellationSender> = HashMap::new();
        let (tx, _rx) = mpsc::channel::<()>(1);
        map.insert("c1".into(), (tx.clone(), dummy_handle()));

        assert!(remove_if_current(&mut map, "c1", &tx));
        assert!(map.is_empty(), "entry should be freed on completion");
    }

    #[tokio::test]
    async fn remove_if_current_leaves_replacement_untouched() {
        // D1: a duplicate connect replaces the entry with a fresh channel. When
        // the OLD connection later completes, its cleanup must not clobber the
        // live replacement.
        let mut map: HashMap<String, CancellationSender> = HashMap::new();
        let (old_tx, _old_rx) = mpsc::channel::<()>(1);
        let (new_tx, _new_rx) = mpsc::channel::<()>(1);

        // Replacement is now installed under the same client id.
        map.insert("c1".into(), (new_tx.clone(), dummy_handle()));

        // Old connection's self-cleanup runs with its stale channel handle.
        assert!(!remove_if_current(&mut map, "c1", &old_tx));
        assert!(
            map.get("c1")
                .is_some_and(|(tx, _)| tx.same_channel(&new_tx)),
            "the live replacement must survive the old connection's cleanup"
        );
    }

    #[tokio::test]
    async fn remove_if_current_no_entry_is_noop() {
        let mut map: HashMap<String, CancellationSender> = HashMap::new();
        let (tx, _rx) = mpsc::channel::<()>(1);
        assert!(!remove_if_current(&mut map, "absent", &tx));
    }

    // --- C3 #18: connection outcome folding ---

    #[test]
    fn fold_outcome_both_eof_is_completed() {
        assert_eq!(
            fold_outcome(Some(DirectionEnd::Eof), Some(DirectionEnd::Eof)),
            ConnectionOutcome::Completed
        );
    }

    #[test]
    fn fold_outcome_any_error_is_failed() {
        assert_eq!(
            fold_outcome(Some(DirectionEnd::Eof), Some(DirectionEnd::Error)),
            ConnectionOutcome::Failed
        );
        assert_eq!(
            fold_outcome(Some(DirectionEnd::Error), Some(DirectionEnd::Reset)),
            ConnectionOutcome::Failed
        );
    }

    #[test]
    fn fold_outcome_join_failure_is_failed() {
        // A direction whose task panicked/aborted (None) is a failure.
        assert_eq!(
            fold_outcome(None, Some(DirectionEnd::Eof)),
            ConnectionOutcome::Failed
        );
    }

    #[test]
    fn fold_outcome_reset_without_error() {
        // External teardown / sample-miss resets a direction without an error.
        assert_eq!(
            fold_outcome(Some(DirectionEnd::Eof), Some(DirectionEnd::Reset)),
            ConnectionOutcome::Reset
        );
        assert_eq!(
            fold_outcome(Some(DirectionEnd::Reset), Some(DirectionEnd::Reset)),
            ConnectionOutcome::Reset
        );
    }
}
