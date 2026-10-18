//! Session-scoped external MCP pool.
//!
//! The pool contains only the authenticated account's enabled servers. Host
//! configuration files, plugins and builtin instances are explicitly disabled,
//! so a cloud session cannot inherit the cloud host's MCP surface.

use std::sync::Arc;
use std::time::Duration;

use peri_middlewares::mcp::{McpClientPool, McpInitStatus, McpTaskOwner};
use tokio::sync::{watch, Mutex};
use uuid::Uuid;

use super::store::{config, McpServerRecord, McpServerStore};

const INIT_WAIT: Duration = Duration::from_secs(5);

pub(crate) struct McpSession {
    pub pool: Arc<McpClientPool>,
    owner: McpTaskOwner,
}

impl Drop for McpSession {
    fn drop(&mut self) {
        // Fixed close order: pool begin-close, then the task owner, then the
        // pool's service-close transaction (which the spawned future runs).
        self.pool.begin_shutdown();
        self.owner.begin_shutdown();
        let pool = self.pool.clone();
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                let _ = pool.shutdown().await;
            });
        }
    }
}

struct Cached {
    principal: Uuid,
    revision: i64,
    session: Option<Arc<McpSession>>,
}

pub(crate) struct McpCache {
    state: Mutex<Option<Cached>>,
}

impl McpCache {
    pub(crate) fn new() -> Self {
        Self {
            state: Mutex::new(None),
        }
    }

    /// Returns `None` when the account has no enabled servers. The revision is
    /// read on every turn so configuration changes take effect without a new
    /// cloud session; an unchanged revision reuses the live pool.
    pub(crate) async fn load(
        &self,
        store: &McpServerStore,
        principal: Uuid,
    ) -> Option<Arc<McpSession>> {
        let revision = store.revision().await.unwrap_or(-1);
        let mut state = self.state.lock().await;
        if let Some(cached) = state.as_ref() {
            if cached.principal == principal && cached.revision == revision {
                return cached.session.clone();
            }
        }
        let servers = store.enabled().await.unwrap_or_default();
        let session = create(&servers).await;
        *state = Some(Cached {
            principal,
            revision,
            session: session.clone(),
        });
        session
    }
}

async fn create(servers: &[McpServerRecord]) -> Option<Arc<McpSession>> {
    if servers.is_empty() {
        return None;
    }
    let (owner, spawner) = McpTaskOwner::new();
    let pool = Arc::new(McpClientPool::new_pending_with_spawner(spawner));
    if let Err(error) = pool.set_builtin_available(false) {
        tracing::warn!(%error, "MCP pool rejected builtin closure");
        return None;
    }
    if let Err(error) = pool.set_stdio_available(false) {
        tracing::warn!(%error, "MCP pool rejected stdio closure");
        return None;
    }
    if let Err(error) = pool.set_plugin_discovery_available(false) {
        tracing::warn!(%error, "MCP pool rejected plugin closure");
        return None;
    }
    let mut configured = std::collections::HashMap::new();
    for server in servers {
        // Revalidate because DNS can change between configuration and connect.
        if super::egress::validate(&server.url).await.is_err() {
            tracing::warn!(server = %server.name, "MCP endpoint failed egress revalidation");
            continue;
        }
        configured.insert(server.name.clone(), config(server));
    }
    if configured.is_empty() {
        return None;
    }
    if let Err(error) = pool.set_session_servers(configured) {
        tracing::warn!(%error, "MCP pool rejected account servers");
        return None;
    }
    let (status_tx, mut status_rx) = watch::channel(McpInitStatus::Pending);
    let initializing = pool.clone();
    // Stdio is disabled, so the working directory only satisfies the pool's
    // reconnect bookkeeping and never names a process directory.
    let cwd = std::env::temp_dir();
    tokio::spawn(async move {
        McpClientPool::run_initialize_session_only(initializing, &cwd, status_tx).await;
    });
    let _ = tokio::time::timeout(INIT_WAIT, async {
        loop {
            if matches!(
                &*status_rx.borrow(),
                McpInitStatus::Ready { .. } | McpInitStatus::Failed(_)
            ) {
                break;
            }
            if status_rx.changed().await.is_err() {
                break;
            }
        }
    })
    .await;
    Some(Arc::new(McpSession { pool, owner }))
}
