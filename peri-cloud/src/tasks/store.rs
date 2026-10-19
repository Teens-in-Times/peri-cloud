use super::*;
use sqlx::{Row, SqlitePool};

#[derive(Clone)]
pub(super) struct TaskStore {
    pub pool: SqlitePool,
}
impl TaskStore {
    pub async fn get(&self, owner: Uuid, id: Uuid) -> Result<TaskRecord> {
        let raw: String =
            sqlx::query_scalar("SELECT record_json FROM agent_tasks WHERE id=? AND principal=?")
                .bind(id.to_string())
                .bind(owner.to_string())
                .fetch_optional(&self.pool)
                .await?
                .ok_or(TaskError::Forbidden)?;
        Ok(serde_json::from_str(&raw)?)
    }
    pub async fn by_session(&self, owner: Uuid, session: Uuid) -> Result<Option<TaskRecord>> {
        let raw: Option<String> = sqlx::query_scalar(
            "SELECT record_json FROM agent_tasks WHERE worker_session=? AND principal=?",
        )
        .bind(session.to_string())
        .bind(owner.to_string())
        .fetch_optional(&self.pool)
        .await?;
        raw.map(|v| serde_json::from_str(&v).map_err(TaskError::from))
            .transpose()
    }
    pub async fn existing(&self, scope: &TaskScope, key: &str) -> Result<Option<TaskRecord>> {
        let raw: Option<String> = sqlx::query_scalar("SELECT record_json FROM agent_tasks WHERE principal=? AND parent_session=? AND request_key=?").bind(scope.principal.to_string()).bind(scope.session.to_string()).bind(key).fetch_optional(&self.pool).await?;
        raw.map(|v| serde_json::from_str(&v).map_err(TaskError::from))
            .transpose()
    }
    pub async fn insert(&self, record: &TaskRecord) -> Result<()> {
        sqlx::query("INSERT INTO agent_tasks(id,principal,parent_session,worker_session,request_key,record_json) VALUES(?,?,?,?,?,?)").bind(record.id.to_string()).bind(record.principal.to_string()).bind(record.parent_session.to_string()).bind(record.worker_session.to_string()).bind(&record.request_key).bind(serde_json::to_string(record)?).execute(&self.pool).await?;
        Ok(())
    }
    pub async fn save(&self, record: &mut TaskRecord) -> Result<()> {
        let previous = record.revision;
        record.revision = record.revision.saturating_add(1);
        let updated = sqlx::query("UPDATE agent_tasks SET record_json=? WHERE id=? AND principal=? AND json_extract(record_json,'$.revision')=?").bind(serde_json::to_string(record)?).bind(record.id.to_string()).bind(record.principal.to_string()).bind(previous as i64).execute(&self.pool).await?.rows_affected();
        if updated != 1 {
            return Err(TaskError::Conflict);
        }
        Ok(())
    }
    pub async fn list(&self, scope: &TaskScope) -> Result<Vec<TaskRecord>> {
        let rows: Vec<String> = sqlx::query_scalar("SELECT record_json FROM agent_tasks WHERE principal=? AND parent_session=? ORDER BY (json_extract(record_json,'$.state') IN ('completed','failed','cancelled','recovery_required','invalidated')),rowid DESC LIMIT 64").bind(scope.principal.to_string()).bind(scope.root_session.to_string()).fetch_all(&self.pool).await?;
        rows.into_iter()
            .map(|raw| serde_json::from_str(&raw).map_err(TaskError::from))
            .collect()
    }
    pub async fn unfinished(&self, after: i64) -> Result<Vec<(i64, TaskRecord)>> {
        let rows: Vec<(i64,String)> = sqlx::query_as("SELECT rowid,record_json FROM agent_tasks WHERE rowid>? AND json_extract(record_json,'$.notified')=0 ORDER BY rowid LIMIT 128").bind(after).fetch_all(&self.pool).await?;
        rows.into_iter()
            .map(|(id, raw)| {
                serde_json::from_str(&raw)
                    .map(|record| (id, record))
                    .map_err(TaskError::from)
            })
            .collect()
    }
    pub async fn message(
        &self,
        task: Uuid,
        key: &str,
        direction: &str,
        body: &str,
    ) -> Result<serde_json::Value> {
        if body.trim().is_empty() || body.len() > 16384 || serde_json::to_vec(body)?.len() > 16384 {
            return Err(TaskError::Invalid);
        }
        let mut tx = self.pool.begin().await?;
        let count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM agent_task_messages WHERE task_id=?")
                .bind(task.to_string())
                .fetch_one(&mut *tx)
                .await?;
        let exists: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM agent_task_messages WHERE task_id=? AND request_key=?",
        )
        .bind(task.to_string())
        .bind(key)
        .fetch_one(&mut *tx)
        .await?;
        if count >= 256 && exists == 0 {
            return Err(TaskError::Limit);
        }
        sqlx::query("INSERT OR IGNORE INTO agent_task_messages(task_id,request_key,direction,body,state) VALUES(?,?,?,?,'submitted')").bind(task.to_string()).bind(key).bind(direction).bind(body).execute(&mut *tx).await?;
        let row = sqlx::query("SELECT seq,body,direction,state FROM agent_task_messages WHERE task_id=? AND request_key=?").bind(task.to_string()).bind(key).fetch_one(&mut *tx).await?;
        if row.get::<String, _>("body") != body || row.get::<String, _>("direction") != direction {
            return Err(TaskError::Conflict);
        }
        let result = serde_json::json!({"message_id":row.get::<i64,_>("seq"),"state":row.get::<String,_>("state")});
        tx.commit().await?;
        Ok(result)
    }
    pub async fn pending_messages(
        &self,
        task: Uuid,
        direction: &str,
    ) -> Result<Vec<serde_json::Value>> {
        let rows = sqlx::query("SELECT seq,body FROM agent_task_messages WHERE task_id=? AND direction=? AND state='submitted' ORDER BY seq LIMIT 16").bind(task.to_string()).bind(direction).fetch_all(&self.pool).await?;
        Ok(rows.into_iter().map(|row|serde_json::json!({"message_id":row.get::<i64,_>("seq"),"body":row.get::<String,_>("body")})).collect())
    }
    pub async fn parent_context(&self, task: Uuid) -> Result<Vec<serde_json::Value>> {
        let rows=sqlx::query("SELECT seq,body,state FROM (SELECT seq,body,state FROM agent_task_messages WHERE task_id=? AND direction='parent' ORDER BY seq DESC LIMIT 8) ORDER BY seq")
            .bind(task.to_string()).fetch_all(&self.pool).await?;
        Ok(rows.into_iter().map(|row|{let body:String=row.get("body");serde_json::json!({"message_id":row.get::<i64,_>("seq"),"body_preview":clip(&body,200),"body_bytes":body.len(),"truncated":body.chars().count()>200,"state":row.get::<String,_>("state")})}).collect())
    }
    pub async fn consume(&self, task: Uuid, ids: &[i64]) -> Result<()> {
        for id in ids {
            sqlx::query("UPDATE agent_task_messages SET state='consumed' WHERE task_id=? AND seq=? AND state='submitted'").bind(task.to_string()).bind(id).execute(&self.pool).await?;
        }
        Ok(())
    }
    pub async fn append_trace(&self, task: Uuid, event: serde_json::Value) -> Result<()> {
        // Retain bounded observation previews. Canonical turn history remains the detailed evidence.
        let mut tx = self.pool.begin().await?;
        sqlx::query("INSERT INTO agent_task_trace(task_id,event_json) VALUES(?,?)")
            .bind(task.to_string())
            .bind(serde_json::to_string(&event)?)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM agent_task_trace WHERE task_id=? AND seq NOT IN (SELECT seq FROM agent_task_trace WHERE task_id=? ORDER BY seq DESC LIMIT 1024)").bind(task.to_string()).bind(task.to_string()).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(())
    }
    pub async fn trace(&self, task: Uuid, after: i64, limit: usize) -> Result<serde_json::Value> {
        let rows=sqlx::query("SELECT seq,event_json FROM agent_task_trace WHERE task_id=? AND seq>? ORDER BY seq LIMIT ?").bind(task.to_string()).bind(after).bind(limit.clamp(1,32) as i64).fetch_all(&self.pool).await?;
        let entries: Vec<_> = rows.into_iter().map(|r| serde_json::json!({"cursor":r.get::<i64,_>("seq"),"event":serde_json::from_str::<serde_json::Value>(r.get("event_json")).unwrap_or_default()})).collect();
        let next = entries
            .last()
            .and_then(|e| e["cursor"].as_i64())
            .unwrap_or(after);
        Ok(
            serde_json::json!({"entries":entries,"next_cursor":next,"retention":"latest_1024_previews","raw":"inspect conversation for retained canonical evidence"}),
        )
    }
}
