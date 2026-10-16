//! A host-owned executor retains exactly the same tool implementation and journal.
use std::path::Path;
use std::sync::Arc;

use peri_executor::Executor;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use super::config::LocalInstanceConfig;
use super::{DeviceConnection, HostError, HostResult};
use crate::identity::DeviceRecord;

pub(super) struct LocalExecutor {
    pub connection: DeviceConnection,
    pub record: DeviceRecord,
    executor: Arc<Executor>,
    stop: CancellationToken,
    worker: Option<JoinHandle<HostResult<()>>>,
}

impl LocalExecutor {
    pub async fn start(root: &Path, config: &LocalInstanceConfig) -> HostResult<Self> {
        let state = root.join("server-executor");
        tokio::fs::create_dir_all(&config.workspace)
            .await
            .map_err(|_| HostError::Io)?;
        let workspace = tokio::fs::canonicalize(&config.workspace)
            .await
            .map_err(|_| HostError::Io)?;
        let executor = Arc::new(
            Executor::open(&state, &config.name)
                .await
                .map_err(|_| HostError::State)?,
        );
        let info = executor.info();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .map_err(|_| HostError::Io)?;
        let endpoint = listener.local_addr().map_err(|_| HostError::Io)?;
        let stop = CancellationToken::new();
        let cancelled = stop.clone();
        let router = peri_executor::web::router(executor.clone());
        let worker = tokio::spawn(async move {
            axum::serve(listener, router)
                .with_graceful_shutdown(cancelled.cancelled_owned())
                .await
                .map_err(|_| HostError::Io)
        });
        Ok(Self {
            connection: DeviceConnection {
                wireguard: None,
                device_id: info.device_id,
                endpoint,
                token_file: state.join("transport-token"),
                ssh: None,
            },
            record: DeviceRecord {
                id: info.device_id,
                name: config.name.clone(),
                platform: info.platform,
                default_workspace: workspace.to_str().ok_or(HostError::Configuration)?.into(),
                connection_id: None,
                is_local: true,
                revoked: false,
            },
            executor,
            stop,
            worker: Some(worker),
        })
    }

    pub async fn shutdown(mut self) -> HostResult<()> {
        self.executor
            .shutdown()
            .await
            .map_err(|_| HostError::Worker)?;
        self.stop.cancel();
        if let Some(worker) = self.worker.take() {
            worker.await.map_err(|_| HostError::Worker)??;
        }
        Ok(())
    }
}

impl Drop for LocalExecutor {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}
