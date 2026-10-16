use std::path::PathBuf;

use clap::Parser;
use peri_cloud::host::{self, HostConfig, HostError};
use tokio_util::sync::CancellationToken;

#[derive(Parser)]
#[command(about = "Private cloud Agent, optional QQ gateway and native device execution")]
struct Args {
    #[arg(long)]
    config: PathBuf,
    /// Validate configuration without starting services or reading credentials.
    #[arg(long)]
    check_config: bool,
    /// Authenticate configured devices without starting a model, QQ or the portal.
    /// Existing SSH forwards must already be running.
    #[arg(long, conflicts_with = "check_config")]
    check_devices: bool,
}

#[tokio::main]
async fn main() -> Result<(), HostError> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with_writer(std::io::stderr)
        .init();
    let args = Args::parse();
    let config = HostConfig::read(&args.config).await?;
    if args.check_config {
        tracing::info!(component = "cloud_host", "cloud configuration is valid");
        return Ok(());
    }
    if args.check_devices {
        for device in config.devices.iter().chain(
            config
                .isolated_instance
                .iter()
                .map(|local| &local.connection),
        ) {
            let metadata = tokio::fs::metadata(&device.token_file)
                .await
                .map_err(|_| HostError::Credential)?;
            if !metadata.is_file() || metadata.len() > 8192 {
                return Err(HostError::Credential);
            }
            let token = tokio::fs::read_to_string(&device.token_file)
                .await
                .map_err(|_| HostError::Credential)?;
            if token.trim().is_empty() {
                return Err(HostError::Credential);
            }
            let client = peri_remote_tools::DeviceClient::connect_routes(
                device.routes(),
                token.trim().into(),
                device.device_id,
            )
            .await
            .map_err(|_| HostError::Connection)?;
            tracing::info!(device_id = %client.info().device_id, platform = %client.info().platform, transport = client.active_transport().await, "Executor authenticated");
        }
        return Ok(());
    }
    let stop = CancellationToken::new();
    let signal = stop.clone();
    let signals = tokio::spawn(async move {
        tokio::select! { biased; _ = signal.cancelled() => (), _ = shutdown_signal() => signal.cancel() }
    });
    let result = host::serve(config, stop.clone()).await;
    stop.cancel();
    signals.await.map_err(|_| HostError::Worker)?;
    result
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut terminate) => {
                tokio::select! { _ = tokio::signal::ctrl_c() => {}, _ = terminate.recv() => {} }
            }
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}
