use super::*;
use crate::gateway::ChannelRoute;
use peri_scheduler::{
    CommandEnvelope, DispatchState, Occurrence, OwnerScope, Plan, Policy, ScheduleCommand,
    ScheduleSpec, SqliteStore, TaskRef,
};
use sqlx::{Row, SqlitePool};
use uuid::Uuid;
#[derive(Clone)]
pub(crate) struct ScheduleStore {
    pub pool: SqlitePool,
    pub core: SqliteStore,
}
#[derive(Clone)]
pub(crate) struct Firing {
    pub principal: Uuid,
    pub route: ChannelRoute,
    pub schedule: Schedule,
    pub event: Occurrence,
    pub request_key: String,
}
impl Firing {
    pub fn fingerprint(&self) -> ScheduleResult<String> {
        use sha2::{Digest, Sha256};
        Ok((Sha256::digest(serde_json::to_vec(&(
            self.principal,
            self.event.id,
            &self.event.task,
            self.event.planned_at,
            &self.route,
            self.schedule.mode,
            self.schedule.notify,
            &self.schedule.prompt,
            self.schedule.binding,
            self.schedule.device_id,
            &self.schedule.workspace,
        ))?))
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>())
    }
}
impl ScheduleStore {
    pub fn new(journal: &crate::state::CloudJournal) -> Self {
        let pool = journal.identity_pool();
        Self {
            core: SqliteStore::from_pool(pool.clone()),
            pool,
        }
    }
    pub async fn get(&self, principal: Uuid, id: Uuid) -> ScheduleResult<Schedule> {
        let plan = self.core.get(OwnerScope(principal), id).await?;
        let mut schedule = self.task(principal, &plan.spec.task).await?.1;
        project(&mut schedule, &plan);
        Ok(schedule)
    }
    pub async fn task(
        &self,
        principal: Uuid,
        task: &TaskRef,
    ) -> ScheduleResult<(ChannelRoute, Schedule)> {
        let row=sqlx::query("SELECT route_json,record_json FROM scheduled_tasks WHERE owner=? AND id=? AND revision=?")
            .bind(principal.to_string()).bind(task.id.to_string()).bind(task.revision as i64).fetch_optional(&self.pool).await?.ok_or(ScheduleError::Forbidden)?;
        Ok((
            serde_json::from_str(row.get("route_json"))?,
            serde_json::from_str(row.get("record_json"))?,
        ))
    }
    pub async fn list(&self, principal: Uuid) -> ScheduleResult<Vec<Schedule>> {
        let mut result = Vec::new();
        for plan in self.core.plans(OwnerScope(principal), 0, 100).await? {
            let mut schedule = self.task(principal, &plan.spec.task).await?.1;
            project(&mut schedule, &plan);
            result.push(schedule);
        }
        Ok(result)
    }
    pub async fn create(
        &self,
        principal: Uuid,
        route: &ChannelRoute,
        input: NewSchedule,
        now: i64,
        push: bool,
    ) -> ScheduleResult<Schedule> {
        let request = serde_json::to_string(&input)?;
        let mut tx = self.pool.begin().await?;
        if let Some(row) = sqlx::query(
            "SELECT owner,route_json,request_json FROM scheduled_tasks WHERE id=? AND revision=1",
        )
        .bind(input.request_id.to_string())
        .fetch_optional(&mut *tx)
        .await?
        {
            if row.get::<String, _>("owner") != principal.to_string()
                || row.get::<String, _>("request_json") != request
                || row.get::<String, _>("route_json") != serde_json::to_string(route)?
            {
                return Err(ScheduleError::Conflict);
            }
            tx.commit().await?;
            return self.get(principal, input.request_id).await;
        }
        if input.title.trim().is_empty()
            || input.title.chars().count() > 128
            || input.title.chars().any(char::is_control)
            || input.prompt.trim().is_empty()
            || input.prompt.len() > 8192
            || input.prompt.contains('\0')
            || input.prompt.trim().starts_with('/')
        {
            return Err(ScheduleError::Invalid("invalid title or prompt"));
        }
        let row=sqlx::query("SELECT s.state_json FROM cloud_sessions s JOIN gateway_routes r ON r.session_id=s.id AND r.principal=s.principal WHERE s.id=? AND s.principal=? AND r.route_json=?")
            .bind(input.session_id.to_string()).bind(principal.to_string()).bind(serde_json::to_string(route)?).fetch_optional(&mut *tx).await?.ok_or(ScheduleError::Forbidden)?;
        let session: crate::state::SessionState = serde_json::from_str(row.get("state_json"))?;
        let device_task = input.mode == ScheduleMode::Task;
        if device_task && (session.frozen.device_id.is_none() || session.frozen.binding.is_none()) {
            return Err(ScheduleError::Invalid(
                "device task needs an authorized device session",
            ));
        }
        if device_task {
            let (owned,):(i64,)=sqlx::query_as("SELECT COUNT(*) FROM identity_devices WHERE id=? AND principal=? AND COALESCE(json_extract(record_json,'$.revoked'),0)=0")
                .bind(session.frozen.device_id.map(|id|id.to_string())).bind(principal.to_string()).fetch_one(&mut *tx).await?;
            if owned != 1 {
                return Err(ScheduleError::Forbidden);
            }
        }
        let mut schedule = Schedule {
            mode: input.mode,
            notify: input.notify,
            id: input.request_id,
            session_id: input.session_id,
            device_id: if device_task {
                session.frozen.device_id
            } else {
                None
            },
            workspace: if device_task {
                session.frozen.workspace()
            } else {
                String::new()
            },
            title: input.title,
            prompt: input.prompt,
            timing: input.timing.clone(),
            enabled: true,
            deleted: false,
            revision: 1,
            next_fire: None,
            created_at: now,
            notification: if push { "push" } else { "inbox" }.into(),
            binding: if device_task {
                TargetBinding::Device
            } else {
                TargetBinding::Chat
            },
        };
        let plan = SqliteStore::apply_in(
            &mut tx,
            OwnerScope(principal),
            CommandEnvelope {
                command_id: input.request_id,
                command: ScheduleCommand::Create {
                    spec: ScheduleSpec {
                        id: input.request_id,
                        task: TaskRef {
                            id: input.request_id,
                            revision: 1,
                        },
                        timing: input.timing,
                        policy: Policy::default(),
                    },
                },
            },
            now,
        )
        .await?;
        project(&mut schedule, &plan);
        sqlx::query("INSERT INTO scheduled_tasks(id,revision,owner,route_json,request_json,record_json) VALUES(?,1,?,?,?,?)")
            .bind(schedule.id.to_string()).bind(principal.to_string()).bind(serde_json::to_string(route)?).bind(request).bind(serde_json::to_string(&schedule)?).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(schedule)
    }
    pub async fn change(
        &self,
        principal: Uuid,
        id: Uuid,
        revision: i64,
        action: &str,
        now: i64,
    ) -> ScheduleResult<Schedule> {
        let command = match action {
            "pause" | "resume" => ScheduleCommand::Update {
                id,
                expected_revision: revision,
                timing: None,
                task: None,
                enabled: Some(action == "resume"),
            },
            "delete" => ScheduleCommand::Remove {
                id,
                expected_revision: revision,
            },
            _ => return Err(ScheduleError::Invalid("unknown schedule action")),
        };
        self.core
            .apply(
                OwnerScope(principal),
                CommandEnvelope {
                    command_id: tools::stable_id(id, &format!("{revision}:{action}")),
                    command,
                },
                now,
            )
            .await?;
        self.get(principal, id).await
    }
    pub async fn edit(
        &self,
        principal: Uuid,
        id: Uuid,
        input: ScheduleEdit,
        now: i64,
    ) -> ScheduleResult<Schedule> {
        let plan = self.core.get(OwnerScope(principal), id).await?;
        let (route, mut schedule) = self.task(principal, &plan.spec.task).await?;
        if input.title.trim().is_empty()
            || input.title.chars().count() > 128
            || input.title.chars().any(char::is_control)
            || input.prompt.trim().is_empty()
            || input.prompt.len() > 8192
            || input.prompt.contains('\0')
            || input.prompt.trim().starts_with('/')
        {
            return Err(ScheduleError::Invalid("invalid title or prompt"));
        }
        let request = serde_json::to_string(&input)?;
        let task_id = tools::stable_id(input.request_id, &request);
        schedule.title = input.title;
        schedule.prompt = input.prompt;
        schedule.notify = input.notify;
        let mut tx = self.pool.begin().await?;
        let updated = SqliteStore::apply_in(
            &mut tx,
            OwnerScope(principal),
            CommandEnvelope {
                command_id: input.request_id,
                command: ScheduleCommand::Update {
                    id,
                    expected_revision: input.revision,
                    timing: Some(input.timing),
                    task: Some(TaskRef {
                        id: task_id,
                        revision: 1,
                    }),
                    enabled: Some(input.enabled),
                },
            },
            now,
        )
        .await?;
        project(&mut schedule, &updated);
        sqlx::query("INSERT OR IGNORE INTO scheduled_tasks(id,revision,owner,route_json,request_json,record_json) VALUES(?,1,?,?,?,?)")
            .bind(task_id.to_string()).bind(principal.to_string()).bind(serde_json::to_string(&route)?).bind(request).bind(serde_json::to_string(&schedule)?).execute(&mut *tx).await?;
        tx.commit().await?;
        self.get(principal, id).await
    }
    pub async fn runs(&self, principal: Uuid, id: Uuid) -> ScheduleResult<Vec<ScheduleRun>> {
        let mut result = Vec::new();
        for event in self
            .core
            .occurrences(OwnerScope(principal), id, 0, 100)
            .await?
        {
            let row=sqlx::query("SELECT session_id,turn_id,execution_state,delivery_state FROM scheduled_admissions WHERE owner=? AND occurrence_id=?")
                .bind(principal.to_string()).bind(event.id.to_string()).fetch_optional(&self.pool).await?;
            let (session_id, turn_id, state, delivery) = if let Some(row) = row {
                (
                    Some(parse_uuid(row.get("session_id"))?),
                    Some(parse_uuid(row.get("turn_id"))?),
                    row.get("execution_state"),
                    row.get("delivery_state"),
                )
            } else {
                (
                    None,
                    None,
                    match event.state {
                        DispatchState::Pending | DispatchState::Dispatching => "claimed",
                        DispatchState::Deferred => "deferred",
                        DispatchState::Accepted => "submitted",
                        DispatchState::Rejected | DispatchState::Skipped => "skipped",
                        DispatchState::Unknown => "recovery_required",
                    }
                    .into(),
                    "pending".into(),
                )
            };
            result.push(ScheduleRun {
                occurrence_id: event.id,
                session_id,
                delivery_state: delivery,
                schedule_id: id,
                scheduled_at: event.planned_at,
                state,
                reason: event.reason,
                turn_id,
            });
        }
        Ok(result)
    }
    pub async fn firing(&self, event: &Occurrence) -> ScheduleResult<Firing> {
        let (route, schedule) = self.task(event.owner.0, &event.task).await?;
        Ok(Firing {
            principal: event.owner.0,
            route,
            schedule,
            event: event.clone(),
            request_key: event.request_key(),
        })
    }
}
fn project(s: &mut Schedule, p: &Plan) {
    s.enabled = p.enabled;
    s.deleted = p.deleted;
    s.revision = p.revision;
    s.next_fire = p.next_at;
    s.timing = p.spec.timing.clone();
}
pub(crate) fn parse_uuid(value: &str) -> ScheduleResult<Uuid> {
    Uuid::parse_str(value).map_err(|_| ScheduleError::Invalid("invalid persisted identity"))
}
