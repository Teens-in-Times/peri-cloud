use async_trait::async_trait;
use peri_scheduler::*;
use sqlx::sqlite::SqlitePoolOptions;
use std::sync::{
    atomic::{AtomicI64, AtomicUsize, Ordering},
    Arc, Mutex,
};
use uuid::Uuid;

struct TestClock(AtomicI64);
impl Clock for TestClock {
    fn now(&self) -> i64 {
        self.0.load(Ordering::SeqCst)
    }
}
struct Dispatcher {
    calls: AtomicUsize,
    lookups: AtomicUsize,
    submits: Mutex<Vec<Occurrence>>,
    unknown: bool,
}
#[async_trait]
impl TaskDispatcher for Dispatcher {
    async fn submit(&self, e: &Occurrence) -> SubmitOutcome {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.submits.lock().unwrap().push(e.clone());
        if self.unknown {
            SubmitOutcome::Unknown
        } else {
            SubmitOutcome::Accepted {
                run_ref: "original-run".into(),
            }
        }
    }
    async fn lookup(&self, _: &Occurrence) -> LookupOutcome {
        self.lookups.fetch_add(1, Ordering::SeqCst);
        LookupOutcome::Accepted {
            run_ref: "original-run".into(),
        }
    }
}
async fn fixture(unknown: bool) -> (Arc<SqliteStore>, Arc<TestClock>, Arc<Dispatcher>, Scheduler) {
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .unwrap();
    let store = Arc::new(SqliteStore::from_pool(pool));
    store.migrate().await.unwrap();
    let clock = Arc::new(TestClock(AtomicI64::new(1_900_000_000)));
    let dispatch = Arc::new(Dispatcher {
        calls: AtomicUsize::new(0),
        lookups: AtomicUsize::new(0),
        submits: Mutex::new(Vec::new()),
        unknown,
    });
    let scheduler = Scheduler::new(store.clone(), clock.clone(), dispatch.clone());
    (store, clock, dispatch, scheduler)
}
fn command(id: Uuid, timing: Timing) -> CommandEnvelope {
    CommandEnvelope {
        command_id: id,
        command: ScheduleCommand::Create {
            spec: ScheduleSpec {
                id,
                task: TaskRef { id, revision: 1 },
                timing,
                policy: Policy::default(),
            },
        },
    }
}
#[tokio::test]
async fn command_idempotency_is_scoped_immutable_and_preserves_relative_anchor() {
    let (store, clock, _, scheduler) = fixture(false).await;
    let owner = OwnerScope(Uuid::new_v4());
    let id = Uuid::new_v4();
    let input = command(id, Timing::After { seconds: 60 });
    let first = scheduler.apply(owner, input.clone()).await.unwrap();
    clock.0.fetch_add(15, Ordering::SeqCst);
    assert_eq!(scheduler.apply(owner, input).await.unwrap(), first);
    assert!(matches!(
        scheduler
            .apply(owner, command(id, Timing::After { seconds: 90 }))
            .await,
        Err(Error::Conflict)
    ));
    assert!(matches!(
        store.get(OwnerScope(Uuid::new_v4()), id).await,
        Err(Error::Forbidden)
    ));
    let pause = CommandEnvelope {
        command_id: Uuid::new_v4(),
        command: ScheduleCommand::Update {
            id,
            expected_revision: 1,
            timing: None,
            task: None,
            enabled: Some(false),
        },
    };
    let paused = scheduler.apply(owner, pause.clone()).await.unwrap();
    assert_eq!(scheduler.apply(owner, pause).await.unwrap(), paused);
    let stale = CommandEnvelope {
        command_id: Uuid::new_v4(),
        command: ScheduleCommand::Remove {
            id,
            expected_revision: 1,
        },
    };
    assert!(matches!(
        scheduler.apply(owner, stale).await,
        Err(Error::Conflict)
    ));
}
#[tokio::test]
async fn lost_admission_receipt_is_queried_without_a_second_submission_after_restart() {
    let (store, clock, dispatch, scheduler) = fixture(true).await;
    let owner = OwnerScope(Uuid::new_v4());
    let id = Uuid::new_v4();
    scheduler
        .apply(owner, command(id, Timing::After { seconds: 1 }))
        .await
        .unwrap();
    clock.0.fetch_add(1, Ordering::SeqCst);
    scheduler.tick().await.unwrap();
    assert_eq!(dispatch.calls.load(Ordering::SeqCst), 1);
    store.recover_owned().await.unwrap();
    clock.0.fetch_add(30, Ordering::SeqCst);
    let restarted = Scheduler::new(store.clone(), clock, dispatch.clone());
    restarted.tick().await.unwrap();
    assert_eq!(dispatch.calls.load(Ordering::SeqCst), 1);
    assert_eq!(dispatch.lookups.load(Ordering::SeqCst), 1);
    let events = store.occurrences(owner, id, 0, 100).await.unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].state, DispatchState::Accepted);
    assert_eq!(events[0].run_ref.as_deref(), Some("original-run"));
}
#[tokio::test]
async fn repeated_concurrent_ticks_claim_one_occurrence_and_do_not_advance_command_revision() {
    let (store, clock, dispatch, scheduler) = fixture(false).await;
    let owner = OwnerScope(Uuid::new_v4());
    let id = Uuid::new_v4();
    let plan = scheduler
        .apply(owner, command(id, Timing::After { seconds: 1 }))
        .await
        .unwrap();
    clock.0.fetch_add(1, Ordering::SeqCst);
    let (a, b) = tokio::join!(scheduler.tick(), scheduler.tick());
    a.unwrap();
    b.unwrap();
    assert_eq!(dispatch.calls.load(Ordering::SeqCst), 1);
    assert_eq!(store.get(owner, id).await.unwrap().revision, plan.revision);
}
#[tokio::test]
async fn expired_greeting_never_calls_the_dispatcher() {
    let (store, clock, dispatch, scheduler) = fixture(false).await;
    let owner = OwnerScope(Uuid::new_v4());
    let id = Uuid::new_v4();
    scheduler
        .apply(owner, command(id, Timing::After { seconds: 1 }))
        .await
        .unwrap();
    clock.0.fetch_add(302, Ordering::SeqCst);
    scheduler.tick().await.unwrap();
    assert_eq!(dispatch.calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        store.occurrences(owner, id, 0, 100).await.unwrap()[0]
            .reason
            .as_deref(),
        Some("misfire_expired")
    );
}
#[tokio::test]
async fn schema_metadata_never_changes_the_host_user_version() {
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .unwrap();
    sqlx::query("PRAGMA user_version=73")
        .execute(&pool)
        .await
        .unwrap();
    SqliteStore::from_pool(pool.clone())
        .migrate()
        .await
        .unwrap();
    assert_eq!(
        sqlx::query_as::<_, (i64,)>("PRAGMA user_version")
            .fetch_one(&pool)
            .await
            .unwrap()
            .0,
        73
    );
    sqlx::query("UPDATE scheduler_metadata SET version=99")
        .execute(&pool)
        .await
        .unwrap();
    assert!(matches!(
        SqliteStore::from_pool(pool).migrate().await,
        Err(Error::Version)
    ));
}

struct BusyOnce {
    calls: AtomicUsize,
    seen: Mutex<Vec<Occurrence>>,
}
#[async_trait]
impl TaskDispatcher for BusyOnce {
    async fn submit(&self, event: &Occurrence) -> SubmitOutcome {
        self.seen.lock().unwrap().push(event.clone());
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            SubmitOutcome::Deferred {
                until: event.planned_at + 5,
                reason: "foreground_busy".into(),
            }
        } else {
            SubmitOutcome::Accepted {
                run_ref: "run".into(),
            }
        }
    }
    async fn lookup(&self, _: &Occurrence) -> LookupOutcome {
        LookupOutcome::Unresolved
    }
}
#[tokio::test]
async fn pausing_a_deferred_one_shot_suppresses_it_even_after_natural_exhaustion() {
    let (store, clock, _, _) = fixture(false).await;
    let dispatcher = Arc::new(BusyOnce {
        calls: AtomicUsize::new(0),
        seen: Mutex::new(Vec::new()),
    });
    let scheduler = Scheduler::new(store.clone(), clock.clone(), dispatcher.clone());
    let owner = OwnerScope(Uuid::new_v4());
    let id = Uuid::new_v4();
    scheduler
        .apply(owner, command(id, Timing::After { seconds: 1 }))
        .await
        .unwrap();
    clock.0.fetch_add(1, Ordering::SeqCst);
    scheduler.tick().await.unwrap();
    assert!(!store.get(owner, id).await.unwrap().enabled);
    scheduler
        .apply(
            owner,
            CommandEnvelope {
                command_id: Uuid::new_v4(),
                command: ScheduleCommand::Update {
                    id,
                    expected_revision: 1,
                    timing: None,
                    task: None,
                    enabled: Some(false),
                },
            },
        )
        .await
        .unwrap();
    clock.0.fetch_add(5, Ordering::SeqCst);
    scheduler.tick().await.unwrap();
    assert_eq!(dispatcher.calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        store.occurrences(owner, id, 0, 100).await.unwrap()[0].state,
        DispatchState::Skipped
    );
}
#[tokio::test]
async fn editing_preserves_the_task_snapshot_of_an_already_deferred_occurrence() {
    let (store, clock, _, _) = fixture(false).await;
    let dispatcher = Arc::new(BusyOnce {
        calls: AtomicUsize::new(0),
        seen: Mutex::new(Vec::new()),
    });
    let scheduler = Scheduler::new(store.clone(), clock.clone(), dispatcher.clone());
    let owner = OwnerScope(Uuid::new_v4());
    let id = Uuid::new_v4();
    scheduler
        .apply(owner, command(id, Timing::After { seconds: 1 }))
        .await
        .unwrap();
    clock.0.fetch_add(1, Ordering::SeqCst);
    scheduler.tick().await.unwrap();
    scheduler
        .apply(
            owner,
            CommandEnvelope {
                command_id: Uuid::new_v4(),
                command: ScheduleCommand::Update {
                    id,
                    expected_revision: 1,
                    timing: Some(Timing::After { seconds: 60 }),
                    task: Some(TaskRef {
                        id: Uuid::new_v4(),
                        revision: 2,
                    }),
                    enabled: Some(true),
                },
            },
        )
        .await
        .unwrap();
    clock.0.fetch_add(5, Ordering::SeqCst);
    scheduler.tick().await.unwrap();
    let seen = dispatcher.seen.lock().unwrap();
    assert_eq!(seen.len(), 2);
    assert_eq!(seen[0].id, seen[1].id);
    assert_eq!(seen[1].task, TaskRef { id, revision: 1 });
}
#[tokio::test]
async fn a_clock_rollback_and_plan_edit_cannot_recreate_an_already_accepted_slot() {
    let (store, clock, dispatcher, scheduler) = fixture(false).await;
    let owner = OwnerScope(Uuid::new_v4());
    let id = Uuid::new_v4();
    let start = clock.now();
    scheduler
        .apply(owner, command(id, Timing::After { seconds: 1 }))
        .await
        .unwrap();
    clock.0.store(start + 1, Ordering::SeqCst);
    scheduler.tick().await.unwrap();
    clock.0.store(start, Ordering::SeqCst);
    scheduler
        .apply(
            owner,
            CommandEnvelope {
                command_id: Uuid::new_v4(),
                command: ScheduleCommand::Update {
                    id,
                    expected_revision: 1,
                    timing: Some(Timing::After { seconds: 1 }),
                    task: Some(TaskRef { id, revision: 2 }),
                    enabled: Some(true),
                },
            },
        )
        .await
        .unwrap();
    clock.0.store(start + 1, Ordering::SeqCst);
    scheduler.tick().await.unwrap();
    assert_eq!(dispatcher.calls.load(Ordering::SeqCst), 1);
    assert_eq!(store.occurrences(owner, id, 0, 100).await.unwrap().len(), 1);
}
struct Blocking {
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
}
#[async_trait]
impl TaskDispatcher for Blocking {
    async fn submit(&self, _: &Occurrence) -> SubmitOutcome {
        self.entered.notify_one();
        self.release.notified().await;
        SubmitOutcome::Accepted {
            run_ref: "held-future".into(),
        }
    }
    async fn lookup(&self, _: &Occurrence) -> LookupOutcome {
        LookupOutcome::Unresolved
    }
}
#[tokio::test]
async fn shutdown_keeps_the_inflight_admission_alive_until_its_receipt_is_recorded() {
    let (store, clock, _, _) = fixture(false).await;
    let dispatcher = Arc::new(Blocking {
        entered: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
    });
    let scheduler = Arc::new(Scheduler::new(
        store.clone(),
        clock.clone(),
        dispatcher.clone(),
    ));
    let owner = OwnerScope(Uuid::new_v4());
    let id = Uuid::new_v4();
    scheduler
        .apply(owner, command(id, Timing::After { seconds: 1 }))
        .await
        .unwrap();
    clock.0.fetch_add(1, Ordering::SeqCst);
    let stop = tokio_util::sync::CancellationToken::new();
    let worker = scheduler.clone();
    let worker_stop = stop.clone();
    let handle = tokio::spawn(async move { worker.run(worker_stop).await });
    dispatcher.entered.notified().await;
    stop.cancel();
    assert!(!handle.is_finished());
    dispatcher.release.notify_one();
    let report = tokio::time::timeout(std::time::Duration::from_secs(3), handle)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(report.processed, 1);
    assert_eq!(report.unresolved, 0);
    assert_eq!(
        store.occurrences(owner, id, 0, 100).await.unwrap()[0]
            .run_ref
            .as_deref(),
        Some("held-future")
    );
}
