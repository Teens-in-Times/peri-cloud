//! Running native operations keep their old window identity after context rotation.
use super::*;
use sqlx::{Row, SqlitePool};

impl WorkerService {
    pub(super) async fn cancel_task_native(&self, task: Uuid) -> Result<bool> {
        let total: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM cloud_native_operations WHERE task_id=?")
                .bind(task.to_string())
                .fetch_one(&self.store.pool)
                .await?;
        let rows=sqlx::query("SELECT id,principal,frozen_json,invocation_id FROM cloud_native_operations WHERE task_id=? AND state!='confirmed' AND NOT EXISTS(SELECT 1 FROM cloud_native_cancellations c WHERE c.operation_id=cloud_native_operations.id) LIMIT 16").bind(task.to_string()).fetch_all(&self.store.pool).await?;
        for row in rows {
            let id: String = row.get("id");
            // One persisted attempt only. A lost response leads to queries of
            // the original operation, never blind cancellation resubmission.
            let inserted=sqlx::query("INSERT OR IGNORE INTO cloud_native_cancellations(operation_id,created_at,state) VALUES(?,?,'submitted_unknown')").bind(&id).bind(now()).execute(&self.store.pool).await?.rows_affected();
            if inserted == 0 {
                continue;
            }
            let owner = Uuid::parse_str(row.get("principal")).map_err(|_| TaskError::Invalid)?;
            let frozen: crate::state::FrozenSession = serde_json::from_str(row.get("frozen_json"))?;
            let gateway = self.gateway.upgrade().ok_or(TaskError::Unavailable)?;
            let attempted = async {
                let remote = gateway
                    .reopen_native_target(owner, &frozen)
                    .await
                    .map_err(|_| TaskError::Unavailable)?;
                let invocation =
                    Uuid::parse_str(row.get("invocation_id")).map_err(|_| TaskError::Invalid)?;
                remote
                    .cancel_execution(invocation)
                    .await
                    .map_err(|_| TaskError::Unavailable)
            };
            if let Ok(Ok(job)) =
                tokio::time::timeout(std::time::Duration::from_secs(3), attempted).await
            {
                sqlx::query("UPDATE cloud_native_cancellations SET state='response_received',receipt_json=? WHERE operation_id=?").bind(serde_json::to_string(&job)?).bind(id).execute(&self.store.pool).await?;
            }
        }
        Ok(total != 0)
    }
}
pub(crate) async fn migrate(pool: &SqlitePool) -> crate::state::StateResult<()> {
    let version: i64 = sqlx::query_scalar("PRAGMA user_version")
        .fetch_one(pool)
        .await?;
    if version >= 23 {
        return Ok(());
    }
    if version != 22 {
        return Err(crate::state::StateError::Version);
    }
    let mut tx = pool.begin().await?;
    sqlx::query("CREATE TABLE cloud_native_cancellations(operation_id TEXT PRIMARY KEY REFERENCES cloud_native_operations(id),created_at INTEGER NOT NULL,state TEXT NOT NULL,receipt_json TEXT)").execute(&mut *tx).await?;
    sqlx::query("PRAGMA user_version=23")
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}
