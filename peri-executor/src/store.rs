use std::path::Path;

use chrono::Utc;
use sha2::Digest as _;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};
use sqlx::{Row, SqliteConnection, SqlitePool};
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
        if !(0..=2).contains(&version) {
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
        if version <= 1 {
            // 保留策略墓碑（P1-1）：过期 job 删除后保留 id/请求指纹/终态，
            // 使幂等受理与 GET 结算在保留窗口之后仍然成立。
            let mut tx = pool.begin().await?;
            sqlx::query(
                "CREATE TABLE IF NOT EXISTS tombstones (
                    id TEXT PRIMARY KEY, request_hash TEXT NOT NULL,
                    snapshot_json TEXT NOT NULL, created_at TEXT NOT NULL
                )",
            )
            .execute(&mut *tx)
            .await?;
            sqlx::query("PRAGMA user_version = 2")
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
        let mut connection = self.pool.acquire().await?;
        existing_on(&mut connection, request).await
    }

    pub async fn create_job(&self, request: &SubmitJob) -> Result<(bool, JobSnapshot)> {
        let mut tx = self.pool.begin().await?;
        if let Some(snapshot) = existing_on(&mut tx, request).await? {
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
        let (json,): (String,) = sqlx::query_as(
            "SELECT snapshot_json FROM jobs WHERE id = ?1
             UNION ALL SELECT snapshot_json FROM tombstones WHERE id = ?1",
        )
        .bind(id.to_string())
        .fetch_optional(&self.pool)
        .await?
        .ok_or(Error::NotFound)?;
        Ok(serde_json::from_str(&json)?)
    }

    /// Compact old terminal jobs while retaining their exact invocation identity and status.
    pub async fn enforce_retention(&self, root: &Path) -> Result<()> {
        let cutoff = (Utc::now() - chrono::Duration::seconds(RETENTION_MAX_AGE_SECS)).to_rfc3339();
        let rows = sqlx::query(
            "WITH terminal AS (
                SELECT id, rowid, COALESCE(
                    julianday(json_extract(snapshot_json, '$.finished_at')) < julianday(?1), 0
                ) AS expired
                FROM jobs WHERE status IN (?2, ?3, ?4, ?5)
            ), ranked AS (
                SELECT id, expired, ROW_NUMBER() OVER (ORDER BY expired, rowid DESC) AS rank
                FROM terminal
            )
            SELECT id FROM ranked WHERE expired OR rank > ?6",
        )
        .bind(cutoff)
        .bind(status_key(JobStatus::Completed)?)
        .bind(status_key(JobStatus::Failed)?)
        .bind(status_key(JobStatus::Cancelled)?)
        .bind(status_key(JobStatus::TimedOut)?)
        .bind(RETENTION_MAX_JOBS as i64)
        .fetch_all(&self.pool)
        .await?;

        let logs_root = root.join("logs");
        for row in rows {
            let id = row.get::<String, _>("id");
            let Ok(invocation) = Uuid::parse_str(&id) else {
                tracing::warn!("retention skipped invalid invocation ID");
                continue;
            };
            match self.compact_terminal_job(&id).await {
                Ok(true) => remove_job_logs(&logs_root.join(invocation.to_string())).await,
                Ok(false) => {}
                Err(error) => tracing::warn!(%invocation, %error, "executor retention failed"),
            }
        }
        cleanup_orphan_logs(&logs_root, &self.pool).await;
        Ok(())
    }

    async fn compact_terminal_job(&self, id: &str) -> Result<bool> {
        let mut tx = self.pool.begin().await?;
        let row = sqlx::query("SELECT request_json, snapshot_json FROM jobs WHERE id = ?")
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?;
        let Some(row) = row else {
            return Ok(false);
        };
        let mut snapshot: JobSnapshot = serde_json::from_str(row.get("snapshot_json"))?;
        if !snapshot.status.is_terminal() {
            return Ok(false);
        }
        let request: SubmitJob = serde_json::from_str(row.get("request_json"))?;
        snapshot.stdout_path = None;
        snapshot.stderr_path = None;
        let message = "Invocation settled. Its captured output and logs expired.";
        snapshot.output = snapshot.output.and_then(|output| {
            output.execution.map(|mut evidence| {
                evidence.output_ref = None;
                evidence.output_truncated = true;
                peri_acp_types::tools::ToolOutput::with_execution(message, evidence)
            })
        });
        snapshot.failure.get_or_insert_with(|| JobFailure {
            code: "retention_expired".into(),
            message: message.into(),
        });
        sqlx::query("INSERT INTO tombstones (id, request_hash, snapshot_json, created_at) VALUES (?, ?, ?, ?)")
            .bind(id)
            .bind(request_hash(&request)?)
            .bind(serde_json::to_string(&snapshot)?)
            .bind(Utc::now().to_rfc3339())
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM jobs WHERE id = ?")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(true)
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

async fn existing_on(
    connection: &mut SqliteConnection,
    request: &SubmitJob,
) -> Result<Option<JobSnapshot>> {
    let row = sqlx::query(
        "SELECT request_json, NULL AS request_hash, snapshot_json FROM jobs WHERE id = ?1
         UNION ALL SELECT NULL, request_hash, snapshot_json FROM tombstones WHERE id = ?1",
    )
    .bind(request.invocation_id.to_string())
    .fetch_optional(connection)
    .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    let matches = if let Some(original) = row.get::<Option<String>, _>("request_json") {
        serde_json::from_str::<SubmitJob>(&original)? == *request
    } else {
        row.get::<String, _>("request_hash") == request_hash(request)?
    };
    if !matches {
        return Err(Error::InvocationConflict);
    }
    Ok(Some(serde_json::from_str(row.get("snapshot_json"))?))
}

async fn remove_job_logs(path: &Path) {
    if let Err(error) = tokio::fs::remove_dir_all(path).await {
        if error.kind() != std::io::ErrorKind::NotFound {
            tracing::warn!(dir = %path.display(), %error, "executor log cleanup failed");
        }
    }
}

fn status_key(status: JobStatus) -> Result<String> {
    Ok(serde_json::to_string(&status)?)
}

/// 保留策略常量（executor review 2026-10-26 P1-1，参照 reach 量级）：
/// 终态 job 最多保 1000 条 / 14 天，超限移入墓碑并删除日志目录。
pub(crate) const RETENTION_MAX_JOBS: usize = 1_000;
pub(crate) const RETENTION_MAX_AGE_SECS: i64 = 14 * 24 * 60 * 60;

fn request_hash(request: &SubmitJob) -> Result<String> {
    let mut value = serde_json::to_value(request)?;
    value.sort_all_objects();
    let bytes = serde_json::to_vec(&value)?;
    let digest: [u8; 32] = sha2::Sha256::digest(&bytes).into();
    let mut hex = String::with_capacity(64);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(hex, "{byte:02x}");
    }
    Ok(hex)
}

/// 删除「没有 job 行、且目录修改时间超过保留年龄」的孤儿日志目录。
async fn cleanup_orphan_logs(logs_root: &Path, pool: &SqlitePool) {
    let Ok(entries) = tokio::fs::read_dir(logs_root).await else {
        return;
    };
    let cutoff = Utc::now() - chrono::Duration::seconds(RETENTION_MAX_AGE_SECS);
    let mut stale: Vec<std::path::PathBuf> = Vec::new();
    let mut entries = entries;
    while let Ok(Some(entry)) = entries.next_entry().await {
        let path = entry.path();
        if !entry.file_type().await.is_ok_and(|kind| kind.is_dir()) {
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if Uuid::parse_str(name).is_err() {
            continue;
        }
        let known = sqlx::query("SELECT 1 FROM jobs WHERE id = ?")
            .bind(name)
            .fetch_optional(pool)
            .await
            .map(|row| row.is_some())
            .unwrap_or(true); // 查询失败时保守保留
        if known {
            continue;
        }
        let old = entry
            .metadata()
            .await
            .ok()
            .and_then(|meta| meta.modified().ok())
            .and_then(|modified| {
                modified
                    .duration_since(std::time::UNIX_EPOCH)
                    .ok()
                    .map(|elapsed| chrono::DateTime::from_timestamp(elapsed.as_secs() as i64, 0))
            })
            .flatten()
            .is_some_and(|modified| modified < cutoff);
        if old {
            stale.push(path);
        }
    }
    for path in stale {
        remove_job_logs(&path).await;
    }
}

#[cfg(test)]
#[path = "store_test.rs"]
mod tests;
