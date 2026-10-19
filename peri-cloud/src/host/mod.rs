//! Deployment owner for account HTTP, optional QQ, the original Peri runtime,
//! and device connections. No model/tool policy is introduced here.

mod config;
mod connections;
mod local;

pub use config::{
    DeviceConnection, HostConfig, IsolatedInstanceConfig, LocalInstanceConfig, ModelConfig,
    OneBotConfig, QqConfig, QqRelay, QqTransport, SshConnection,
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
    #[error("configured executor could not authenticate; check its transport and token")]
    Connection,
}
pub type HostResult<T> = Result<T, HostError>;

/// Run until a deployment signal or authenticated account shutdown request.
/// Incomplete cleanup retains owners and SSH tunnels, and is never reported as
/// successful shutdown. An operator can still force an OS kill; journals then
/// retain the existing crash-recovery semantics rather than replaying tasks.
pub async fn serve(config: HostConfig, stop: CancellationToken) -> HostResult<()> {
    config.validate()?;
    let bootstrap = config::credential(&config.bootstrap_env)?;
    let key = match &config.aggregation {
        Some(aggregate) => read_secret(&aggregate.inference_key_file).await?,
        None => config::credential(&config.model.api_key_env)?,
    };
    let portal_config = PortalConfig::new(&config.public_origin)
        .map_err(|_| HostError::Configuration)?
        .with_service_name(config.display_name.as_deref())
        .with_listen_authority(config.listen);
    let model_base = match &config.aggregation {
        Some(a) => a.origin.join("v1").map_err(|_| HostError::Configuration)?,
        None => config.model.api_base.clone(),
    };
    let mut model = OpenAiConfig::new(model_base, key.clone(), &config.model.model)
        .with_max_tokens(config.model.max_tokens)
        .with_thinking_enabled(config.model.thinking_enabled)
        .with_thinking_content(config.model.supports_thinking_content);
    if let Some(effort) = &config.model.reasoning_effort {
        model = model.with_reasoning_effort(effort);
    }
    let model: Arc<dyn peri_model::Model> = if config.aggregation.is_none()
        && config.model.protocol == crate::models::ModelProtocol::Anthropic
    {
        crate::models::direct_model(
            config.model.protocol,
            config.model.api_base.clone(),
            key,
            config.model.model.clone(),
            config.model.max_tokens,
        )
    } else {
        Arc::new(OpenAiModel::new(model))
    };
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
    let onebot = match &config.onebot {
        Some(value) => Some(
            crate::onebot::OneBotAdapter::new(
                value.instance_id.clone(),
                value.self_id.clone(),
                config::credential(&value.access_token_env)?,
                value.access_policy()?,
            )
            .map_err(|_| HostError::Configuration)?,
        ),
        None => None,
    };
    let onebot_listener = match &config.onebot {
        Some(value) => Some(
            tokio::net::TcpListener::bind(value.listen)
                .await
                .map_err(|_| HostError::Io)?,
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
    let mut providers = config.model_providers;
    providers.insert(
        0,
        crate::models::ModelProvider {
            id: "default".into(),
            name: "默认供应商（自有密钥）".into(),
            api_base: config.model.api_base.clone(),
            protocol: config.model.protocol,
        },
    );
    let account_models = crate::models::AccountModels::new(
        journal.clone(),
        providers,
        config.model.model.clone(),
        config.model.max_tokens,
    )
    .map_err(|_| HostError::Configuration)?;
    if let Some(aggregate) = &config.aggregation {
        let control = crate::aggregation::Aggregation::new(
            aggregate,
            read_secret(&aggregate.management_key_file).await?,
            read_secret(&aggregate.inference_key_file).await?,
            identity.clone(),
            journal.clone(),
            config.model.model.clone(),
            config.model.max_tokens,
        )
        .map_err(|_| HostError::Configuration)?;
        account_models.set_aggregation(control);
    }
    let local = match &config.local_instance {
        Some(value) => Some(local::LocalExecutor::start(&config.state_dir, value).await?),
        None => None,
    };
    let mut devices = config.devices;
    if let Some(instance) = config.isolated_instance {
        identity
            .configure_local_instance(crate::identity::DeviceRecord {
                id: instance.connection.device_id,
                name: instance.name,
                platform: "linux".into(),
                default_workspace: instance.workspace,
                connection_id: None,
                is_local: true,
                revoked: false,
            })
            .await
            .map_err(|_| HostError::State)?;
        devices.push(instance.connection);
    }
    if let Some(local) = &local {
        identity
            .configure_local_instance(local.record.clone())
            .await
            .map_err(|_| HostError::State)?;
        devices.push(local.connection.clone());
    }
    let connector = connections::Connections::new(
        devices,
        model,
        config.system_prompt,
        config.model.context_window,
        Some(identity.connection_store()),
    )?;
    connector.set_account_models(account_models.clone());
    connector.set_context_assembler(Arc::new(
        crate::context::ContextAssembler::new(config.context, config.state_dir.join("context"))
            .map_err(|_| HostError::Configuration)?,
    ));
    let runtime =
        CloudRuntime::with_limits(journal, config.limits).map_err(|_| HostError::Configuration)?;
    let mut adapters: Vec<Arc<dyn MessageAdapter>> = qq
        .iter()
        .map(|adapter| adapter.clone() as Arc<dyn MessageAdapter>)
        .collect();
    adapters.push(Arc::new(crate::gateway::WebAdapter::from_journal(
        runtime.journal(),
    )));
    if let Some(adapter) = &onebot {
        adapters.push(adapter.clone());
    }
    let gateway = Gateway::new(
        runtime.clone(),
        identity.clone(),
        connector.clone(),
        adapters,
        config.max_iterations,
    )
    .await
    .map_err(|_| HostError::State)?;
    gateway
        .set_control_origin(&config.public_origin)
        .map_err(|_| HostError::Configuration)?;
    let mut router = portal::router_for_host_with_models(
        identity,
        portal_config,
        gateway.clone(),
        stop.clone(),
        account_models,
    );
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
    let scheduler = gateway.clone();
    workers.spawn(async move { ("scheduler", schedule(scheduler, cancelled).await) });
    if let (Some(adapter), Some(listener)) = (onebot, onebot_listener) {
        let gateway = gateway.clone();
        let cancelled = stop.clone();
        workers.spawn(async move {
            (
                "onebot",
                adapter
                    .run(listener, gateway, cancelled)
                    .await
                    .map_err(|_| HostError::Worker),
            )
        });
    }
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
    if let Some(local) = local {
        local.shutdown().await?;
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

async fn schedule(gateway: Arc<Gateway>, stop: CancellationToken) -> HostResult<()> {
    let mut interval = tokio::time::interval(Duration::from_secs(1));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! { biased; _=stop.cancelled()=>return Ok(()), _=interval.tick()=>() }
        // Finish this deployment-owned batch before shutdown drains Gateway.
        gateway
            .schedule_tick()
            .await
            .map_err(|_| HostError::Worker)?;
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

async fn read_secret(path: &std::path::Path) -> HostResult<String> {
    let metadata = tokio::fs::metadata(path)
        .await
        .map_err(|_| HostError::Credential)?;
    if !metadata.is_file() || metadata.len() > 8192 {
        return Err(HostError::Credential);
    }
    let value = tokio::fs::read_to_string(path)
        .await
        .map_err(|_| HostError::Credential)?;
    let value = value.trim();
    if value.is_empty() || value.chars().any(char::is_control) {
        return Err(HostError::Credential);
    }
    Ok(value.to_owned())
}
