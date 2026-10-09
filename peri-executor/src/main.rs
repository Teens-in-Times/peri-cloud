use std::io::Write;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use clap::{Parser, Subcommand};
use peri_executor::login::LoginClient;
use peri_executor::{web, Executor};

mod tunnel;

#[derive(Parser)]
#[command(version, about = "Native device executor, connected through SSH")]
struct Args {
    #[arg(long)]
    state_dir: PathBuf,
    #[arg(long, default_value = "127.0.0.1:42371")]
    listen: SocketAddr,
    #[arg(long, global = true)]
    device_name: Option<String>,
    #[command(flatten)]
    tunnel: tunnel::TunnelArgs,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Authorize in a browser on this device, then register its workspace.
    Login {
        #[arg(long)]
        cloud_url: String,
        #[arg(long, default_value = ".")]
        workspace: PathBuf,
        #[arg(long)]
        no_browser: bool,
    },
    /// Show saved account/device metadata without exposing tokens.
    LoginStatus {
        #[arg(long)]
        cloud_url: String,
        #[arg(long, default_value = ".")]
        workspace: PathBuf,
    },
    /// Retry device registration using the saved login.
    Register {
        #[arg(long)]
        cloud_url: String,
        #[arg(long, default_value = ".")]
        workspace: PathBuf,
    },
    /// Remove local login only; remote device revocation is in the cloud portal.
    ForgetLogin {
        #[arg(long)]
        cloud_url: String,
        #[arg(long, default_value = ".")]
        workspace: PathBuf,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    let args = Args::parse();
    anyhow::ensure!(
        args.listen.ip().is_loopback(),
        "Executor must listen on a loopback address; use SSH forwarding for remote access"
    );
    let name = args.device_name.unwrap_or_else(|| {
        std::env::var("COMPUTERNAME")
            .or_else(|_| std::env::var("HOSTNAME"))
            .unwrap_or_else(|_| "device".into())
    });
    if let Some(command) = args.command {
        return account_command(&args.state_dir, &name, command).await;
    }
    args.tunnel.validate()?;
    let executor = Arc::new(Executor::open(&args.state_dir, &name).await?);
    let listener = tokio::net::TcpListener::bind(args.listen).await?;
    let tunnel_stop = tokio_util::sync::CancellationToken::new();
    let connection = tokio::spawn(
        args.tunnel
            .supervise(listener.local_addr()?, tunnel_stop.clone()),
    );
    tracing::info!(device_id = %executor.info().device_id, listen = %listener.local_addr()?, state_dir = %args.state_dir.display(), "Executor started");
    let server_result = axum::serve(listener, web::router(executor.clone()))
        .with_graceful_shutdown(shutdown_signal())
        .await;
    while executor.shutdown().await.is_err() {
        tracing::error!(
            component = "executor",
            "Shutdown requires recovery; execution and connection owners retained"
        );
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
    tunnel_stop.cancel();
    connection.await.context("SSH connection owner failed")?;
    server_result.context("Executor HTTP server failed")
}

async fn account_command(
    root: &std::path::Path,
    name: &str,
    command: Command,
) -> anyhow::Result<()> {
    let (origin, workspace) = match &command {
        Command::Login {
            cloud_url,
            workspace,
            ..
        }
        | Command::LoginStatus {
            cloud_url,
            workspace,
        }
        | Command::Register {
            cloud_url,
            workspace,
        }
        | Command::ForgetLogin {
            cloud_url,
            workspace,
        } => (cloud_url, workspace),
    };
    let client = LoginClient::new(origin, root, name, workspace).await?;
    match command {
        Command::Login { no_browser, .. } => {
            let pending = client.begin().await?;
            writeln!(
                std::io::stdout(),
                "请在本机浏览器完成授权：\n{}",
                pending.authorization_url()
            )?;
            if !no_browser {
                if let Err(error) = open_browser(pending.authorization_url().as_str()).await {
                    tracing::warn!(error_kind = ?error.kind(), "Could not open browser; use the displayed URL on this device");
                }
            }
            let receipt = tokio::select! {
                result = pending.finish(Duration::from_secs(600)) => result?,
                _ = shutdown_signal() => anyhow::bail!("Login cancelled"),
            };
            writeln!(
                std::io::stdout(),
                "已授权并登记设备：{} ({})\n工作区：{}\nSSH 在线连接仍需配置。",
                receipt.device.name,
                receipt.device.id,
                receipt.device.default_workspace
            )?;
        }
        Command::LoginStatus { .. } => match client.status().await? {
            Some(status) => writeln!(
                std::io::stdout(),
                "{}",
                serde_json::to_string_pretty(&status)?
            )?,
            None => writeln!(std::io::stdout(), "未保存登录。")?,
        },
        Command::Register { .. } => {
            let receipt = client.register_saved().await?;
            writeln!(
                std::io::stdout(),
                "已登记设备：{} ({})\n工作区：{}",
                receipt.device.name,
                receipt.device.id,
                receipt.device.default_workspace
            )?;
        }
        Command::ForgetLogin { .. } => {
            client.forget_local_login().await?;
            writeln!(
                std::io::stdout(),
                "已清除本地登录。云端设备和授权请在账号页面撤销。"
            )?;
        }
    }
    Ok(())
}

async fn open_browser(url: &str) -> std::io::Result<()> {
    #[cfg(windows)]
    let mut command = {
        let mut command = tokio::process::Command::new("rundll32.exe");
        command.arg("url.dll,FileProtocolHandler").arg(url);
        command
    };
    #[cfg(unix)]
    let mut command = {
        let mut command = tokio::process::Command::new("xdg-open");
        command.arg(url);
        command
    };
    let mut child = command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()?;
    tokio::spawn(async move {
        let _ = child.wait().await;
    });
    Ok(())
}

#[cfg(test)]
#[path = "main_test.rs"]
mod tests;

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .expect("install SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {},
            _ = terminate.recv() => {},
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}
