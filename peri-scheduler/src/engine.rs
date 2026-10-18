use crate::*;
use async_trait::async_trait;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

pub trait Clock: Send + Sync {
    fn now(&self) -> i64;
}
pub struct SystemClock;
impl Clock for SystemClock {
    fn now(&self) -> i64 {
        chrono::Utc::now().timestamp()
    }
}

/// Admission only. The cloud host observes actual execution overlap and owns
/// both run outcomes and delivery outcomes.
#[async_trait]
pub trait TaskDispatcher: Send + Sync {
    async fn submit(&self, event: &Occurrence) -> SubmitOutcome;
    async fn lookup(&self, event: &Occurrence) -> LookupOutcome;
}

#[derive(Clone, Debug, Default)]
pub struct DrainReport {
    pub processed: u64,
    pub unresolved: u64,
}
pub struct Scheduler {
    store: Arc<dyn SchedulerStore>,
    clock: Arc<dyn Clock>,
    dispatcher: Arc<dyn TaskDispatcher>,
    gate: tokio::sync::Mutex<()>,
}
impl Scheduler {
    pub fn new(
        store: Arc<dyn SchedulerStore>,
        clock: Arc<dyn Clock>,
        dispatcher: Arc<dyn TaskDispatcher>,
    ) -> Self {
        Self {
            store,
            clock,
            dispatcher,
            gate: tokio::sync::Mutex::new(()),
        }
    }
    pub async fn apply(&self, owner: OwnerScope, command: CommandEnvelope) -> Result<Plan> {
        self.store.apply(owner, command, self.clock.now()).await
    }
    pub async fn inspect(&self, owner: OwnerScope, query: ScheduleQuery) -> Result<QueryPage> {
        self.store.inspect(owner, query).await
    }
    /// A bounded dispatch step, useful for host integration and virtual clocks.
    /// The owner must keep this future alive even when its HTTP waiter leaves.
    pub async fn tick(&self) -> Result<DrainReport> {
        let _guard = self.gate.lock().await;
        let now = self.clock.now();
        self.store.claim_due(now).await?;
        let mut report = DrainReport::default();
        for event in self.store.pending(now).await? {
            let outcome = if event.state == DispatchState::Unknown {
                match self.dispatcher.lookup(&event).await {
                    LookupOutcome::Accepted { run_ref } => SubmitOutcome::Accepted { run_ref },
                    LookupOutcome::Unresolved => SubmitOutcome::Unknown,
                    LookupOutcome::DefinitelyNotAccepted => {
                        // Re-enter pending through a durable transition. A
                        // later tick rechecks pause/deadline before submission.
                        SubmitOutcome::Deferred {
                            until: now + 1,
                            reason: "authoritatively_not_admitted".into(),
                        }
                    }
                }
            } else {
                if !self.store.begin_dispatch(&event, now).await? {
                    continue;
                }
                self.dispatcher.submit(&event).await
            };
            if matches!(outcome, SubmitOutcome::Unknown) {
                report.unresolved += 1;
            }
            self.store.finish(&event, outcome, self.clock.now()).await?;
            report.processed += 1;
        }
        report.unresolved = self.store.unresolved().await?;
        Ok(report)
    }
    pub async fn run(&self, stop: CancellationToken) -> Result<DrainReport> {
        let mut report = DrainReport::default();
        let mut timer = tokio::time::interval(std::time::Duration::from_secs(1));
        timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {biased;_=stop.cancelled()=>{report.unresolved=self.store.unresolved().await?;return Ok(report);},_=timer.tick()=>{}}
            // Stop never drops an in-flight admission; finish it, then drain.
            let tick = self.tick().await?;
            report.processed += tick.processed;
            report.unresolved = tick.unresolved;
        }
    }
}
