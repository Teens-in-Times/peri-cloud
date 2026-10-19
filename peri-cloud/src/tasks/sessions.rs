//! Stable execution conversations. Physical cloud sessions are context windows.
use super::*;
use crate::state::{FrozenSession, StateError, StateResult};
use sqlx::{Connection, Row, SqlitePool};
use std::collections::BTreeMap;

pub(super) struct SessionBoundary {
    pub private: bool,
    pub epoch: u64,
    pub generation: u64,
}
impl SessionBoundary {
    pub fn new(private: bool, epoch: u64, generation: u64) -> Self {
        Self {
            private,
            epoch,
            generation,
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct ExecutionSession {
    pub id: Uuid,
    pub agent_id: Uuid,
    pub principal: Uuid,
    pub device: Option<Uuid>,
    pub workspace: String,
    pub workspace_identity: Option<peri_acp_types::device_executor::WorkspaceIdentity>,
    pub visibility: String,
    pub privacy_epoch: u64,
    pub sources: BTreeMap<Uuid, u64>,
    pub window: Uuid,
    pub window_number: u64,
    pub created_at: i64,
}

fn visibility(private: bool, route: &ChannelRoute) -> Result<String> {
    // Private OAuth-owned routes can explicitly share. Public conversations
    // never inherit each other's audience, even for the same principal.
    Ok(if private {
        "private".into()
    } else {
        format!("public:{}", serde_json::to_string(route)?)
    })
}

impl ExecutionSession {
    fn new(
        frozen: &FrozenSession,
        scope: &TaskScope,
        private: bool,
        epoch: u64,
        generation: u64,
    ) -> Result<Self> {
        Ok(Self {
            id: Uuid::new_v4(),
            agent_id: Uuid::new_v4(),
            principal: scope.principal,
            device: frozen.device_id,
            workspace: frozen.workspace(),
            workspace_identity: frozen.binding.as_ref().map(|b| b.workspace_identity),
            visibility: visibility(private, &scope.route)?,
            privacy_epoch: epoch,
            sources: BTreeMap::from([(scope.root_session, generation)]),
            window: frozen.session_id,
            window_number: 0,
            created_at: now(),
        })
    }

    fn matches(
        &self,
        frozen: &FrozenSession,
        private: bool,
        route: &ChannelRoute,
        epoch: u64,
    ) -> Result<bool> {
        Ok(self.principal == frozen.principal_id
            && self.device == frozen.device_id
            && self.workspace == frozen.workspace()
            && self.workspace_identity == frozen.binding.as_ref().map(|b| b.workspace_identity)
            && self.visibility == visibility(private, route)?
            && self.privacy_epoch == epoch)
    }
}

impl WorkerService {
    pub(super) async fn execution_valid(&self, task: &TaskRecord) -> Result<bool> {
        let record = self
            .execution_session(task.principal, task.execution_session)
            .await?;
        if record.privacy_epoch != task.privacy_epoch
            || record.visibility != visibility(task.private_memory, &task.route)?
        {
            return Ok(false);
        }
        for (source, generation) in &record.sources {
            let state = self
                .journal
                .session(task.principal, *source)
                .await
                .map_err(|_| TaskError::Forbidden)?;
            if state.context_generation != *generation {
                return Ok(false);
            }
        }
        Ok(true)
    }
    pub(crate) async fn execution_session(
        &self,
        principal: Uuid,
        id: Uuid,
    ) -> Result<ExecutionSession> {
        let raw: String = sqlx::query_scalar(
            "SELECT record_json FROM agent_execution_sessions WHERE id=? AND principal=?",
        )
        .bind(id.to_string())
        .bind(principal.to_string())
        .fetch_optional(&self.store.pool)
        .await?
        .ok_or(TaskError::Forbidden)?;
        Ok(serde_json::from_str(&raw)?)
    }

    pub(super) async fn resolve_execution_session(
        &self,
        scope: &TaskScope,
        requested: Option<Uuid>,
        candidate: &FrozenSession,
        boundary: SessionBoundary,
    ) -> Result<ExecutionSession> {
        let SessionBoundary {
            private,
            epoch,
            generation,
        } = boundary;
        if candidate.principal_id != scope.principal {
            return Err(TaskError::Forbidden);
        }
        if let Some(id) = requested {
            let mut existing = self.execution_session(scope.principal, id).await?;
            if !existing.matches(candidate, private, &scope.route, epoch)? {
                return Err(TaskError::Forbidden);
            }
            for (parent, expected) in &existing.sources {
                let state = self
                    .journal
                    .session(scope.principal, *parent)
                    .await
                    .map_err(|_| TaskError::Forbidden)?;
                if state.context_generation != *expected {
                    return Err(TaskError::Forbidden);
                }
            }
            if existing.sources.len() >= 64 && !existing.sources.contains_key(&scope.root_session) {
                return Err(TaskError::Limit);
            }
            existing.sources.insert(scope.root_session, generation);
            sqlx::query(
                "UPDATE agent_execution_sessions SET record_json=? WHERE id=? AND principal=?",
            )
            .bind(serde_json::to_string(&existing)?)
            .bind(id.to_string())
            .bind(scope.principal.to_string())
            .execute(&self.store.pool)
            .await?;
            return Ok(existing);
        }
        let record = ExecutionSession::new(candidate, scope, private, epoch, generation)?;
        let mut tx = self.store.pool.begin().await?;
        sqlx::query("INSERT INTO agent_execution_sessions(id,principal,record_json) VALUES(?,?,?)")
            .bind(record.id.to_string())
            .bind(record.principal.to_string())
            .bind(serde_json::to_string(&record)?)
            .execute(&mut *tx)
            .await?;
        sqlx::query("INSERT INTO agent_execution_windows(execution_session,window_number,cloud_session) VALUES(?,0,?)")
            .bind(record.id.to_string()).bind(record.window.to_string()).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(record)
    }

    pub(crate) async fn session_head(&self, task: &TaskRecord) -> Result<bool> {
        let head: Option<String> = sqlx::query_scalar("SELECT id FROM agent_tasks WHERE worker_session=? AND json_extract(record_json,'$.state') NOT IN ('completed','failed','cancelled','recovery_required','invalidated') ORDER BY rowid LIMIT 1")
            .bind(task.worker_session.to_string()).fetch_optional(&self.store.pool).await?;
        Ok(head.as_deref() == Some(task.id.to_string().as_str()))
    }

    pub(super) async fn list_execution_sessions(
        &self,
        scope: &TaskScope,
    ) -> Result<serde_json::Value> {
        let gateway = self.gateway.upgrade().ok_or(TaskError::Unavailable)?;
        if !gateway
            .worker_scope_valid(scope)
            .await
            .map_err(|_| TaskError::Unavailable)?
        {
            return Err(TaskError::Forbidden);
        }
        let epoch = crate::memory::MemoryStore::new(&self.journal, scope.principal, 65536)
            .privacy_epoch()
            .await
            .map_err(|_| TaskError::Unavailable)?;
        let audience = visibility(gateway.worker_private_route(&scope.route), &scope.route)?;
        let rows: Vec<String> = sqlx::query_scalar("SELECT record_json FROM agent_execution_sessions WHERE principal=? ORDER BY rowid DESC LIMIT 64")
            .bind(scope.principal.to_string()).fetch_all(&self.store.pool).await?;
        let devices = crate::memory::scope::devices(&self.store.pool, scope.principal)
            .await
            .map_err(|_| TaskError::Unavailable)?;
        let mut entries = Vec::new();
        for raw in rows {
            let record: ExecutionSession = serde_json::from_str(&raw)?;
            if record.visibility != audience
                || record.privacy_epoch != epoch
                || record
                    .device
                    .is_some_and(|id| !devices.iter().any(|d| d.id == id && !d.revoked))
            {
                continue;
            }
            let mut valid = true;
            for (parent, generation) in &record.sources {
                if self
                    .journal
                    .session(scope.principal, *parent)
                    .await
                    .map(|s| s.context_generation != *generation)
                    .unwrap_or(true)
                {
                    valid = false;
                    break;
                }
            }
            if valid {
                entries.push(serde_json::json!({"execution_session":record.id,"agent_id":record.agent_id,"device_id":record.device,"workspace":record.workspace,"window_number":record.window_number}));
            }
        }
        Ok(
            serde_json::json!({"sessions":entries,"limit":64,"reuse":"explicit; same principal/device/canonical workspace/privacy scope"}),
        )
    }
}

#[cfg(test)]
#[path = "sessions_test.rs"]
mod tests;

/// One-time table rebuild removes the old one-task/one-window uniqueness.
/// No history or uncertain effects are discarded. The journal owns one SQLite connection.
pub(crate) async fn migrate(pool: &SqlitePool) -> StateResult<()> {
    let version: i64 = sqlx::query_scalar("PRAGMA user_version")
        .fetch_one(pool)
        .await?;
    if version >= 17 {
        return Ok(());
    }
    if version != 16 {
        return Err(StateError::Version);
    }
    let mut conn = pool.acquire().await?;
    sqlx::query("PRAGMA foreign_keys=OFF")
        .execute(&mut *conn)
        .await?;
    let result: StateResult<()> = async {
        let mut tx = conn.begin().await?;
        for sql in [
            "CREATE TABLE agent_tasks_rebuilt (id TEXT PRIMARY KEY,principal TEXT NOT NULL REFERENCES principals(id),parent_session TEXT NOT NULL REFERENCES cloud_sessions(id),worker_session TEXT NOT NULL REFERENCES cloud_sessions(id),request_key TEXT NOT NULL,record_json TEXT NOT NULL,UNIQUE(parent_session,request_key))",
            "INSERT INTO agent_tasks_rebuilt SELECT * FROM agent_tasks",
            "DROP TABLE agent_tasks",
            "ALTER TABLE agent_tasks_rebuilt RENAME TO agent_tasks",
            "CREATE INDEX agent_tasks_owner ON agent_tasks(principal,parent_session)",
            "CREATE INDEX agent_tasks_window ON agent_tasks(worker_session)",
            "CREATE TABLE agent_execution_sessions (id TEXT PRIMARY KEY,principal TEXT NOT NULL REFERENCES principals(id),record_json TEXT NOT NULL)",
            "CREATE INDEX agent_execution_owner ON agent_execution_sessions(principal)",
            "CREATE TABLE agent_execution_windows (execution_session TEXT NOT NULL REFERENCES agent_execution_sessions(id),window_number INTEGER NOT NULL,cloud_session TEXT NOT NULL UNIQUE REFERENCES cloud_sessions(id),PRIMARY KEY(execution_session,window_number))",
        ] { sqlx::query(sql).execute(&mut *tx).await?; }
        let rows = sqlx::query("SELECT t.record_json,s.state_json FROM agent_tasks t JOIN cloud_sessions s ON s.id=t.worker_session").fetch_all(&mut *tx).await?;
        for row in rows {
            let mut task: TaskRecord = serde_json::from_str(row.get("record_json"))?;
            let state: crate::state::SessionState = serde_json::from_str(row.get("state_json"))?;
            let scope = TaskScope { service: Weak::new(), principal: task.principal, session: task.parent_session, root_session: task.parent_session, task: None, role: AgentRole::Main, execution_agent: None, route: task.route.clone() };
            let record = ExecutionSession::new(&state.frozen, &scope, task.private_memory, task.privacy_epoch, task.parent_generation).map_err(|_| StateError::BindingConflict)?;
            task.execution_session = record.id;
            sqlx::query("INSERT INTO agent_execution_sessions(id,principal,record_json) VALUES(?,?,?)").bind(record.id.to_string()).bind(record.principal.to_string()).bind(serde_json::to_string(&record)?).execute(&mut *tx).await?;
            sqlx::query("INSERT INTO agent_execution_windows VALUES(?,0,?)").bind(record.id.to_string()).bind(record.window.to_string()).execute(&mut *tx).await?;
            sqlx::query("UPDATE agent_tasks SET record_json=? WHERE id=?").bind(serde_json::to_string(&task)?).bind(task.id.to_string()).execute(&mut *tx).await?;
        }
        let invalid = sqlx::query("PRAGMA foreign_key_check").fetch_optional(&mut *tx).await?;
        if invalid.is_some() { return Err(StateError::BindingConflict); }
        sqlx::query("PRAGMA user_version=17").execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(())
    }.await;
    sqlx::query("PRAGMA foreign_keys=ON")
        .execute(&mut *conn)
        .await?;
    result
}
