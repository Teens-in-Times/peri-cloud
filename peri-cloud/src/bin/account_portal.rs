//! Standalone account entry point, also used for native executor enrollment.
//! Channel routing and the Agent runtime are assembled by the cloud deployment.

use std::net::SocketAddr;
use std::path::PathBuf;

use clap::Parser;
use peri_cloud::identity::IdentityService;
use peri_cloud::portal::{self, PortalConfig};
use peri_cloud::state::CloudJournal;
use tokio::net::TcpListener;

#[derive(Parser)]
#[command(about = "Browser login and device enrollment for a private cloud Agent")]
struct Args {
    /// Private persistent account/session directory.
    #[arg(long)]
    state_dir: PathBuf,
    /// Loopback listener, carried by SSH or an HTTPS reverse proxy.
    #[arg(long, default_value = "127.0.0.1:8765")]
    listen: SocketAddr,
    /// Browser origin; defaults to the actual loopback listener address.
    #[arg(long)]
    public_origin: Option<String>,
    /// Environment variable holding the one-time initialization credential.
    #[arg(long, default_value = "PERI_CLOUD_SETUP_TOKEN")]
    bootstrap_env: String,
}

#[derive(Debug, thiserror::Error)]
enum Error {
    #[error("account entry point must listen on loopback")]
    Listen,
    #[error("account initialization environment variable is missing")]
    Bootstrap,
    #[error(transparent)]
    State(#[from] peri_cloud::state::StateError),
    #[error(transparent)]
    Identity(#[from] peri_cloud::identity::IdentityError),
    #[error(transparent)]
    Portal(#[from] peri_cloud::portal::PortalError),
    #[error("account server I/O failed")]
    Io(#[from] std::io::Error),
}

#[tokio::main]
async fn main() -> Result<(), Error> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with_writer(std::io::stderr)
        .init();
    run(Args::parse()).await
}

async fn run(args: Args) -> Result<(), Error> {
    if !args.listen.ip().is_loopback() {
        return Err(Error::Listen);
    }
    let bootstrap = std::env::var(&args.bootstrap_env).map_err(|_| Error::Bootstrap)?;
    let listener = TcpListener::bind(args.listen).await?;
    let address = listener.local_addr()?;
    let origin = args
        .public_origin
        .unwrap_or_else(|| format!("http://{address}"));
    let config = PortalConfig::new(&origin)?;
    let journal = CloudJournal::open(&args.state_dir).await?;
    let identity = IdentityService::new(&journal, &bootstrap).await?;
    drop(bootstrap);
    let router = portal::router(identity, config);
    tracing::info!(component = "account_portal", %address, %origin, "account entry point is ready");
    // HTTP first drains admitted identity operations; only its deployment owner
    // may close the journal. Agent/task shutdown is a separate cloud concern.
    let result = axum::serve(listener, router)
        .with_graceful_shutdown(shutdown_signal())
        .await;
    journal.close().await;
    result?;
    Ok(())
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
