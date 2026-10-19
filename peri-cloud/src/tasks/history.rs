//! Read-only history across explicitly shared execution context; mutations stay task-local.
use super::*;
use serde_json::{json, Value};

impl WorkerService {
    pub(super) async fn readable(&self, scope: &TaskScope, id: Uuid) -> Result<TaskRecord> {
        match self.authorized(scope, id).await {
            Ok(record) => return Ok(record),
            Err(TaskError::Forbidden) => (),
            Err(error) => return Err(error),
        }
        let record = self.store.get(scope.principal, id).await?;
        let execution = self
            .execution_session(scope.principal, record.execution_session)
            .await?;
        let gateway = self.gateway.upgrade().ok_or(TaskError::Unavailable)?;
        let private = gateway.worker_private_route(&scope.route);
        if !execution.sources.contains_key(&scope.root_session)
            || private != (execution.visibility == "private")
            || (!private && record.route != scope.route)
        {
            return Err(TaskError::Forbidden);
        }
        if let Some(task) = scope.task {
            let current = self.authorized(scope, task).await?;
            if current.execution_session != record.execution_session {
                return Err(TaskError::Forbidden);
            }
        } else if !gateway
            .worker_scope_valid(scope)
            .await
            .map_err(|_| TaskError::Forbidden)?
        {
            return Err(TaskError::Forbidden);
        }
        // Explicit reuse already shares this execution context. Historical
        // reads never grant message/control/write rights to a different task.
        self.authorized(&self.scope_record(&record), id).await
    }
    pub(super) async fn task_history_page(
        &self,
        scope: &TaskScope,
        execution: Option<Uuid>,
        after: i64,
        limit: usize,
    ) -> Result<Value> {
        if after < 0 || !(1..=32).contains(&limit) {
            return Err(TaskError::Invalid);
        }
        let rows:Vec<(i64,String)>=match execution {
            Some(execution)=>sqlx::query_as("SELECT rowid,record_json FROM agent_tasks WHERE principal=? AND json_extract(record_json,'$.execution_session')=? AND (?=0 OR rowid<?) ORDER BY rowid DESC LIMIT ?")
                .bind(scope.principal.to_string()).bind(execution.to_string()).bind(after).bind(after).bind(limit as i64+1).fetch_all(&self.store.pool).await?,
            None=>sqlx::query_as("SELECT rowid,record_json FROM agent_tasks WHERE principal=? AND parent_session=? AND (?=0 OR rowid<?) ORDER BY rowid DESC LIMIT ?")
                .bind(scope.principal.to_string()).bind(scope.root_session.to_string()).bind(after).bind(after).bind(limit as i64+1).fetch_all(&self.store.pool).await?,
        };
        let more = rows.len() > limit;
        let mut tasks = Vec::new();
        let mut cursor = after;
        for (seq, raw) in rows.into_iter().take(limit) {
            cursor = seq;
            let record: TaskRecord = serde_json::from_str(&raw)?;
            match self.readable(scope, record.id).await {
                Ok(record) => {
                    let mut summary = self.summary(&record);
                    summary["cursor"] = json!(seq);
                    tasks.push(summary);
                }
                Err(TaskError::Forbidden) => (),
                Err(error) => return Err(error),
            }
        }
        Ok(
            json!({"tasks":tasks,"next_cursor":cursor,"has_more":more,"order":"newest_first","previews":true,"scope":if execution.is_some(){"explicitly_shared_execution_session"}else{"current_main_conversation"}}),
        )
    }
}
