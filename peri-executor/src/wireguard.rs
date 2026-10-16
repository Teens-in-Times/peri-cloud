use peri_executor::{web, Executor};
use peri_process::wireguard::WireGuardEndpoint;
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// A missing adapter must not take down the independent loopback/SSH listener.
pub async fn supervise(
    config: WireGuardEndpoint,
    executor: Arc<Executor>,
    stop: CancellationToken,
) {
    loop {
        if stop.is_cancelled() {
            return;
        }
        let attempt = async {
            config.source_address(true).await?;
            let listener = tokio::net::TcpListener::bind(config.endpoint).await?;
            tracing::info!(component = "wireguard", "Verified WireGuard listener ready");
            axum::serve(listener, web::router(executor.clone()))
                .with_graceful_shutdown(stop.clone().cancelled_owned())
                .await
        };
        let result = attempt.await;
        if stop.is_cancelled() {
            return;
        }
        tracing::warn!(
            component = "wireguard",
            failed = result.is_err(),
            "WireGuard listener unavailable; SSH remains available"
        );
        tokio::select! {
            _ = stop.cancelled() => return,
            _ = tokio::time::sleep(Duration::from_secs(5)) => {}
        }
    }
}
