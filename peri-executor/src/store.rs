use std::path::Path;

use chrono::Utc;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};
use sqlx::{Row, SqlitePool};
use uuid::Uuid;

use crate::{Error, Result};
use peri_acp_types::device_executor::{
    JobFailure, JobSnapshot, JobStatus, SessionBinding, SubmitJob,
};

#[derive(Clone)]
pub(crate) struct Store {
    pool: SqlitePool,
}

impl Store {
    pub async fn open(root: &Path) -> Result<Self> {
        let options = SqliteConnectOptions::new()
            .filename(root.join("jobs.sqlite"))
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .synchronous(SqliteSynchronous::Full)
            .foreign_keys(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await?;
        let (version,): (i64,) = sqlx::query_as("PRAGMA user_version")
            .fetch_one(&pool)
            .await?;
        if !matches!(version, 0 | 1) {
            return Err(Error::Invalid(
                "unsupported executor database version".into(),
            ));
        }
        if version == 0 {
            let mut tx = pool.begin().await?;
            sqlx::query("CREATE TABLE sessions (id TEXT PRIMARY KEY, workspace TEXT NOT NULL, identity_json TEXT NOT NULL)")
                .execute(&mut *tx)
                .await?;
            sqlx::query(
                "CREATE TABLE jobs (
                    id TEXT PRIMARY KEY, session_id TEXT NOT NULL REFERENCES sessions(id),
                    request_json TEXT NOT NULL, snapshot_json TEXT NOT NULL, status TEXT NOT NULL
                )",
            )
            .execute(&mut *tx)
            .await?;
            sqlx::query("CREATE INDEX jobs_session_status ON jobs(session_id, status)")
                .execute(&mut *tx)
                .await?;
            sqlx::query("PRAGMA user_version = 1")
                .execute(&mut *tx)
                .await?;
            tx.commit().await?;
        }
        let store = Self { pool };
        store.recover_interrupted().await?;
        Ok(store)
    }

    pub async fn close(&self) {
        self.pool.close().await;
    }

    pub async fn bind(&self, session: SessionBinding) -> Result<SessionBinding> {
        sqlx::query(
            "INSERT OR IGNORE INTO sessions (id, workspace, identity_json) VALUES (?, ?, ?)",
        )
        .bind(session.session_id.to_string())
        .bind(&session.workspace)
        .bind(serde_json::to_string(&session.workspace_identity)?)
        .execute(&self.pool)
        .await?;
        let bound = self.session(session.session_id).await?;
        if bound != session {
            return Err(Error::SessionConflict);
        }
        Ok(bound)
    }

    pub async fn session(&self, id: Uuid) -> Result<SessionBinding> {
        let row = sqlx::query("SELECT workspace, identity_json FROM sessions WHERE id = ?")
            .bind(id.to_string())
            .fetch_optional(&self.pool)
            .await?
            .ok_or(Error::NotFound)?;
        Ok(SessionBinding {
            session_id: id,
            workspace: row.get("workspace"),
            workspace_identity: serde_json::from_str(row.get("identity_json"))?,
        })
    }

    pub async fn existing_job(&self, request: &SubmitJob) -> Result<Option<JobSnapshot>> {
        let row = sqlx::query("SELECT request_json, snapshot_json FROM jobs WHERE id = ?")
            .bind(request.invocation_id.to_string())
            .fetch_optional(&self.pool)
            .await?;
        match row {
            Some(row) => {
                let original: SubmitJob = serde_json::from_str(row.get("request_json"))?;
                if original != *request {
                    return Err(Error::InvocationConflict);
                }
                Ok(Some(serde_json::from_str(row.get("snapshot_json"))?))
            }
            None => Ok(None),
        }
    }

    pub async fn create_job(&self, request: &SubmitJob) -> Result<(bool, JobSnapshot)> {
        let mut tx = self.pool.begin().await?;
        if let Some(row) = sqlx::query("SELECT request_json, snapshot_json FROM jobs WHERE id = ?")
            .bind(request.invocation_id.to_string())
            .fetch_optional(&mut *tx)
            .await?
        {
            let original: SubmitJob = serde_json::from_str(row.get("request_json"))?;
            if original != *request {
                return Err(Error::InvocationConflict);
            }
            let snapshot = serde_json::from_str(row.get("snapshot_json"))?;
            tx.commit().await?;
            return Ok((false, snapshot));
        }
        let (needs_recovery,): (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM jobs WHERE session_id = ? AND status = ?")
                .bind(request.session_id.to_string())
                .bind(status_key(JobStatus::RecoveryRequired)?)
                .fetch_one(&mut *tx)
                .await?;
        if needs_recovery > 0 {
            return Err(Error::RecoveryRequired);
        }
        let snapshot = JobSnapshot {
            invocation_id: request.invocation_id,
            session_id: request.session_id,
            tool: request.tool.clone(),
            status: JobStatus::Queued,
            cancel_requested: false,
            submitted_at: Utc::now().to_rfc3339(),
            started_at: None,
            finished_at: None,
            pid: None,
            stdout_path: None,
            stderr_path: None,
            output: None,
            failure: None,
        };
        sqlx::query(
            "INSERT INTO jobs (id, session_id, request_json, snapshot_json, status) VALUES (?, ?, ?, ?, ?)",
        )
        .bind(request.invocation_id.to_string())
        .bind(request.session_id.to_string())
        .bind(serde_json::to_string(request)?)
        .bind(serde_json::to_string(&snapshot)?)
        .bind(status_key(snapshot.status)?)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok((true, snapshot))
    }

    pub async fn job(&self, id: Uuid) -> Result<JobSnapshot> {
        let (json,): (String,) = sqlx::query_as("SELECT snapshot_json FROM jobs WHERE id = ?")
            .bind(id.to_string())
            .fetch_optional(&self.pool)
            .await?
            .ok_or(Error::NotFound)?;
        Ok(serde_json::from_str(&json)?)
    }

    pub async fn session_jobs(&self, id: Uuid) -> Result<Vec<JobSnapshot>> {
        let rows = sqlx::query(
            "SELECT snapshot_json FROM jobs WHERE session_id = ? ORDER BY rowid DESC LIMIT 100",
        )
        .bind(id.to_string())
        .fetch_all(&self.pool)
        .await?;
        rows.iter()
            .map(|row| serde_json::from_str(row.get("snapshot_json")).map_err(Error::from))
            .collect()
    }

    pub async fn update(
        &self,
        id: Uuid,
        mutate: impl FnOnce(&mut JobSnapshot),
    ) -> Result<JobSnapshot> {
        let mut tx = self.pool.begin().await?;
        let (json,): (String,) = sqlx::query_as("SELECT snapshot_json FROM jobs WHERE id = ?")
            .bind(id.to_string())
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(Error::NotFound)?;
        let mut snapshot: JobSnapshot = serde_json::from_str(&json)?;
        mutate(&mut snapshot);
        sqlx::query("UPDATE jobs SET snapshot_json = ?, status = ? WHERE id = ?")
            .bind(serde_json::to_string(&snapshot)?)
            .bind(status_key(snapshot.status)?)
            .bind(id.to_string())
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(snapshot)
    }

    async fn recover_interrupted(&self) -> Result<()> {
        let rows = sqlx::query("SELECT id FROM jobs WHERE status IN (?, ?)")
            .bind(status_key(JobStatus::Queued)?)
            .bind(status_key(JobStatus::Running)?)
            .fetch_all(&self.pool)
            .await?;
        for row in rows {
            let id: String = row.get("id");
            let id = Uuid::parse_str(&id)
                .map_err(|_| Error::Invalid("invalid persisted invocation identity".into()))?;
            self.update(id, |job| {
                if job.status == JobStatus::Queued {
                    job.status = JobStatus::Cancelled;
                    job.finished_at = Some(Utc::now().to_rfc3339());
                    job.failure = Some(JobFailure {
                        code: "executor_restarted_before_start".into(),
                        message: "Executor restarted before this invocation began; it was not replayed.".into(),
                    });
                } else {
                    job.status = JobStatus::RecoveryRequired;
                    job.failure = Some(JobFailure {
                        code: "execution_state_unknown_after_restart".into(),
                        message: "Executor restarted while this invocation was running. Inspect the recorded process and logs before recovering this session.".into(),
                    });
                }
            }).await?;
        }
        Ok(())
    }
}

fn status_key(status: JobStatus) -> Result<String> {
    Ok(serde_json::to_string(&status)?)
}

#[cfg(test)]
#[path = "store_test.rs"]
mod tests;
