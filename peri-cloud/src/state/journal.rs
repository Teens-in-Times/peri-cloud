use std::fs::{File, OpenOptions};
use std::path::Path;

use fs2::FileExt;
use peri_acp_types::device_executor::{JobSnapshot, JobStatus};
use peri_acp_types::messages::MessageContent;
use peri_acp_types::permission::PermissionMode;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};
use sqlx::{Row, SqlitePool};
use uuid::Uuid;

use super::{
    Admission, FrozenSession, SessionState, StateError, StateResult, TurnRecord, TurnState,
};
use crate::CloudTurnResult;

/// A deployment holds this journal until its owned tasks have settled. The file
/// lock prevents another process from recovering a still-live owner's turns.
pub struct CloudJournal {
    pool: SqlitePool,
    _owner: File,
    gateway_claimed: std::sync::atomic::AtomicBool,
}

impl CloudJournal {
    pub async fn open(root: &Path) -> StateResult<Self> {
        let owner_root = root.to_owned();
        let owner = tokio::task::spawn_blocking(move || open_owner(&owner_root))
            .await
            .map_err(|_| {
                StateError::Io(std::io::Error::other("cloud owner initialization failed"))
            })??;
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(
                SqliteConnectOptions::new()
                    .filename(root.join("cloud.sqlite"))
                    .create_if_missing(true)
                    .journal_mode(SqliteJournalMode::Wal)
                    .synchronous(SqliteSynchronous::Full)
                    .foreign_keys(true),
            )
            .await?;
        let (version,): (i64,) = sqlx::query_as("PRAGMA user_version")
            .fetch_one(&pool)
            .await?;
        if !matches!(version, 0..=3) {
            return Err(StateError::Version);
        }
        if version == 0 {
            let mut tx = pool.begin().await?;
            sqlx::query("CREATE TABLE cloud_sessions (id TEXT PRIMARY KEY, principal TEXT NOT NULL, state_json TEXT NOT NULL)").execute(&mut *tx).await?;
            sqlx::query("CREATE TABLE cloud_turns (id TEXT PRIMARY KEY, session_id TEXT NOT NULL REFERENCES cloud_sessions(id), request_key TEXT NOT NULL, state TEXT NOT NULL, record_json TEXT NOT NULL, UNIQUE(session_id, request_key))").execute(&mut *tx).await?;
            sqlx::query("CREATE INDEX cloud_turns_state ON cloud_turns(session_id,state)")
                .execute(&mut *tx)
                .await?;
            sqlx::query("PRAGMA user_version = 1")
                .execute(&mut *tx)
                .await?;
            tx.commit().await?;
        }
        crate::identity::store::migrate(&pool).await?;
        crate::gateway::store::migrate(&pool).await?;
        let journal = Self {
            pool,
            _owner: owner,
            gateway_claimed: std::sync::atomic::AtomicBool::new(false),
        };
        journal.recover().await?;
        Ok(journal)
    }

    pub async fn close(&self) {
        self.pool.close().await;
    }

    pub(crate) fn identity_pool(&self) -> SqlitePool {
        self.pool.clone()
    }

    pub(crate) fn claim_gateway(&self) -> bool {
        !self
            .gateway_claimed
            .swap(true, std::sync::atomic::Ordering::AcqRel)
    }

    pub async fn has_unconfirmed_turn(&self, session: Uuid) -> StateResult<bool> {
        let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM cloud_turns WHERE session_id=? AND state IN ('queued','running','recovery_required')")
            .bind(session.to_string()).fetch_one(&self.pool).await?;
        Ok(count > 0)
    }

    pub async fn bind(
        &self,
        frozen: FrozenSession,
        mode: PermissionMode,
    ) -> StateResult<SessionState> {
        let state = SessionState {
            frozen: frozen.clone(),
            permission_mode: mode as u8,
            history: Vec::new(),
            history_flags: Default::default(),
            unsettled_jobs: Some(Vec::new()),
            revision: 0,
        };
        sqlx::query("INSERT OR IGNORE INTO cloud_sessions(id,principal,state_json) VALUES(?,?,?)")
            .bind(frozen.session_id.to_string())
            .bind(frozen.principal_id.to_string())
            .bind(serde_json::to_string(&state)?)
            .execute(&self.pool)
            .await?;
        let existing = self.session(frozen.principal_id, frozen.session_id).await?;
        if existing.frozen != frozen {
            return Err(StateError::BindingConflict);
        }
        Ok(existing)
    }

    pub async fn session(&self, principal: Uuid, id: Uuid) -> StateResult<SessionState> {
        let row = sqlx::query("SELECT principal,state_json FROM cloud_sessions WHERE id=?")
            .bind(id.to_string())
            .fetch_optional(&self.pool)
            .await?
            .ok_or(StateError::NotFound)?;
        if row.get::<String, _>("principal") != principal.to_string() {
            return Err(StateError::Forbidden);
        }
        Ok(serde_json::from_str(row.get("state_json"))?)
    }

    /// The exact request and turn identity are committed before any execution.
    pub async fn admit(
        &self,
        principal: Uuid,
        session_id: Uuid,
        request_key: String,
        prompt: MessageContent,
    ) -> StateResult<Admission> {
        if request_key.is_empty() || request_key.len() > 512 {
            return Err(StateError::InvocationConflict);
        }
        let mut tx = self.pool.begin().await?;
        let row = sqlx::query("SELECT principal,state_json FROM cloud_sessions WHERE id=?")
            .bind(session_id.to_string())
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(StateError::NotFound)?;
        if row.get::<String, _>("principal") != principal.to_string() {
            return Err(StateError::Forbidden);
        }
        let session: SessionState = serde_json::from_str(row.get("state_json"))?;
        if let Some(row) =
            sqlx::query("SELECT record_json FROM cloud_turns WHERE session_id=? AND request_key=?")
                .bind(session_id.to_string())
                .bind(&request_key)
                .fetch_optional(&mut *tx)
                .await?
        {
            let turn: TurnRecord = serde_json::from_str(row.get("record_json"))?;
            if serde_json::to_value(&turn.prompt)? != serde_json::to_value(&prompt)? {
                return Err(StateError::InvocationConflict);
            }
            tx.commit().await?;
            return Ok(Admission {
                created: false,
                turn,
                session,
            });
        }
        let (active,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM cloud_turns WHERE session_id=? AND state IN ('queued','running','recovery_required')")
            .bind(session_id.to_string()).fetch_one(&mut *tx).await?;
        if active != 0 || !session.execution_settled() {
            return Err(StateError::Busy);
        }
        session.permissions()?;
        let turn = TurnRecord {
            turn_id: Uuid::new_v4(),
            session_id,
            request_key,
            prompt,
            permission_mode: session.permission_mode,
            base_revision: session.revision,
            state: TurnState::Queued,
            replies: Vec::new(),
            cancel_requested: false,
        };
        sqlx::query("INSERT INTO cloud_turns(id,session_id,request_key,state,record_json) VALUES(?,?,?,?,?)")
            .bind(turn.turn_id.to_string()).bind(session_id.to_string()).bind(&turn.request_key)
            .bind("queued").bind(serde_json::to_string(&turn)?).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(Admission {
            created: true,
            turn,
            session,
        })
    }

    pub async fn turn(&self, principal: Uuid, session: Uuid, id: Uuid) -> StateResult<TurnRecord> {
        self.session(principal, session).await?;
        let row = sqlx::query("SELECT record_json FROM cloud_turns WHERE id=? AND session_id=?")
            .bind(id.to_string())
            .bind(session.to_string())
            .fetch_optional(&self.pool)
            .await?
            .ok_or(StateError::NotFound)?;
        Ok(serde_json::from_str(row.get("record_json"))?)
    }

    pub async fn active_turn(
        &self,
        principal: Uuid,
        session: Uuid,
    ) -> StateResult<Option<TurnRecord>> {
        self.session(principal, session).await?;
        let row = sqlx::query("SELECT record_json FROM cloud_turns WHERE session_id=? AND state IN ('queued','running','recovery_required') ORDER BY rowid DESC LIMIT 1")
            .bind(session.to_string()).fetch_optional(&self.pool).await?;
        row.map(|row| serde_json::from_str(row.get("record_json")).map_err(StateError::from))
            .transpose()
    }

    /// Permission changes affect the next admitted turn, never an active one.
    pub async fn set_permissions(
        &self,
        principal: Uuid,
        session: Uuid,
        mode: PermissionMode,
    ) -> StateResult<()> {
        let mut tx = self.pool.begin().await?;
        let row = sqlx::query("SELECT principal,state_json FROM cloud_sessions WHERE id=?")
            .bind(session.to_string())
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(StateError::NotFound)?;
        if row.get::<String, _>("principal") != principal.to_string() {
            return Err(StateError::Forbidden);
        }
        let mut state: SessionState = serde_json::from_str(row.get("state_json"))?;
        let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM cloud_turns WHERE session_id=? AND state IN ('queued','running','recovery_required')")
            .bind(session.to_string()).fetch_one(&mut *tx).await?;
        if count != 0 || !state.execution_settled() {
            return Err(StateError::Busy);
        }
        state.permission_mode = mode as u8;
        sqlx::query("UPDATE cloud_sessions SET state_json=? WHERE id=?")
            .bind(serde_json::to_string(&state)?)
            .bind(session.to_string())
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn start(&self, id: Uuid) -> StateResult<()> {
        self.transition(id, TurnState::Queued, TurnState::Running, false)
            .await
    }

    pub async fn mark_uncertain(&self, id: Uuid) -> StateResult<()> {
        let mut tx = self.pool.begin().await?;
        let row = sqlx::query("SELECT record_json FROM cloud_turns WHERE id=?")
            .bind(id.to_string())
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(StateError::NotFound)?;
        let mut turn: TurnRecord = serde_json::from_str(row.get("record_json"))?;
        if turn.state != TurnState::Running {
            return Err(StateError::TransitionConflict);
        }
        turn.state = TurnState::RecoveryRequired;
        update_turn(&mut tx, &turn).await?;
        let row = sqlx::query("SELECT state_json FROM cloud_sessions WHERE id=?")
            .bind(turn.session_id.to_string())
            .fetch_one(&mut *tx)
            .await?;
        let mut session: SessionState = serde_json::from_str(row.get("state_json"))?;
        session.unsettled_jobs = None;
        sqlx::query("UPDATE cloud_sessions SET state_json=? WHERE id=?")
            .bind(serde_json::to_string(&session)?)
            .bind(turn.session_id.to_string())
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn request_cancel(
        &self,
        principal: Uuid,
        session: Uuid,
        id: Uuid,
    ) -> StateResult<TurnRecord> {
        let turn = self.turn(principal, session, id).await?;
        self.transition(id, turn.state, turn.state, true).await?;
        self.turn(principal, session, id).await
    }

    async fn transition(
        &self,
        id: Uuid,
        before: TurnState,
        after: TurnState,
        cancel: bool,
    ) -> StateResult<()> {
        let mut tx = self.pool.begin().await?;
        let row = sqlx::query("SELECT record_json FROM cloud_turns WHERE id=?")
            .bind(id.to_string())
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(StateError::NotFound)?;
        let mut turn: TurnRecord = serde_json::from_str(row.get("record_json"))?;
        if turn.state != before {
            return Err(StateError::TransitionConflict);
        }
        turn.state = after;
        turn.cancel_requested |= cancel;
        update_turn(&mut tx, &turn).await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn settle(&self, id: Uuid, result: &CloudTurnResult) -> StateResult<TurnRecord> {
        let mut tx = self.pool.begin().await?;
        let row = sqlx::query("SELECT record_json FROM cloud_turns WHERE id=?")
            .bind(id.to_string())
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(StateError::NotFound)?;
        let mut turn: TurnRecord = serde_json::from_str(row.get("record_json"))?;
        if turn.state != TurnState::Running {
            return Err(StateError::TransitionConflict);
        }
        let row = sqlx::query("SELECT state_json FROM cloud_sessions WHERE id=?")
            .bind(turn.session_id.to_string())
            .fetch_one(&mut *tx)
            .await?;
        let mut session: SessionState = serde_json::from_str(row.get("state_json"))?;
        if session.revision != turn.base_revision {
            return Err(StateError::TransitionConflict);
        }
        session.history = result.history.clone();
        session.history_flags = result.history_flags.clone();
        session.unsettled_jobs = result.unsettled_jobs.clone();
        session.revision += 1;
        turn.state = result.status.into();
        turn.replies = result.replies.clone();
        update_turn(&mut tx, &turn).await?;
        sqlx::query("UPDATE cloud_sessions SET state_json=? WHERE id=?")
            .bind(serde_json::to_string(&session)?)
            .bind(turn.session_id.to_string())
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(turn)
    }

    /// Confirm a previously uncertain device state from an authenticated query.
    /// A restart's lost canonical turn stays RecoveryRequired until explicitly
    /// acknowledged; querying empty jobs alone must not rewrite that turn.
    pub async fn reconcile_jobs(
        &self,
        principal: Uuid,
        id: Uuid,
        expected_revision: u64,
        jobs: Vec<JobSnapshot>,
    ) -> StateResult<()> {
        let mut tx = self.pool.begin().await?;
        let row = sqlx::query("SELECT principal,state_json FROM cloud_sessions WHERE id=?")
            .bind(id.to_string())
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(StateError::NotFound)?;
        if row.get::<String, _>("principal") != principal.to_string() {
            return Err(StateError::Forbidden);
        }
        let mut session: SessionState = serde_json::from_str(row.get("state_json"))?;
        let (active,): (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM cloud_turns WHERE session_id=? AND state IN ('queued','running')",
        )
        .bind(id.to_string())
        .fetch_one(&mut *tx)
        .await?;
        if session.revision != expected_revision || active != 0 {
            return Err(StateError::TransitionConflict);
        }
        if jobs
            .iter()
            .any(|job| job.session_id != session.frozen.binding.session_id)
        {
            return Err(StateError::BindingConflict);
        }
        session.unsettled_jobs = Some(
            jobs.into_iter()
                .filter(|job| {
                    !job.status.is_terminal() || job.status == JobStatus::RecoveryRequired
                })
                .collect(),
        );
        sqlx::query("UPDATE cloud_sessions SET state_json=? WHERE id=?")
            .bind(serde_json::to_string(&session)?)
            .bind(id.to_string())
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Deployment restart never replays queued/running tools. It distinguishes
    /// a not-started prompt from a turn whose side effects may have happened.
    async fn recover(&self) -> StateResult<()> {
        let mut tx = self.pool.begin().await?;
        let rows =
            sqlx::query("SELECT record_json FROM cloud_turns WHERE state IN ('queued','running')")
                .fetch_all(&mut *tx)
                .await?;
        for row in rows {
            let mut turn: TurnRecord = serde_json::from_str(row.get("record_json"))?;
            if turn.state == TurnState::Running {
                turn.state = TurnState::RecoveryRequired;
                let row = sqlx::query("SELECT state_json FROM cloud_sessions WHERE id=?")
                    .bind(turn.session_id.to_string())
                    .fetch_one(&mut *tx)
                    .await?;
                let mut session: SessionState = serde_json::from_str(row.get("state_json"))?;
                session.unsettled_jobs = None;
                sqlx::query("UPDATE cloud_sessions SET state_json=? WHERE id=?")
                    .bind(serde_json::to_string(&session)?)
                    .bind(turn.session_id.to_string())
                    .execute(&mut *tx)
                    .await?;
            } else {
                turn.state = TurnState::Interrupted;
            }
            update_turn(&mut tx, &turn).await?;
        }
        tx.commit().await?;
        Ok(())
    }
}

fn open_owner(root: &Path) -> StateResult<File> {
    std::fs::create_dir_all(root)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(root, std::fs::Permissions::from_mode(0o700))?;
    }
    let owner = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(root.join("cloud-owner.lock"))?;
    owner
        .try_lock_exclusive()
        .map_err(|_| StateError::AlreadyOwned)?;
    Ok(owner)
}

async fn update_turn(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    turn: &TurnRecord,
) -> StateResult<()> {
    let key = match turn.state {
        TurnState::Queued => "queued",
        TurnState::Running => "running",
        TurnState::Completed => "completed",
        TurnState::Interrupted => "interrupted",
        TurnState::Failed => "failed",
        TurnState::RecoveryRequired => "recovery_required",
    };
    sqlx::query("UPDATE cloud_turns SET state=?,record_json=? WHERE id=?")
        .bind(key)
        .bind(serde_json::to_string(turn)?)
        .bind(turn.turn_id.to_string())
        .execute(&mut **tx)
        .await?;
    Ok(())
}
