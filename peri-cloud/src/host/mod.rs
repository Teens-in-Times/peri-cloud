//! Deployment owner for account HTTP, optional QQ, the original Peri runtime,
//! and device connections. No model/tool policy is introduced here.

mod config;
mod connections;

pub use config::{
    DeviceConnection, HostConfig, ModelConfig, QqConfig, QqRelay, QqTransport, SshConnection,
};

use std::sync::Arc;
use std::time::Duration;

use axum::extract::Request;
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Json;
use peri_model::{OpenAiConfig, OpenAiModel};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use crate::gateway::{Gateway, MessageAdapter};
use crate::identity::IdentityService;
use crate::portal::{self, PortalConfig};
use crate::qq::{webhook_router, QqAdapter};
use crate::state::CloudJournal;
use crate::CloudRuntime;

#[derive(Debug, thiserror::Error)]
pub enum HostError {
    #[error("cloud service configuration is invalid")]
    Configuration,
    #[error("cloud service credential is missing or invalid")]
    Credential,
    #[error("cloud service could not initialize its state")]
    State,
    #[error("cloud service I/O failed")]
    Io,
    #[error("cloud service owned worker failed")]
    Worker,
}
pub type HostResult<T> = Result<T, HostError>;

/// Run until a deployment signal or authenticated account shutdown request.
/// Incomplete cleanup retains owners and SSH tunnels, and is never reported as
/// successful shutdown. An operator can still force an OS kill; journals then
/// retain the existing crash-recovery semantics rather than replaying tasks.
pub async fn serve(config: HostConfig, stop: CancellationToken) -> HostResult<()> {
    config.validate()?;
    let bootstrap = config::credential(&config.bootstrap_env)?;
    let key = config::credential(&config.model.api_key_env)?;
    let portal_config =
        PortalConfig::new(&config.public_origin).map_err(|_| HostError::Configuration)?;
    let mut model = OpenAiConfig::new(config.model.api_base.clone(), key, &config.model.model)
        .with_max_tokens(config.model.max_tokens)
        .with_thinking_enabled(config.model.thinking_enabled)
        .with_thinking_content(config.model.supports_thinking_content);
    if let Some(effort) = &config.model.reasoning_effort {
        model = model.with_reasoning_effort(effort);
    }
    let connector = connections::Connections::new(
        config.devices,
        Arc::new(OpenAiModel::new(model)),
        config.system_prompt,
        config.model.context_window,
    )?;
    let qq = match &config.qq {
        Some(qq) => Some(
            QqAdapter::new(
                qq.instance_id.clone(),
                config::credential(&qq.app_id_env)?,
                config::credential(&qq.client_secret_env)?,
                &config.public_origin,
                qq.interaction.clone(),
                qq.endpoints()?,
            )
            .map_err(|_| HostError::Configuration)?,
        ),
        None => None,
    };
    let listener = tokio::net::TcpListener::bind(config.listen)
        .await
        .map_err(|_| HostError::Io)?;
    let address = listener.local_addr().map_err(|_| HostError::Io)?;
    let journal = Arc::new(
        CloudJournal::open(&config.state_dir)
            .await
            .map_err(|_| HostError::State)?,
    );
    let identity = IdentityService::new(&journal, &bootstrap)
        .await
        .map_err(|_| HostError::State)?;
    drop(bootstrap);
    let runtime = CloudRuntime::new(journal);
    let adapters = qq
        .iter()
        .map(|adapter| adapter.clone() as Arc<dyn MessageAdapter>)
        .collect();
    let gateway = Gateway::new(
        runtime.clone(),
        identity.clone(),
        connector.clone(),
        adapters,
        config.max_iterations,
    )
    .await
    .map_err(|_| HostError::State)?;
    let mut router =
        portal::router_for_host(identity, portal_config, gateway.clone(), stop.clone());
    if let (Some(adapter), Some(qq)) = (&qq, &config.qq) {
        if qq.transport == QqTransport::Webhook {
            router = router.merge(webhook_router(adapter.clone(), gateway.clone()));
        }
    }
    router = router
        .route(
            "/healthz",
            get(|| async { Json(serde_json::json!({"process":"serving"})) }),
        )
        .layer(axum::middleware::from_fn(request_deadline));
    let tunnel_stop = CancellationToken::new();
    let tunnels = connector.tunnels(tunnel_stop.clone());
    let mut workers = JoinSet::new();
    let cancelled = stop.clone();
    workers.spawn(async move {
        (
            "http",
            axum::serve(listener, router)
                .with_graceful_shutdown(cancelled.cancelled_owned())
                .await
                .map_err(|_| HostError::Io),
        )
    });
    let cancelled = stop.clone();
    let dispatcher = gateway.clone();
    workers.spawn(async move { ("dispatch", dispatch(dispatcher, cancelled).await) });
    if let (Some(adapter), Some(qq)) = (qq, &config.qq) {
        if qq.transport == QqTransport::Websocket {
            let cancelled = stop.clone();
            let gateway = gateway.clone();
            workers.spawn(async move {
                (
                    "qq",
                    adapter
                        .run_websocket(gateway, cancelled)
                        .await
                        .map_err(|_| HostError::Worker),
                )
            });
        }
    }
    tracing::info!(component="cloud_host", %address, "cloud service is serving account and configured gateways");
    let mut failed = false;
    tokio::select! {
        biased;
        _ = stop.cancelled() => (),
        result = workers.join_next() => { failed = true; report_worker(result); stop.cancel(); }
    }
    stop.cancel();
    while let Some(result) = workers.join_next().await {
        if result.as_ref().is_err() || result.as_ref().is_ok_and(|(_, result)| result.is_err()) {
            failed = true;
        }
        report_worker(Some(result));
    }
    // Do not drop the gateway/runtime or tunnels on budget expiration. This
    // process remains in shutdown until its owned work can be settled.
    loop {
        match gateway.shutdown(Duration::from_secs(3)).await {
            Ok(true) => break,
            _ => {
                tracing::error!(
                    component = "cloud_host",
                    "gateway shutdown requires recovery; owners retained"
                );
                tokio::time::sleep(Duration::from_secs(3)).await;
            }
        }
    }
    loop {
        let report = runtime.shutdown(Duration::from_secs(5)).await;
        if report.complete {
            break;
        }
        tracing::error!(
            component = "cloud_host",
            pending = report.pending_sessions.len(),
            unconfirmed = report.unconfirmed_sessions.len(),
            "execution shutdown incomplete; owners and tunnels retained"
        );
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
    tunnel_stop.cancel();
    for tunnel in tunnels {
        if tunnel.await.is_err() {
            failed = true;
        }
    }
    tracing::info!(
        component = "cloud_host",
        "cloud service stopped after owned work was settled"
    );
    if failed {
        Err(HostError::Worker)
    } else {
        Ok(())
    }
}

async fn dispatch(gateway: Arc<Gateway>, stop: CancellationToken) -> HostResult<()> {
    let mut interval = tokio::time::interval(Duration::from_millis(250));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! { biased; _ = stop.cancelled() => return Ok(()), _ = interval.tick() => () }
        gateway.dispatch().await.map_err(|_| HostError::Worker)?;
    }
}

fn report_worker(result: Option<Result<(&str, HostResult<()>), tokio::task::JoinError>>) {
    match result {
        Some(Ok((component, Err(_)))) => tracing::error!(component, "cloud service worker failed"),
        Some(Err(_)) => tracing::error!(component = "cloud_host", "cloud service worker panicked"),
        _ => (),
    }
}

async fn request_deadline(request: Request, next: Next) -> Response {
    match tokio::time::timeout(Duration::from_secs(30), next.run(request)).await {
        Ok(response) => response,
        Err(_) => (
            StatusCode::GATEWAY_TIMEOUT,
            Json(serde_json::json!({"error":"request_timed_out"})),
        )
            .into_response(),
    }
}
