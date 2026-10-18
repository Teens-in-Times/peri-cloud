use crate::*;
use async_trait::async_trait;
use sqlx::{Row, Sqlite, SqlitePool, Transaction};
use uuid::Uuid;

/// Atomic storage port. Implementations must preserve each operation's
/// transaction boundary; none of these calls performs external side effects.
#[async_trait]
pub trait SchedulerStore: Send + Sync {
    async fn apply(&self, owner: OwnerScope, command: CommandEnvelope, now: i64) -> Result<Plan>;
    async fn inspect(&self, owner: OwnerScope, query: ScheduleQuery) -> Result<QueryPage>;
    async fn claim_due(&self, now: i64) -> Result<()>;
    async fn pending(&self, now: i64) -> Result<Vec<Occurrence>>;
    async fn begin_dispatch(&self, event: &Occurrence, now: i64) -> Result<bool>;
    async fn finish(&self, event: &Occurrence, outcome: SubmitOutcome, now: i64) -> Result<()>;
    async fn unresolved(&self) -> Result<u64>;
}
#[async_trait]
impl SchedulerStore for SqliteStore {
    async fn apply(&self, owner: OwnerScope, command: CommandEnvelope, now: i64) -> Result<Plan> {
        SqliteStore::apply(self, owner, command, now).await
    }
    async fn inspect(&self, owner: OwnerScope, query: ScheduleQuery) -> Result<QueryPage> {
        Ok(match query {
            ScheduleQuery::Plans { offset, limit } => {
                QueryPage::Plans(self.plans(owner, offset, limit).await?)
            }
            ScheduleQuery::Plan { id } => QueryPage::Plan(self.get(owner, id).await?),
            ScheduleQuery::Occurrences { id, offset, limit } => {
                QueryPage::Occurrences(self.occurrences(owner, id, offset, limit).await?)
            }
        })
    }
    async fn claim_due(&self, now: i64) -> Result<()> {
        SqliteStore::claim_due(self, now).await
    }
    async fn pending(&self, now: i64) -> Result<Vec<Occurrence>> {
        SqliteStore::pending(self, now).await
    }
    async fn begin_dispatch(&self, event: &Occurrence, now: i64) -> Result<bool> {
        SqliteStore::begin_dispatch(self, event, now).await
    }
    async fn finish(&self, event: &Occurrence, outcome: SubmitOutcome, now: i64) -> Result<()> {
        SqliteStore::finish(self, event, outcome, now).await
    }
    async fn unresolved(&self) -> Result<u64> {
        let (count,): (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM scheduler_occurrences WHERE state IN ('unknown','dispatching')",
        )
        .fetch_one(&self.pool)
        .await?;
        Ok(count as u64)
    }
}

/// Uses the host's already-owned pool. Opening a second process on the cloud
/// state directory is deliberately not part of this interface.
#[derive(Clone)]
pub struct SqliteStore {
    pool: SqlitePool,
}
impl SqliteStore {
    pub fn from_pool(pool: SqlitePool) -> Self {
        Self { pool }
    }

    pub async fn migrate(&self) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("CREATE TABLE IF NOT EXISTS scheduler_metadata (singleton INTEGER PRIMARY KEY CHECK(singleton=1),version INTEGER NOT NULL)").execute(&mut *tx).await?;
        let version: Option<(i64,)> =
            sqlx::query_as("SELECT version FROM scheduler_metadata WHERE singleton=1")
                .fetch_optional(&mut *tx)
                .await?;
        if version.is_some_and(|(v,)| v != 1) {
            return Err(Error::Version);
        }
        for sql in [
            "CREATE TABLE IF NOT EXISTS scheduler_plans (id TEXT PRIMARY KEY,owner TEXT NOT NULL,record_json TEXT NOT NULL,enabled INTEGER NOT NULL,deleted INTEGER NOT NULL,next_at INTEGER)",
            "CREATE INDEX IF NOT EXISTS scheduler_due ON scheduler_plans(enabled,deleted,next_at)",
            "CREATE INDEX IF NOT EXISTS scheduler_owner ON scheduler_plans(owner)",
            "CREATE TABLE IF NOT EXISTS scheduler_occurrences (id TEXT PRIMARY KEY,schedule_id TEXT NOT NULL REFERENCES scheduler_plans(id),owner TEXT NOT NULL,planned_at INTEGER NOT NULL,state TEXT NOT NULL,next_attempt INTEGER NOT NULL,record_json TEXT NOT NULL,UNIQUE(schedule_id,planned_at))",
            "CREATE INDEX IF NOT EXISTS scheduler_pending ON scheduler_occurrences(state,next_attempt)",
            "CREATE TABLE IF NOT EXISTS scheduler_commands (owner TEXT NOT NULL,command_id TEXT NOT NULL,request_json TEXT NOT NULL,receipt_json TEXT NOT NULL,PRIMARY KEY(owner,command_id))",
            "INSERT OR IGNORE INTO scheduler_metadata(singleton,version) VALUES(1,1)",
        ] {sqlx::query(sql).execute(&mut *tx).await?;}
        tx.commit().await?;
        Ok(())
    }
    /// Call exactly once when acquiring deployment ownership, before dispatch.
    pub async fn recover_owned(&self) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        let rows =
            sqlx::query("SELECT record_json FROM scheduler_occurrences WHERE state='dispatching'")
                .fetch_all(&mut *tx)
                .await?;
        for row in rows {
            let mut event: Occurrence = serde_json::from_str(row.get("record_json"))?;
            event.state = DispatchState::Unknown;
            event.next_attempt = 0;
            event.reason = Some("owner_restarted_during_dispatch".into());
            save_occurrence(&mut tx, &event).await?;
        }
        tx.commit().await?;
        Ok(())
    }
    pub async fn apply(
        &self,
        owner: OwnerScope,
        envelope: CommandEnvelope,
        now: i64,
    ) -> Result<Plan> {
        let mut tx = self.pool.begin().await?;
        let plan = Self::apply_in(&mut tx, owner, envelope, now).await?;
        tx.commit().await?;
        Ok(plan)
    }
    /// Lets a single-owner host atomically create its opaque task reference and
    /// the corresponding plan without a distributed transaction.
    pub async fn apply_in(
        tx: &mut Transaction<'_, Sqlite>,
        owner: OwnerScope,
        envelope: CommandEnvelope,
        now: i64,
    ) -> Result<Plan> {
        let request = serde_json::to_string(&envelope.command)?;
        if let Some(row)=sqlx::query("SELECT request_json,receipt_json FROM scheduler_commands WHERE owner=? AND command_id=?")
            .bind(owner.0.to_string()).bind(envelope.command_id.to_string()).fetch_optional(&mut **tx).await? {
            if row.get::<String,_>("request_json")!=request {return Err(Error::Conflict);}
            return Ok(serde_json::from_str(row.get("receipt_json"))?);
        }
        let record = match envelope.command {
            ScheduleCommand::Create { mut spec } => {
                spec.policy.validate()?;
                if spec.task.revision == 0 {
                    return Err(Error::Invalid("task version must be positive"));
                }
                spec.timing = spec.timing.resolve(now)?;
                let next = spec
                    .timing
                    .next_after(now)?
                    .ok_or(Error::Invalid("no future occurrence"))?;
                let (exists,): (i64,) =
                    sqlx::query_as("SELECT COUNT(*) FROM scheduler_plans WHERE id=?")
                        .bind(spec.id.to_string())
                        .fetch_one(&mut **tx)
                        .await?;
                if exists != 0 {
                    return Err(Error::Conflict);
                }
                let (owned, total): (i64, i64) = sqlx::query_as(
                    "SELECT COALESCE(SUM(owner=?),0),COUNT(*) FROM scheduler_plans WHERE deleted=0",
                )
                .bind(owner.0.to_string())
                .fetch_one(&mut **tx)
                .await?;
                if owned >= MAX_SCHEDULES_PER_OWNER || total >= MAX_SCHEDULES {
                    return Err(Error::Limit);
                }
                Plan {
                    owner,
                    spec,
                    revision: 1,
                    enabled: true,
                    paused: false,
                    deleted: false,
                    next_at: Some(next),
                }
            }
            ScheduleCommand::Update {
                id,
                expected_revision,
                timing,
                task,
                enabled,
            } => {
                let mut plan = read_plan(tx, owner, id).await?;
                check_revision(&plan, expected_revision)?;
                if let Some(task) = task {
                    if task.revision == 0 {
                        return Err(Error::Invalid("invalid task version"));
                    }
                    plan.spec.task = task;
                }
                if let Some(timing) = timing {
                    plan.spec.timing = timing.resolve(now)?;
                    plan.next_at = plan.spec.timing.next_after(now)?;
                }
                if let Some(enabled) = enabled {
                    if enabled && !plan.enabled {
                        plan.next_at = plan.spec.timing.next_after(now)?;
                    }
                    plan.enabled = enabled;
                    plan.paused = !enabled;
                }
                if plan.enabled && plan.next_at.is_none() {
                    return Err(Error::Invalid("no future occurrence"));
                }
                plan.revision += 1;
                plan
            }
            ScheduleCommand::Remove {
                id,
                expected_revision,
            } => {
                let mut plan = read_plan(tx, owner, id).await?;
                check_revision(&plan, expected_revision)?;
                plan.deleted = true;
                plan.enabled = false;
                plan.paused = true;
                plan.revision += 1;
                plan
            }
        };
        save_plan(tx, &record).await?;
        sqlx::query("INSERT INTO scheduler_commands(owner,command_id,request_json,receipt_json) VALUES(?,?,?,?)")
            .bind(owner.0.to_string()).bind(envelope.command_id.to_string()).bind(request).bind(serde_json::to_string(&record)?)
            .execute(&mut **tx).await?;
        Ok(record)
    }
    pub async fn get(&self, owner: OwnerScope, id: Uuid) -> Result<Plan> {
        let mut tx = self.pool.begin().await?;
        read_plan(&mut tx, owner, id).await
    }
    pub async fn plans(&self, owner: OwnerScope, offset: u32, limit: u16) -> Result<Vec<Plan>> {
        let rows=sqlx::query("SELECT record_json FROM scheduler_plans WHERE owner=? AND deleted=0 ORDER BY rowid DESC LIMIT ? OFFSET ?")
            .bind(owner.0.to_string()).bind(i64::from(limit.min(100))).bind(i64::from(offset)).fetch_all(&self.pool).await?;
        rows.into_iter()
            .map(|r| Ok(serde_json::from_str(r.get("record_json"))?))
            .collect()
    }
    pub async fn occurrences(
        &self,
        owner: OwnerScope,
        id: Uuid,
        offset: u32,
        limit: u16,
    ) -> Result<Vec<Occurrence>> {
        self.get(owner, id).await?;
        let rows=sqlx::query("SELECT record_json FROM scheduler_occurrences WHERE owner=? AND schedule_id=? ORDER BY planned_at DESC LIMIT ? OFFSET ?")
            .bind(owner.0.to_string()).bind(id.to_string()).bind(i64::from(limit.min(100))).bind(i64::from(offset)).fetch_all(&self.pool).await?;
        rows.into_iter()
            .map(|r| Ok(serde_json::from_str(r.get("record_json"))?))
            .collect()
    }
    pub async fn occurrence(&self, owner: OwnerScope, id: Uuid) -> Result<Occurrence> {
        let row =
            sqlx::query("SELECT record_json FROM scheduler_occurrences WHERE owner=? AND id=?")
                .bind(owner.0.to_string())
                .bind(id.to_string())
                .fetch_optional(&self.pool)
                .await?
                .ok_or(Error::Forbidden)?;
        Ok(serde_json::from_str(row.get("record_json"))?)
    }
    pub(crate) async fn claim_due(&self, now: i64) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        // Round-robin owners so a large backlog cannot monopolise the batch.
        let rows=sqlx::query("SELECT record_json FROM (SELECT record_json,next_at,id,ROW_NUMBER() OVER(PARTITION BY owner ORDER BY next_at,id) AS rank FROM scheduler_plans WHERE enabled=1 AND deleted=0 AND next_at<=?) ORDER BY rank,next_at,id LIMIT 32")
            .bind(now).fetch_all(&mut *tx).await?;
        let mut remaining = MAX_BATCH;
        for row in rows {
            if remaining == 0 {
                break;
            }
            let mut plan: Plan = serde_json::from_str(row.get("record_json"))?;
            let first = plan
                .next_at
                .ok_or(Error::Invalid("missing next occurrence"))?;
            let mut dates = vec![first];
            if !matches!(plan.spec.policy.misfire, Misfire::Skip) {
                let limit = match plan.spec.policy.misfire {
                    Misfire::CatchUp { limit } => usize::from(limit).min(remaining),
                    _ => 1,
                };
                dates.clear();
                let mut cursor = now;
                for _ in 0..limit {
                    let Some(at) = plan.spec.timing.at_or_before(cursor)? else {
                        break;
                    };
                    if at < first {
                        break;
                    }
                    dates.push(at);
                    let Some(before) = at.checked_sub(1) else {
                        break;
                    };
                    cursor = before;
                }
                dates.reverse();
            }
            for at in dates {
                let deadline = at.saturating_add(plan.spec.policy.start_grace_seconds);
                let expired = now > deadline;
                let event = Occurrence {
                    id: Occurrence::stable_id(plan.spec.id, at),
                    owner: plan.owner,
                    schedule_id: plan.spec.id,
                    planned_at: at,
                    task: plan.spec.task.clone(),
                    start_deadline: deadline,
                    state: if expired {
                        DispatchState::Skipped
                    } else {
                        DispatchState::Pending
                    },
                    next_attempt: now,
                    reason: expired.then(|| "misfire_expired".into()),
                    run_ref: None,
                };
                // Never replace an existing event, including its immutable task snapshot.
                sqlx::query("INSERT OR IGNORE INTO scheduler_occurrences(id,schedule_id,owner,planned_at,state,next_attempt,record_json) VALUES(?,?,?,?,?,?,?)")
                    .bind(event.id.to_string()).bind(event.schedule_id.to_string()).bind(event.owner.0.to_string()).bind(at)
                    .bind(state_name(&event.state)).bind(now).bind(serde_json::to_string(&event)?).execute(&mut *tx).await?;
                remaining -= 1;
            }
            plan.next_at = plan.spec.timing.next_after(now)?;
            plan.enabled = plan.next_at.is_some();
            save_plan(&mut tx, &plan).await?;
        }
        tx.commit().await?;
        Ok(())
    }
    pub(crate) async fn pending(&self, now: i64) -> Result<Vec<Occurrence>> {
        let rows=sqlx::query("SELECT record_json FROM (SELECT record_json,planned_at,id,ROW_NUMBER() OVER(PARTITION BY owner ORDER BY next_attempt,planned_at,id) AS rank FROM scheduler_occurrences WHERE state IN ('pending','deferred','unknown') AND next_attempt<=?) ORDER BY rank,planned_at,id LIMIT 32")
            .bind(now).fetch_all(&self.pool).await?;
        rows.into_iter()
            .map(|r| Ok(serde_json::from_str(r.get("record_json"))?))
            .collect()
    }
    pub(crate) async fn begin_dispatch(&self, event: &Occurrence, now: i64) -> Result<bool> {
        let mut tx = self.pool.begin().await?;
        let plan = read_plan(&mut tx, event.owner, event.schedule_id).await?;
        let mut event = event.clone();
        // Natural exhaustion of a one-shot is different from explicit pause.
        let paused = plan.deleted || plan.paused;
        if paused || now > event.start_deadline {
            event.state = DispatchState::Skipped;
            event.reason = Some(
                if paused {
                    "plan_disabled"
                } else {
                    "start_deadline_expired"
                }
                .into(),
            );
            save_occurrence(&mut tx, &event).await?;
            tx.commit().await?;
            return Ok(false);
        }
        let (unresolved,):(i64,)=sqlx::query_as("SELECT COUNT(*) FROM scheduler_occurrences WHERE schedule_id=? AND id<>? AND state IN ('unknown','dispatching')")
            .bind(event.schedule_id.to_string()).bind(event.id.to_string()).fetch_one(&mut *tx).await?;
        if unresolved != 0 {
            event.state = DispatchState::Deferred;
            event.next_attempt = now.saturating_add(30);
            event.reason = Some("previous_admission_unknown".into());
            save_occurrence(&mut tx, &event).await?;
            tx.commit().await?;
            return Ok(false);
        }
        let changed=sqlx::query("UPDATE scheduler_occurrences SET state='dispatching' WHERE id=? AND state IN ('pending','deferred')")
            .bind(event.id.to_string()).execute(&mut *tx).await?.rows_affected();
        if changed != 0 {
            event.state = DispatchState::Dispatching;
            save_occurrence(&mut tx, &event).await?;
        }
        tx.commit().await?;
        Ok(changed != 0)
    }
    pub(crate) async fn finish(
        &self,
        event: &Occurrence,
        outcome: SubmitOutcome,
        now: i64,
    ) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        let mut event = event.clone();
        match outcome {
            SubmitOutcome::Accepted { run_ref } => {
                event.state = DispatchState::Accepted;
                event.run_ref = Some(run_ref);
                event.reason = None;
            }
            SubmitOutcome::Deferred { until, reason } => {
                event.state = DispatchState::Deferred;
                event.next_attempt = until.max(now.saturating_add(1));
                event.reason = Some(reason);
            }
            SubmitOutcome::Rejected { reason } => {
                event.state = DispatchState::Rejected;
                event.reason = Some(reason);
            }
            SubmitOutcome::Unknown => {
                event.state = DispatchState::Unknown;
                event.next_attempt = now.saturating_add(30);
                event.reason = Some("admission_unknown".into());
            }
        }
        save_occurrence(&mut tx, &event).await?;
        tx.commit().await?;
        Ok(())
    }
    /// Trusted, one-time migration with the old owner stopped. The caller can
    /// atomically import cloud task references and scheduler records together.
    pub async fn import(
        tx: &mut Transaction<'_, Sqlite>,
        plan: &Plan,
        events: &[Occurrence],
    ) -> Result<()> {
        save_plan(tx, plan).await?;
        for event in events {
            save_occurrence(tx, event).await?;
        }
        Ok(())
    }
}
fn check_revision(plan: &Plan, expected: i64) -> Result<()> {
    if plan.revision != expected || plan.deleted {
        Err(Error::Conflict)
    } else {
        Ok(())
    }
}
async fn read_plan(tx: &mut Transaction<'_, Sqlite>, owner: OwnerScope, id: Uuid) -> Result<Plan> {
    let row = sqlx::query("SELECT record_json FROM scheduler_plans WHERE id=? AND owner=?")
        .bind(id.to_string())
        .bind(owner.0.to_string())
        .fetch_optional(&mut **tx)
        .await?
        .ok_or(Error::Forbidden)?;
    Ok(serde_json::from_str(row.get("record_json"))?)
}
async fn save_plan(tx: &mut Transaction<'_, Sqlite>, p: &Plan) -> Result<()> {
    sqlx::query("INSERT INTO scheduler_plans(id,owner,record_json,enabled,deleted,next_at) VALUES(?,?,?,?,?,?) ON CONFLICT(id) DO UPDATE SET record_json=excluded.record_json,enabled=excluded.enabled,deleted=excluded.deleted,next_at=excluded.next_at")
        .bind(p.spec.id.to_string()).bind(p.owner.0.to_string()).bind(serde_json::to_string(p)?).bind(p.enabled).bind(p.deleted).bind(p.next_at).execute(&mut **tx).await?;
    Ok(())
}
async fn save_occurrence(tx: &mut Transaction<'_, Sqlite>, e: &Occurrence) -> Result<()> {
    sqlx::query("INSERT INTO scheduler_occurrences(id,schedule_id,owner,planned_at,state,next_attempt,record_json) VALUES(?,?,?,?,?,?,?) ON CONFLICT(id) DO UPDATE SET state=excluded.state,next_attempt=excluded.next_attempt,record_json=excluded.record_json")
        .bind(e.id.to_string()).bind(e.schedule_id.to_string()).bind(e.owner.0.to_string()).bind(e.planned_at).bind(state_name(&e.state)).bind(e.next_attempt).bind(serde_json::to_string(e)?).execute(&mut **tx).await?;
    Ok(())
}
fn state_name(state: &DispatchState) -> &'static str {
    match state {
        DispatchState::Pending => "pending",
        DispatchState::Dispatching => "dispatching",
        DispatchState::Deferred => "deferred",
        DispatchState::Accepted => "accepted",
        DispatchState::Rejected => "rejected",
        DispatchState::Skipped => "skipped",
        DispatchState::Unknown => "unknown",
    }
}
