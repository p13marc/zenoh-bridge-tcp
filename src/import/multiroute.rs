use super::connection::{BackendRoute, resolve_backend, routing_key_from_head};
use crate::config::BridgeConfig;
use crate::http_util::{http_400_response, http_502_response};
use anyhow::Result;
use bytes::Bytes;
use flowscope::FlowSide;
use flowscope::http::{HttpEvent, HttpProxyParser};
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_util::sync::CancellationToken;
use tracing::{Instrument, debug, info, warn};
use zenoh::Session;
use zenoh::key_expr::KeyExpr;
use zenoh_ext::{
    AdvancedPublisherBuilderExt, AdvancedSubscriberBuilderExt, CacheConfig, HistoryConfig,
    MissDetectionConfig, RecoveryConfig,
};

/// Run HTTP import mode with per-request routing.
///
/// Each HTTP request on a persistent connection is independently routed
/// based on its Host header. This allows a single client TCP connection
/// to reach multiple backends on a single port.
pub(super) async fn run_http_multiroute_import_mode(
    session: Arc<Session>,
    import_spec: &str,
    config: Arc<BridgeConfig>,
    shutdown_token: CancellationToken,
) -> Result<()> {
    let (service_name, listen_addr) = super::parse_import_spec(import_spec)?;

    super::accept::run_accept_loop(
        super::accept::AcceptLoopCfg {
            mode: "http_multiroute",
            client_id_prefix: "client_",
        },
        session,
        service_name,
        listen_addr,
        config,
        shutdown_token,
        |session, stream, service, client_id, config| async move {
            handle_multiroute_connection(session, stream, &service, &client_id, config).await
        },
    )
    .await
}

/// Outcome of one client request/response exchange.
enum ExchangeOutcome {
    /// The response completed; `keep_alive` says whether the connection may
    /// carry another request.
    Completed { keep_alive: bool },
    /// The connection switched protocols (WebSocket / CONNECT) and was spliced
    /// opaquely to completion.
    Switched,
}

/// How obtaining the next request head ended.
enum NextRequest {
    Head(Box<flowscope::http::RequestHead>),
    /// No more requests — the client half closed cleanly between requests.
    CleanEnd,
    /// The request framing was malformed; `client_fault` picks 400 vs 502.
    Malformed {
        client_fault: bool,
    },
}

/// Drain every event the parser can currently produce into `events`.
fn drain_events(parser: &mut HttpProxyParser, events: &mut VecDeque<HttpEvent>) {
    while let Some(ev) = parser.next_event() {
        events.push_back(ev);
    }
}

/// Push all of `bytes` into `parser` for `side`, returning any tail the parser
/// did not accept (backpressure — to be re-offered after events are drained).
fn push_bytes(parser: &mut HttpProxyParser, side: FlowSide, bytes: &[u8]) -> Bytes {
    let mut pending = Bytes::copy_from_slice(bytes);
    while !pending.is_empty() {
        let accepted = parser.push(side, &pending);
        if accepted == 0 {
            break;
        }
        pending = pending.slice(accepted..);
    }
    pending
}

/// Handle a persistent HTTP connection with per-request routing.
///
/// A single [`HttpProxyParser`] owns both directions of the client connection:
/// client->bridge bytes are fed as [`FlowSide::Initiator`], bridge->client bytes
/// (assembled from whichever Zenoh backend served the request) as
/// [`FlowSide::Responder`]. flowscope frames each request and response — chunked
/// decoding, method-aware bodies, `1xx` interim handling, upgrades, and RFC 9112
/// §6.3 smuggling defense all live in that one state machine — so the bridge
/// only routes on the head and relays the raw wire bytes it never retains.
async fn handle_multiroute_connection(
    session: Arc<Session>,
    mut stream: TcpStream,
    service_name: &str,
    client_id: &str,
    config: Arc<BridgeConfig>,
) -> Result<()> {
    // Metrics (G7): the guard counts the connection and decrements the active
    // gauge on every exit path; `svc` is its per-service counters, into which
    // run_exchange records relayed bytes per direction (#63).
    let conn_metrics = crate::metrics::conn_start(service_name);
    let svc = conn_metrics.counters();
    // How the connection ended, for the outcome metric and the access log.
    // Optimistic until a break says otherwise; a refused or unroutable request
    // is a reset, an exchange error is a failure.
    let mut outcome = crate::metrics::ConnOutcome::Completed;

    let mut parser = super::connection::http_head_parser(&config);
    let mut events: VecDeque<HttpEvent> = VecDeque::new();
    let mut client_eof = false;
    let mut request_num = 0u64;

    loop {
        // 1. Obtain the next request head (from buffered events or by reading).
        let head = match next_request_head(
            &mut parser,
            &mut stream,
            &mut events,
            &mut client_eof,
            request_num == 0,
            &config,
        )
        .await
        {
            NextRequest::Head(h) => h,
            NextRequest::CleanEnd => break,
            NextRequest::Malformed { client_fault } => {
                if client_fault {
                    let _ = stream.write_all(&http_400_response()).await;
                }
                warn!("Refusing malformed request");
                outcome = crate::metrics::ConnOutcome::Reset;
                break;
            }
        };

        request_num += 1;
        let request_id = format!("{}:r{}", client_id, request_num);

        // 2. Resolve the routing key (RFC 9112 §3.2 authority; F3).
        let dns = match routing_key_from_head(&head) {
            Ok(d) => d,
            Err(e) => {
                warn!(request_id = %request_id, error = %e, "Unroutable request");
                let _ = stream.write_all(&http_400_response()).await;
                outcome = crate::metrics::ConnOutcome::Reset;
                break;
            }
        };
        info!(request_id = %request_id, dns = %dns, "Routing HTTP request");

        // 3. Resolve the backend: a matching @host backend wins, else the
        // service's default (plain) backend on bare keys. A Zenoh failure
        // refuses this one request (502), same posture as before.
        let route = match resolve_backend(&session, service_name, Some(&dns), &config).await {
            Ok(r) => r,
            Err(e) => {
                warn!(request_id = %request_id, error = %e, "Backend resolution failed");
                BackendRoute::Unavailable
            }
        };
        let route_dns = match route {
            BackendRoute::Host => Some(dns.as_str()),
            BackendRoute::Default => {
                info!(
                    request_id = %request_id,
                    dns = %dns,
                    routed_by = "default",
                    "Routing to the service default backend"
                );
                None
            }
            BackendRoute::Unavailable => {
                warn!(request_id = %request_id, dns = %dns, "No backend available");
                let _ = stream.write_all(&http_502_response(&dns)).await;
                // Keep the connection alive: the client may retry a different Host on
                // it. Consume this request's body first so the next request frames
                // from the right offset.
                consume_request_body(
                    &mut parser,
                    &mut stream,
                    &mut events,
                    &mut client_eof,
                    &config,
                )
                .await;
                continue;
            }
        };

        // 4. Run the exchange over a per-request Zenoh channel.
        // One span per request on this keep-alive connection. Instrumenting
        // the future (rather than holding an entered guard across the awaits
        // inside it) keeps the span scoped to this task.
        let request_span = tracing::info_span!(
            "request",
            request_id = %request_id,
            request_num,
            dns = %dns,
        );
        let exchange = run_exchange(
            &session,
            &mut parser,
            &mut stream,
            &mut events,
            &mut client_eof,
            &head,
            &dns,
            route_dns,
            service_name,
            &request_id,
            &config,
            &svc,
        )
        .instrument(request_span)
        .await;

        // Report the access log before propagating, so a failed exchange is
        // still one closed-connection record rather than a silent gap.
        let exchange = match exchange {
            Ok(o) => o,
            Err(e) => {
                conn_metrics.finish(crate::metrics::ConnOutcome::Failed);
                return Err(e);
            }
        };

        match exchange {
            ExchangeOutcome::Completed { keep_alive } => {
                if !keep_alive || parser.is_done() {
                    debug!(request_id = %request_id, "Closing persistent connection");
                    break;
                }
                debug!(request_id = %request_id, "Request complete, awaiting next request");
            }
            ExchangeOutcome::Switched => break,
        }
    }

    conn_metrics.finish(outcome);
    Ok(())
}

/// Pull the next [`RequestHead`](flowscope::http::RequestHead) out of the parser,
/// reading and pushing client bytes until one is framed (or the client goes away
/// / the framing is refused).
async fn next_request_head(
    parser: &mut HttpProxyParser,
    stream: &mut TcpStream,
    events: &mut VecDeque<HttpEvent>,
    client_eof: &mut bool,
    first: bool,
    config: &BridgeConfig,
) -> NextRequest {
    let mut leftover = Bytes::new();
    loop {
        // A RequestHead already framed (e.g. a pipelined request) wins.
        while let Some(front) = events.front() {
            if matches!(front, HttpEvent::RequestHead(_)) {
                if let Some(HttpEvent::RequestHead(h)) = events.pop_front() {
                    return NextRequest::Head(Box::new(h));
                }
            } else {
                // Stray non-request event between requests — discard.
                events.pop_front();
            }
        }

        if *client_eof {
            return NextRequest::CleanEnd;
        }

        // A protocol switch where a fresh request was expected (an h2
        // preface, most likely a plaintext gRPC client on a route=request
        // listener): this plane is HTTP/1.1-only, and a tunnelled parser
        // accepts nothing more — without this check the re-offer loop below
        // would spin forever on an unshrinkable leftover (flowscope 0.24
        // returns 0 for tunnelled pushes instead of silently dropping).
        if parser.is_tunnelled() {
            return NextRequest::Malformed {
                client_fault: false,
            };
        }

        // Re-offer any backpressured tail before reading more.
        if !leftover.is_empty() {
            leftover = push_bytes(parser, FlowSide::Initiator, &leftover);
            drain_events(parser, events);
            if let Some(reason) = parser.poison() {
                return NextRequest::Malformed {
                    client_fault: reason.implies_client_fault().unwrap_or(true),
                };
            }
            continue;
        }

        let mut tmp = vec![0u8; config.buffer_size];
        let n = match tokio::time::timeout(config.read_timeout, stream.read(&mut tmp)).await {
            Ok(Ok(n)) => n,
            Ok(Err(_)) | Err(_) => {
                // A read error or idle timeout ends the connection. Only the very
                // first request treats it as a client fault worth a 400.
                return if first {
                    NextRequest::Malformed { client_fault: true }
                } else {
                    NextRequest::CleanEnd
                };
            }
        };

        if n == 0 {
            *client_eof = true;
            parser.fin(FlowSide::Initiator);
            drain_events(parser, events);
            continue;
        }

        leftover = push_bytes(parser, FlowSide::Initiator, &tmp[..n]);
        drain_events(parser, events);
        if let Some(reason) = parser.poison() {
            return NextRequest::Malformed {
                client_fault: reason.implies_client_fault().unwrap_or(true),
            };
        }
    }
}

/// Consume (and discard) the remainder of the current request's body up to its
/// `End(Initiator)`, so the next request on a kept-alive connection frames from
/// the correct offset. Used after a 502 when no backend served the request.
async fn consume_request_body(
    parser: &mut HttpProxyParser,
    stream: &mut TcpStream,
    events: &mut VecDeque<HttpEvent>,
    client_eof: &mut bool,
    config: &BridgeConfig,
) {
    loop {
        while let Some(front) = events.pop_front() {
            match front {
                HttpEvent::End {
                    dir: FlowSide::Initiator,
                } => return,
                // The next request is already framed: no body remained. Put it
                // back for the outer loop.
                HttpEvent::RequestHead(h) => {
                    events.push_front(HttpEvent::RequestHead(h));
                    return;
                }
                // Discard body/trailers of the request we refused.
                _ => {}
            }
        }

        if *client_eof {
            return;
        }

        let mut tmp = vec![0u8; config.buffer_size];
        let n = match tokio::time::timeout(config.read_timeout, stream.read(&mut tmp)).await {
            Ok(Ok(n)) => n,
            Ok(Err(_)) | Err(_) => return,
        };
        if n == 0 {
            *client_eof = true;
            parser.fin(FlowSide::Initiator);
            drain_events(parser, events);
            continue;
        }
        let _ = push_bytes(parser, FlowSide::Initiator, &tmp[..n]);
        drain_events(parser, events);
        if parser.poison().is_some() || parser.is_tunnelled() {
            return;
        }
    }
}

/// Run a single request/response exchange over a fresh per-request Zenoh channel.
///
/// Request events (`Initiator`) are streamed to the tx publisher; response events
/// (`Responder`), fed from the rx subscriber, are streamed back to the client.
/// Bodies are relayed as the raw wire bytes and never buffered whole (D4). The
/// two directions advance concurrently, so `Expect: 100-continue` and any `1xx`
/// interim cannot deadlock (E4). A protocol switch splices the rest opaquely
/// (E5). A framing violation stops the connection with a 4xx/5xx (E6).
#[allow(clippy::too_many_arguments)]
async fn run_exchange(
    session: &Arc<Session>,
    parser: &mut HttpProxyParser,
    stream: &mut TcpStream,
    events: &mut VecDeque<HttpEvent>,
    client_eof: &mut bool,
    head: &flowscope::http::RequestHead,
    dns: &str,
    route_dns: Option<&str>,
    service_name: &str,
    request_id: &str,
    config: &BridgeConfig,
    svc: &Arc<crate::metrics::ConnCounters>,
) -> Result<ExchangeOutcome> {
    // Per-request Zenoh entities. `dns` is the requested host (for error
    // bodies and logs); `route_dns` scopes the keys — `None` routes on the
    // bare service keys to the default backend.
    let sub_request_id = format!("{}_{}", request_id, uuid::Uuid::new_v4().as_simple());
    let dns_suffix = route_dns.map(|d| format!("/{d}")).unwrap_or_default();

    let rx_key: KeyExpr<'static> = format!("{}{}/rx/{}", service_name, dns_suffix, sub_request_id)
        .try_into()
        .map_err(|e| anyhow::anyhow!("Invalid key expression: {}", e))?;
    let tx_key: KeyExpr<'static> = format!("{}{}/tx/{}", service_name, dns_suffix, sub_request_id)
        .try_into()
        .map_err(|e| anyhow::anyhow!("Invalid key expression: {}", e))?;
    let liveliness_key = format!("{}{}/clients/{}", service_name, dns_suffix, sub_request_id);

    let liveliness_token = session
        .liveliness()
        .declare_token(&liveliness_key)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to declare liveliness: {}", e))?;
    // D2: drain this request's response subscriber through a bounded, non-blocking
    // channel so a slow client cannot fill the default FIFO handler and block the
    // shared session's reception thread — head-of-line-blocking every other client
    // (the same protection the plain import/export paths already have).
    let rx_cancel = CancellationToken::new();
    let (rx_callback, mut rx) = crate::backpressure::rx_channel(
        config.rx_channel_capacity,
        config.reliability,
        rx_cancel.clone(),
        request_id.to_string(),
    );
    let rx_subscriber = session
        .declare_subscriber(&rx_key)
        .callback(rx_callback)
        .history(HistoryConfig::default().detect_late_publishers())
        .subscriber_detection()
        .recovery(RecoveryConfig::default())
        .await
        .map_err(|e| anyhow::anyhow!("Failed to subscribe: {}", e))?;
    let tx_publisher = session
        .declare_publisher(tx_key)
        .cache(CacheConfig::default().max_samples(config.cache_size))
        .publisher_detection()
        .sample_miss_detection(MissDetectionConfig::default().heartbeat(config.heartbeat_interval))
        .await
        .map_err(|e| anyhow::anyhow!("Failed to declare publisher: {}", e))?;

    // Forward the request head to the backend.
    tx_publisher
        .put(&head.raw[..])
        .await
        .map_err(|e| anyhow::anyhow!("Failed to publish request head: {}", e))?;
    svc.add_up(head.raw.len());

    let mut request_done = false;
    let mut response_done = false;
    let mut switched = false;
    let mut keep_alive = true;
    let mut bytes_written = 0usize;
    let mut client_pending = Bytes::new();
    let mut resp_pending = Bytes::new();

    let deadline = tokio::time::Instant::now() + Duration::from_secs(30).max(config.drain_timeout);

    'exchange: loop {
        // Re-offer any backpressured tails.
        if !client_pending.is_empty() {
            client_pending = push_bytes(parser, FlowSide::Initiator, &client_pending);
            drain_events(parser, events);
        }
        if !resp_pending.is_empty() {
            resp_pending = push_bytes(parser, FlowSide::Responder, &resp_pending);
            drain_events(parser, events);
        }

        // Process every event we can act on now. Response events are handled
        // wherever they sit; events belonging to the NEXT (pipelined) request —
        // anything on the Initiator side once this request is fully sent — are
        // carried aside and restored for the next exchange, so a queued
        // RequestHead never blocks the current response (E2).
        let mut carry: VecDeque<HttpEvent> = VecDeque::new();
        let mut stop_exchange = false;
        while let Some(ev) = events.pop_front() {
            match ev {
                HttpEvent::Body {
                    dir: FlowSide::Initiator,
                    raw,
                    ..
                } if !request_done => {
                    tx_publisher
                        .put(&raw[..])
                        .await
                        .map_err(|e| anyhow::anyhow!("Failed to publish request body: {}", e))?;
                    svc.add_up(raw.len());
                }
                HttpEvent::End {
                    dir: FlowSide::Initiator,
                } if !request_done => {
                    // The request is fully forwarded. Do NOT publish an EOF marker
                    // here: the backend frames the request itself (Content-Length /
                    // chunked terminator), and an empty put would make the export
                    // half-close the backend write side before it responds. The EOF
                    // marker is published once, at teardown, to release the backend.
                    request_done = true;
                }
                HttpEvent::ResponseHead(rh) => {
                    stream.write_all(&rh.raw).await?;
                    bytes_written += rh.raw.len();
                    svc.add_down(rh.raw.len());
                }
                HttpEvent::Body {
                    dir: FlowSide::Responder,
                    raw,
                    ..
                } => {
                    stream.write_all(&raw).await?;
                    bytes_written += raw.len();
                    svc.add_down(raw.len());
                    if bytes_written > config.max_response_size {
                        warn!("Response exceeded max size");
                        keep_alive = false;
                        stop_exchange = true;
                        break;
                    }
                }
                HttpEvent::Trailers {
                    dir: FlowSide::Responder,
                    raw,
                    ..
                } => {
                    stream.write_all(&raw).await?;
                    bytes_written += raw.len();
                    svc.add_down(raw.len());
                }
                HttpEvent::End {
                    dir: FlowSide::Responder,
                } => {
                    response_done = true;
                    stop_exchange = true;
                    break;
                }
                HttpEvent::SwitchProtocols { kind } => {
                    debug!(kind = ?kind, "Protocol switch");
                    switched = true;
                    stop_exchange = true;
                    break;
                }
                // A RequestHead (or any Initiator event) once the current request
                // is done belongs to the next request: defer it.
                other => carry.push_back(other),
            }
        }
        // Restore deferred next-request events to the front, preserving order.
        while let Some(e) = carry.pop_back() {
            events.push_front(e);
        }
        if stop_exchange {
            break 'exchange;
        }

        // A framing violation refuses the connection (E6).
        if let Some(reason) = parser.poison() {
            warn!(reason = %reason.as_str(), "Framing violation");
            if bytes_written == 0 {
                let resp = if reason.implies_client_fault().unwrap_or(true) {
                    http_400_response()
                } else {
                    http_502_response(dns)
                };
                let _ = stream.write_all(&resp).await;
            }
            keep_alive = false;
            break;
        }

        // Need more bytes. Read the client (until its request is done) and the
        // backend response concurrently.
        let mut tmp = vec![0u8; config.buffer_size];
        tokio::select! {
            r = stream.read(&mut tmp), if !request_done && !*client_eof => {
                match r {
                    Ok(0) => {
                        *client_eof = true;
                        parser.fin(FlowSide::Initiator);
                        drain_events(parser, events);
                    }
                    Ok(n) => {
                        client_pending = push_bytes(parser, FlowSide::Initiator, &tmp[..n]);
                        drain_events(parser, events);
                    }
                    Err(e) => return Err(anyhow::anyhow!("client read error: {}", e)),
                }
            }
            s = tokio::time::timeout_at(deadline, rx.recv()) => {
                match s {
                    Ok(Some(sample)) => {
                        let payload = sample.payload().to_bytes();
                        if payload.is_empty() {
                            parser.fin(FlowSide::Responder);
                            drain_events(parser, events);
                        } else {
                            resp_pending = push_bytes(parser, FlowSide::Responder, &payload);
                            drain_events(parser, events);
                        }
                    }
                    Ok(None) => break, // subscriber closed
                    Err(_) => {
                        warn!("Response timeout");
                        if bytes_written == 0 {
                            let _ = stream.write_all(&crate::http_util::http_504_response()).await;
                        }
                        keep_alive = false;
                        break;
                    }
                }
            }
            _ = rx_cancel.cancelled() => {
                // D2 overflow (Stream mode): this client is too slow to drain its
                // response — reset the exchange rather than block the session.
                warn!("Reception buffer full, resetting slow multiroute exchange");
                keep_alive = false;
                break;
            }
        }
    }

    // If the exchange switched protocols (WebSocket / CONNECT), splice the rest
    // opaquely: client bytes -> backend tx, backend rx -> client, until either
    // side closes. Done inline so the concrete Zenoh entity types stay local.
    if switched {
        // First bytes of the switched-to protocol arrive in two places, both
        // of which must be forwarded BEFORE anything read after the switch:
        // the parser's tunnel residue (bytes coalesced into the same segment
        // as the 101/CONNECT-200 — flowscope 0.24.1's take_tunnel_residue),
        // and any tail the tunnelled parser refused back into our pendings.
        let up_first = [
            parser.take_tunnel_residue(FlowSide::Initiator),
            std::mem::take(&mut client_pending),
        ];
        for chunk in up_first.iter().filter(|c| !c.is_empty()) {
            tx_publisher
                .put(&chunk[..])
                .await
                .map_err(|e| anyhow::anyhow!("Failed to splice client bytes: {}", e))?;
            svc.add_up(chunk.len());
        }
        let down_first = [
            parser.take_tunnel_residue(FlowSide::Responder),
            std::mem::take(&mut resp_pending),
        ];
        for chunk in down_first.iter().filter(|c| !c.is_empty()) {
            stream.write_all(chunk).await?;
            svc.add_down(chunk.len());
        }

        let mut tmp = vec![0u8; config.buffer_size];
        loop {
            tokio::select! {
                r = stream.read(&mut tmp) => {
                    match r {
                        Ok(0) => break,
                        Ok(n) => {
                            if tx_publisher.put(&tmp[..n]).await.is_err() {
                                break;
                            }
                            svc.add_up(n);
                        }
                        Err(_) => break,
                    }
                }
                s = rx.recv() => {
                    match s {
                        Some(sample) => {
                            let payload = sample.payload().to_bytes();
                            if payload.is_empty() || stream.write_all(&payload).await.is_err() {
                                break;
                            }
                            svc.add_down(payload.len());
                        }
                        None => break,
                    }
                }
                _ = rx_cancel.cancelled() => break,
            }
        }
    }

    // Tear down: EOF to the backend, then undeclare.
    let _ = tx_publisher.put(Vec::<u8>::new()).await;
    let _ = liveliness_token.undeclare().await;
    let _ = tx_publisher.undeclare().await;
    let _ = rx_subscriber.undeclare().await;

    if switched {
        return Ok(ExchangeOutcome::Switched);
    }

    let keep_alive = keep_alive && response_done && !*client_eof;
    Ok(ExchangeOutcome::Completed { keep_alive })
}

