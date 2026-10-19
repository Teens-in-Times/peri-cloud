//! Shared native operation journal. Original targets and submission identity never change.
use super::*;
use peri_agent::tools::{ToolContext, ToolOutput};
use peri_remote_tools::RemoteSession;
use serde_json::{json, Value};
use sha2::Digest;
use sqlx::{Row, SqlitePool};

fn settled(output: &ToolOutput) -> bool {
    output.execution.as_ref().is_some_and(|e| {
        matches!(
            e.status,
            peri_acp_types::tools::ToolExecutionStatus::Completed
                | peri_acp_types::tools::ToolExecutionStatus::Failed
                | peri_acp_types::tools::ToolExecutionStatus::Cancelled
                | peri_acp_types::tools::ToolExecutionStatus::TimedOut
        )
    })
}
impl WorkerService {
    pub(super) async fn native_begin(
        &self,
        scope: &TaskScope,
        remote: &RemoteSession,
        name: &str,
        input: &Value,
        ctx: &ToolContext<'_>,
    ) -> Result<Uuid> {
        if ctx.session_id.as_deref() != Some(scope.session.to_string().as_str())
            || ctx.cancellation.is_cancelled()
        {
            return Err(TaskError::Forbidden);
        }
        let turn = ctx
            .turn_generation
            .as_deref()
            .and_then(|s| Uuid::parse_str(s).ok())
            .ok_or(TaskError::Invalid)?;
        let call = ctx
            .tool_call_id
            .as_ref()
            .ok_or(TaskError::Invalid)?
            .to_string();
        let state = self
            .journal
            .session(scope.principal, scope.session)
            .await
            .map_err(|_| TaskError::Forbidden)?;
        let active = self
            .journal
            .turn(scope.principal, scope.session, turn)
            .await
            .map_err(|_| TaskError::Forbidden)?;
        if active.state != crate::state::TurnState::Running || active.cancel_requested {
            return Err(TaskError::Conflict);
        }
        let invocation = remote
            .invocation_id(ctx)
            .map_err(|_| TaskError::Forbidden)?;
        let fingerprint = format!(
            "{:x}",
            sha2::Sha256::digest(serde_json::to_vec(&(
                remote.device_id(),
                remote.binding(),
                name,
                input
            ))?)
        );
        let frozen = crate::state::FrozenSession {
            session_id: scope.session,
            principal_id: scope.principal,
            device_id: Some(remote.device_id()),
            binding: Some(remote.binding().clone()),
            system_prompt: state.frozen.system_prompt,
            context_window: state.frozen.context_window,
        };
        let epoch = crate::memory::MemoryStore::new(&self.journal, scope.principal, 65536)
            .privacy_epoch()
            .await
            .map_err(|_| TaskError::Unavailable)?;
        let id = Uuid::new_v4();
        let mut tx = self.store.pool.begin().await?;
        if let Some(task) = scope.task {
            let valid:i64=sqlx::query_scalar("SELECT COUNT(*) FROM agent_task_operations o JOIN agent_tasks t ON t.id=o.task_id WHERE o.task_id=? AND o.turn_id=? AND o.window_id=? AND o.call_id=? AND o.invocation_id=? AND o.instruction_watermark=(SELECT COALESCE(MAX(seq),0) FROM agent_task_messages WHERE task_id=o.task_id AND direction='parent') AND json_extract(t.record_json,'$.state')='running'")
                .bind(task.to_string()).bind(turn.to_string()).bind(scope.session.to_string()).bind(&call).bind(invocation.to_string()).fetch_one(&mut *tx).await?;
            if valid != 1 {
                return Err(TaskError::Conflict);
            }
        }
        if matches!(name, "Write" | "Edit" | "Bash") {
            let pending:i64=sqlx::query_scalar("SELECT COUNT(*) FROM cloud_native_operations WHERE principal=? AND device_id=? AND state!='confirmed'").bind(scope.principal.to_string()).bind(remote.device_id().to_string()).fetch_one(&mut *tx).await?;
            if pending != 0 {
                return Err(TaskError::Conflict);
            }
        }
        let bound_pending:i64=sqlx::query_scalar("SELECT COUNT(*) FROM cloud_native_operations WHERE principal=? AND json_extract(frozen_json,'$.binding.session_id')=? AND state!='confirmed'")
            .bind(scope.principal.to_string()).bind(remote.binding().session_id.to_string()).fetch_one(&mut *tx).await?;
        if bound_pending != 0 {
            return Err(TaskError::Conflict);
        }
        // No INSERT OR IGNORE followed by execution: an old submission is query-only.
        let exists:i64=sqlx::query_scalar("SELECT COUNT(*) FROM cloud_native_operations WHERE principal=? AND source_session=? AND turn_id=? AND call_id=?")
            .bind(scope.principal.to_string()).bind(scope.session.to_string()).bind(turn.to_string()).bind(&call).fetch_one(&mut *tx).await?;
        if exists != 0 {
            return Err(TaskError::Conflict);
        }
        sqlx::query("INSERT INTO cloud_native_operations(id,principal,source_session,turn_id,task_id,call_id,device_id,frozen_json,invocation_id,tool,input_json,input_hash,state,context_generation,privacy_epoch,created_at) VALUES(?,?,?,?,?,?,?,?,?,?,?,?,'submitted_unknown',?,?,?)")
            .bind(id.to_string()).bind(scope.principal.to_string()).bind(scope.session.to_string()).bind(turn.to_string()).bind(scope.task.map(|v|v.to_string())).bind(call).bind(remote.device_id().to_string()).bind(serde_json::to_string(&frozen)?).bind(invocation.to_string()).bind(name).bind(serde_json::to_string(input)?).bind(fingerprint).bind(state.context_generation as i64).bind(epoch as i64).bind(now()).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(id)
    }
    pub(super) async fn native_finish(&self, id: Uuid, output: &ToolOutput) -> Result<()> {
        sqlx::query("UPDATE cloud_native_operations SET receipt_json=?,state=CASE WHEN state='confirmed' OR ? THEN 'confirmed' ELSE 'running_or_unknown' END,reconciled_json=CASE WHEN ? THEN NULL ELSE reconciled_json END WHERE id=? AND receipt_json IS NULL")
            .bind(serde_json::to_string(output)?).bind(settled(output)).bind(settled(output)).bind(id.to_string()).execute(&self.store.pool).await?;
        Ok(())
    }
    pub(super) async fn native_pending(&self, principal: Uuid, session: Uuid) -> Result<bool> {
        let count:i64=sqlx::query_scalar("SELECT COUNT(*) FROM cloud_native_operations WHERE principal=? AND source_session=? AND state!='confirmed'").bind(principal.to_string()).bind(session.to_string()).fetch_one(&self.store.pool).await?;
        Ok(count != 0)
    }
    pub(crate) async fn reconcile_native(&self) -> Result<()> {
        let rows:Vec<String>=sqlx::query_scalar("SELECT id FROM cloud_native_operations WHERE state!='confirmed' ORDER BY last_query_at,created_at LIMIT 16").fetch_all(&self.store.pool).await?;
        for id in rows {
            let id = Uuid::parse_str(&id).map_err(|_| TaskError::Invalid)?;
            sqlx::query("UPDATE cloud_native_operations SET last_query_at=? WHERE id=?")
                .bind(now())
                .bind(id.to_string())
                .execute(&self.store.pool)
                .await?;
            let _ = tokio::time::timeout(std::time::Duration::from_secs(2), self.query_native(id))
                .await;
        }
        Ok(())
    }
    async fn query_native(&self, id: Uuid) -> Result<()> {
        let row=sqlx::query("SELECT principal,source_session,frozen_json,invocation_id,tool FROM cloud_native_operations WHERE id=? AND state!='confirmed'").bind(id.to_string()).fetch_optional(&self.store.pool).await?;
        let Some(row) = row else {
            return Ok(());
        };
        let owner = Uuid::parse_str(row.get("principal")).map_err(|_| TaskError::Invalid)?;
        let frozen: crate::state::FrozenSession = serde_json::from_str(row.get("frozen_json"))?;
        let gateway = self.gateway.upgrade().ok_or(TaskError::Unavailable)?;
        let remote = gateway
            .reopen_native_target(owner, &frozen)
            .await
            .map_err(|_| TaskError::Unavailable)?;
        let invocation =
            Uuid::parse_str(row.get("invocation_id")).map_err(|_| TaskError::Invalid)?;
        let job = remote
            .query_execution(invocation)
            .await
            .map_err(|_| TaskError::Unavailable)?;
        if job.invocation_id != invocation
            || job.session_id != remote.binding().session_id
            || job.tool != row.get::<String, _>("tool")
        {
            return Err(TaskError::Forbidden);
        }
        let output = peri_remote_tools::snapshot_output(job);
        sqlx::query("UPDATE cloud_native_operations SET reconciled_json=?,state=? WHERE id=? AND state!='confirmed'")
            .bind(serde_json::to_string(&output)?)
            .bind(if settled(&output) {
                "confirmed"
            } else {
                "running_or_unknown"
            })
            .bind(id.to_string())
            .execute(&self.store.pool)
            .await?;
        Ok(())
    }
    pub(super) async fn native_history(
        &self,
        scope: &TaskScope,
        after: i64,
        limit: usize,
    ) -> Result<Value> {
        if !self
            .gateway
            .upgrade()
            .ok_or(TaskError::Unavailable)?
            .worker_scope_valid(scope)
            .await
            .map_err(|_| TaskError::Forbidden)?
        {
            return Err(TaskError::Forbidden);
        }
        let state = self
            .journal
            .session(scope.principal, scope.session)
            .await
            .map_err(|_| TaskError::Forbidden)?;
        let epoch = crate::memory::MemoryStore::new(&self.journal, scope.principal, 65536)
            .privacy_epoch()
            .await
            .map_err(|_| TaskError::Unavailable)?;
        let rows=sqlx::query("SELECT rowid,id,turn_id,task_id,device_id,frozen_json,invocation_id,tool,input_hash,state,receipt_json,reconciled_json FROM cloud_native_operations WHERE principal=? AND source_session=? AND context_generation=? AND privacy_epoch=? AND rowid>? ORDER BY rowid LIMIT ?")
            .bind(scope.principal.to_string()).bind(scope.session.to_string()).bind(state.context_generation as i64).bind(epoch as i64).bind(after).bind(limit.clamp(1,16) as i64+1).fetch_all(&self.store.pool).await?;
        let more = rows.len() > limit.clamp(1, 16);
        let devices = crate::memory::scope::devices(&self.store.pool, scope.principal)
            .await
            .map_err(|_| TaskError::Unavailable)?;
        let mut next = after;
        let mut entries = Vec::new();
        for row in rows.into_iter().take(limit.clamp(1, 16)) {
            next = row.get("rowid");
            let frozen: crate::state::FrozenSession = serde_json::from_str(row.get("frozen_json"))?;
            if !devices
                .iter()
                .any(|d| Some(d.id) == frozen.device_id && !d.revoked)
            {
                continue;
            }
            let raw: Option<String> = row
                .get::<Option<String>, _>("reconciled_json")
                .or(row.get("receipt_json"));
            let output: Option<ToolOutput> =
                raw.as_deref().map(serde_json::from_str).transpose()?;
            entries.push(json!({"cursor":row.get::<i64,_>("rowid"),"operation_id":row.get::<String,_>("id"),"turn_id":row.get::<String,_>("turn_id"),"task_id":row.get::<Option<String>,_>("task_id"),"device_id":row.get::<String,_>("device_id"),"workspace":frozen.workspace(),"invocation_id":row.get::<String,_>("invocation_id"),"tool":row.get::<String,_>("tool"),"input_hash":row.get::<String,_>("input_hash"),"state":row.get::<String,_>("state"),"execution":output.as_ref().and_then(|o|o.execution.as_ref()),"output_preview":output.as_ref().map(|o|clip(&o.text,1500)),"output_truncated":output.as_ref().is_some_and(|o|o.text.chars().count()>1500)}));
        }
        Ok(
            json!({"operations":entries,"next_cursor":next,"has_more":more,"missing_receipt":"unknown; query original identity, never replay"}),
        )
    }
    pub(super) async fn native_detail(
        &self,
        scope: &TaskScope,
        id: Uuid,
        view: &str,
        page: (usize, usize, Option<&str>),
    ) -> Result<Value> {
        let (offset, chars, version) = page;
        let state = self
            .journal
            .session(scope.principal, scope.session)
            .await
            .map_err(|_| TaskError::Forbidden)?;
        let epoch = crate::memory::MemoryStore::new(&self.journal, scope.principal, 65536)
            .privacy_epoch()
            .await
            .map_err(|_| TaskError::Unavailable)?;
        let row=sqlx::query("SELECT input_json,receipt_json,reconciled_json,device_id FROM cloud_native_operations WHERE id=? AND principal=? AND source_session=? AND context_generation=? AND privacy_epoch=?")
            .bind(id.to_string()).bind(scope.principal.to_string()).bind(scope.session.to_string()).bind(state.context_generation as i64).bind(epoch as i64).fetch_optional(&self.store.pool).await?.ok_or(TaskError::Forbidden)?;
        let devices = crate::memory::scope::devices(&self.store.pool, scope.principal)
            .await
            .map_err(|_| TaskError::Unavailable)?;
        if !devices
            .iter()
            .any(|d| d.id.to_string() == row.get::<String, _>("device_id") && !d.revoked)
        {
            return Err(TaskError::Forbidden);
        }
        let text = match view {
            "input" => row.get::<String, _>("input_json"),
            "output" => row
                .get::<Option<String>, _>("reconciled_json")
                .or(row.get("receipt_json"))
                .ok_or(TaskError::Unavailable)?,
            _ => return Err(TaskError::Invalid),
        };
        super::evidence::chunk(&text, offset, chars, version)
    }
}
pub(crate) async fn migrate(pool: &SqlitePool) -> crate::state::StateResult<()> {
    let version: i64 = sqlx::query_scalar("PRAGMA user_version")
        .fetch_one(pool)
        .await?;
    if version >= 22 {
        return Ok(());
    }
    if version != 21 {
        return Err(crate::state::StateError::Version);
    }
    let mut tx = pool.begin().await?;
    sqlx::query("CREATE TABLE cloud_device_targets(id TEXT PRIMARY KEY,principal TEXT NOT NULL REFERENCES principals(id),source_session TEXT NOT NULL REFERENCES cloud_sessions(id),turn_id TEXT NOT NULL REFERENCES cloud_turns(id),record_json TEXT NOT NULL)").execute(&mut *tx).await?;
    sqlx::query("CREATE TABLE cloud_native_operations(id TEXT PRIMARY KEY,principal TEXT NOT NULL REFERENCES principals(id),source_session TEXT NOT NULL REFERENCES cloud_sessions(id),turn_id TEXT NOT NULL REFERENCES cloud_turns(id),task_id TEXT REFERENCES agent_tasks(id),call_id TEXT NOT NULL,device_id TEXT NOT NULL,frozen_json TEXT NOT NULL,invocation_id TEXT NOT NULL,tool TEXT NOT NULL,input_json TEXT NOT NULL,input_hash TEXT NOT NULL,state TEXT NOT NULL,receipt_json TEXT,reconciled_json TEXT,context_generation INTEGER NOT NULL,privacy_epoch INTEGER NOT NULL,created_at INTEGER NOT NULL,last_query_at INTEGER NOT NULL DEFAULT 0,UNIQUE(source_session,turn_id,call_id))").execute(&mut *tx).await?;
    sqlx::query(
        "CREATE INDEX cloud_native_pending ON cloud_native_operations(principal,device_id,state)",
    )
    .execute(&mut *tx)
    .await?;
    sqlx::query("PRAGMA user_version=22")
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}
