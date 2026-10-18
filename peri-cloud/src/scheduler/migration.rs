use super::*;
use peri_scheduler::{
    DispatchState, Occurrence, OwnerScope, Plan, Policy, ScheduleSpec, SqliteStore, TaskRef,
};
use sqlx::{Row, SqlitePool};

pub(super) async fn migrate(pool: &SqlitePool) -> crate::state::StateResult<()> {
    let (version,): (i64,) = sqlx::query_as("PRAGMA user_version")
        .fetch_one(pool)
        .await?;
    if version >= 11 {
        SqliteStore::from_pool(pool.clone()).migrate().await?;
        return Ok(());
    }
    if !matches!(version, 9 | 10) {
        return Err(crate::state::StateError::Version);
    }
    SqliteStore::from_pool(pool.clone()).migrate().await?;
    let mut tx = pool.begin().await?;
    for sql in [
        "CREATE TABLE scheduled_tasks (id TEXT NOT NULL,revision INTEGER NOT NULL,owner TEXT NOT NULL REFERENCES principals(id),route_json TEXT NOT NULL,request_json TEXT NOT NULL,record_json TEXT NOT NULL,PRIMARY KEY(id,revision))",
        "CREATE TABLE scheduled_admissions (owner TEXT NOT NULL,occurrence_id TEXT NOT NULL,request_hash TEXT NOT NULL,session_id TEXT NOT NULL REFERENCES cloud_sessions(id),turn_id TEXT NOT NULL UNIQUE REFERENCES cloud_turns(id),context_generation INTEGER NOT NULL,route_json TEXT NOT NULL,request_key TEXT NOT NULL,admitted_at INTEGER NOT NULL,expires_at INTEGER NOT NULL,execution_state TEXT NOT NULL DEFAULT 'submitted',delivery_state TEXT NOT NULL DEFAULT 'pending',PRIMARY KEY(owner,occurrence_id))",
        "CREATE INDEX scheduled_outputs ON scheduled_admissions(execution_state,delivery_state)",
        "CREATE TABLE scheduled_contact_policy (owner TEXT PRIMARY KEY REFERENCES principals(id),record_json TEXT NOT NULL)",
    ] {sqlx::query(sql).execute(&mut *tx).await?;}
    if version == 10 {
        let rows = sqlx::query("SELECT * FROM cloud_schedules")
            .fetch_all(&mut *tx)
            .await?;
        for row in rows {
            let owner = store::parse_uuid(row.get("principal"))?;
            let schedule: Schedule = serde_json::from_str(row.get("record_json"))?;
            let route: crate::gateway::ChannelRoute = serde_json::from_str(row.get("route_json"))?;
            let task = TaskRef {
                id: schedule.id,
                revision: 1,
            };
            // The default TargetBinding is LegacyFrozenSession. Never silently
            // turn an old device/session grant into a route-following grant.
            let plan = Plan {
                owner: OwnerScope(owner),
                spec: ScheduleSpec {
                    id: schedule.id,
                    task: task.clone(),
                    timing: schedule.timing.clone(),
                    policy: Policy::default(),
                },
                revision: schedule.revision,
                enabled: schedule.enabled,
                paused: !schedule.enabled && schedule.next_fire.is_some(),
                deleted: schedule.deleted,
                next_at: schedule.next_fire,
            };
            sqlx::query("INSERT INTO scheduled_tasks(id,revision,owner,route_json,request_json,record_json) VALUES(?,1,?,?,?,?)")
                .bind(schedule.id.to_string()).bind(owner.to_string()).bind(row.get::<String,_>("route_json")).bind(row.get::<String,_>("request_json")).bind(serde_json::to_string(&schedule)?).execute(&mut *tx).await?;
            let rows = sqlx::query("SELECT * FROM cloud_schedule_runs WHERE schedule_id=?")
                .bind(schedule.id.to_string())
                .fetch_all(&mut *tx)
                .await?;
            let mut events = Vec::new();
            for row in rows {
                let at: i64 = row.get("scheduled_at");
                let mut turn: Option<String> = row.get("turn_id");
                let state: String = row.get("state");
                if turn.is_none() {
                    turn = sqlx::query_scalar(
                        "SELECT id FROM cloud_turns WHERE session_id=? AND request_key=?",
                    )
                    .bind(schedule.session_id.to_string())
                    .bind(row.get::<String, _>("request_key"))
                    .fetch_optional(&mut *tx)
                    .await?;
                }
                let event = Occurrence {
                    id: Occurrence::stable_id(schedule.id, at),
                    owner: OwnerScope(owner),
                    schedule_id: schedule.id,
                    planned_at: at,
                    task: task.clone(),
                    start_deadline: at.saturating_add(MISFIRE_GRACE_SECONDS),
                    state: if turn.is_some() {
                        DispatchState::Accepted
                    } else if state == "skipped" {
                        DispatchState::Skipped
                    } else {
                        DispatchState::Unknown
                    },
                    next_attempt: 0,
                    reason: if turn.is_none() && state != "skipped" {
                        Some("legacy_admission_unknown".into())
                    } else {
                        row.get("reason")
                    },
                    run_ref: turn.clone(),
                };
                if let Some(turn) = turn {
                    let (owned,):(i64,)=sqlx::query_as("SELECT COUNT(*) FROM cloud_turns t JOIN cloud_sessions s ON s.id=t.session_id WHERE t.id=? AND t.session_id=? AND t.request_key=? AND s.principal=?")
                        .bind(&turn).bind(schedule.session_id.to_string()).bind(row.get::<String,_>("request_key")).bind(owner.to_string()).fetch_one(&mut *tx).await?;
                    if owned != 1 {
                        return Err(crate::state::StateError::InvocationConflict);
                    }
                    let firing = Firing {
                        principal: owner,
                        route: route.clone(),
                        schedule: schedule.clone(),
                        event: event.clone(),
                        request_key: event.request_key(),
                    };
                    let old_delivery: String = row.get("delivery_state");
                    let delivery = if matches!(old_delivery.as_str(), "queued" | "pending") {
                        "suppressed"
                    } else {
                        old_delivery.as_str()
                    };
                    sqlx::query("INSERT INTO scheduled_admissions(owner,occurrence_id,request_hash,session_id,turn_id,context_generation,route_json,request_key,admitted_at,expires_at,execution_state,delivery_state) VALUES(?,?,?,?,?,0,?,?,?,?,?,?)")
                        .bind(owner.to_string()).bind(event.id.to_string()).bind(firing.fingerprint()?).bind(schedule.session_id.to_string()).bind(turn).bind(serde_json::to_string(&route)?).bind(event.request_key()).bind(at).bind(event.start_deadline).bind(if matches!(state.as_str(),"claimed"|"submitted"|"recovery_required") {"submitted"}else {state.as_str()}).bind(delivery).execute(&mut *tx).await?;
                }
                events.push(event);
            }
            SqliteStore::import(&mut tx, &plan, &events).await?;
        }
    }
    sqlx::query("PRAGMA user_version=11")
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}
