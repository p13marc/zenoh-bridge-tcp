use crate::config::BridgeConfig;
use anyhow::Result;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use zenoh::Session;

/// Internal implementation for both regular and HTTP import modes
pub(super) async fn run_import_mode_internal(
    session: Arc<Session>,
    import_spec: &str,
    http_mode: bool,
    config: Arc<BridgeConfig>,
    shutdown_token: CancellationToken,
) -> Result<()> {
    let (service_name, listen_addr) = super::parse_import_spec(import_spec)?;
    let mode = if http_mode { "http_import" } else { "import" };

    super::accept::run_accept_loop(
        super::accept::AcceptLoopCfg {
            mode,
            client_id_prefix: "client_",
        },
        session,
        service_name,
        listen_addr,
        config,
        shutdown_token,
        move |session, stream, service, client_id, config| async move {
            super::connection::handle_import_connection(
                session, stream, &service, &client_id, http_mode, config,
            )
            .await
        },
    )
    .await
}
