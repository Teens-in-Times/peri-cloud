//! A separate cloud host process is killed; the model and real executors stay alive.
use super::*;
use peri_cloud::gateway::SessionConnector;
use peri_cloud::state::FrozenSession;
use std::path::{Path, PathBuf};

struct CrashConnector {
    native: Arc<NativeSessionConnector>,
    hold_rotation: Option<PathBuf>,
}
#[async_trait]
impl SessionConnector for CrashConnector {
    async fn new_conversation(
        &self,
        principal: Uuid,
    ) -> Result<Arc<peri_cloud::CloudAgent>, GatewayError> {
        self.native.new_conversation(principal).await
    }
    async fn open_conversation(
        &self,
        principal: Uuid,
        frozen: &FrozenSession,
    ) -> Result<Arc<peri_cloud::CloudAgent>, GatewayError> {
        self.native.open_conversation(principal, frozen).await
    }
    async fn directories(
        &self,
        principal: Uuid,
        device: &DeviceRecord,
        query: peri_acp_types::device_executor::DirectoryQuery,
    ) -> Result<peri_acp_types::device_executor::DirectoryListing, GatewayError> {
        self.native.directories(principal, device, query).await
    }
    async fn open(
        &self,
        principal: Uuid,
        device: &DeviceRecord,
        previous: Option<&FrozenSession>,
    ) -> Result<Arc<peri_cloud::CloudAgent>, GatewayError> {
        if previous.is_none() {
            if let Some(path) = &self.hold_rotation {
                std::fs::write(path, b"fresh window preparation reached").unwrap();
                std::future::pending::<()>().await;
            }
        }
        self.native.open(principal, device, previous).await
    }
}

#[tokio::test]
#[ignore = "isolated child process entry, started only by the crash integration test"]
async fn crash_host_child() {
    let config: Value =
        serde_json::from_str(&std::env::var("PERI_FIXTURE_CHILD_CONFIG").unwrap()).unwrap();
    let root = PathBuf::from(config["root"].as_str().unwrap());
    let model = Arc::new(OpenAiModel::new(OpenAiConfig::new(
        config["model_url"].as_str().unwrap().parse().unwrap(),
        "fixture-model-key",
        "fixture-model",
    )));
    let native=NativeSessionConnector::new(model,"You are a personal cloud agent. Use only the selected device. Keep tool and reasoning events internal.".into(),128000).unwrap();
    for client in config["clients"].as_array().unwrap() {
        native.register_client(
            DeviceClient::connect(
                client["address"].as_str().unwrap().parse().unwrap(),
                client["token"].as_str().unwrap().into(),
                Uuid::parse_str(client["device"].as_str().unwrap()).unwrap(),
            )
            .await
            .unwrap(),
        );
    }
    let connector = Arc::new(CrashConnector {
        native,
        hold_rotation: config["hold_rotation"]
            .as_bool()
            .filter(|v| *v)
            .map(|_| root.join("rotation-ready")),
    });
    let limits = serde_json::from_value(config["limits"].clone()).unwrap();
    let cloud = CloudSide::start_with_limits(
        &root,
        Arc::new(Clock(AtomicI64::new(1900000000))),
        connector,
        Arc::new(Adapter::default()),
        false,
        true,
        limits,
    )
    .await;
    std::fs::write(root.join("child-ready"), b"cloud journal owner ready").unwrap();
    loop {
        cloud.gateway.dispatch().await.unwrap();
        tokio::time::sleep(Duration::from_millis(15)).await;
    }
}

pub(super) async fn launch(f: &mut Fixture, rotation: bool) -> tokio::process::Child {
    let old = f.cloud.take().unwrap();
    old.stop().await;
    drop(old);
    let mut config = f.crash_config.clone();
    config["root"] = json!(f.root.path());
    config["limits"] = serde_json::to_value(&f.limits).unwrap();
    config["hold_rotation"] = json!(rotation);
    let mut command = tokio::process::Command::new(std::env::current_exe().unwrap());
    command
        .args([
            "--ignored",
            "--exact",
            "execution_session_tests::crash_tests::crash_host_child",
            "--nocapture",
        ])
        .env("PERI_FIXTURE_CHILD_CONFIG", config.to_string())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    #[cfg(windows)]
    command.creation_flags(0x08000000);
    let mut child = command.spawn().unwrap();
    wait_file(&mut child, &f.root.path().join("child-ready")).await;
    child
}
async fn wait_file(child: &mut tokio::process::Child, path: &Path) {
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            assert!(
                child.try_wait().unwrap().is_none(),
                "cloud fixture process exited before the kill point"
            );
            if path.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
}
pub(super) async fn reopen(f: &mut Fixture) {
    f.cloud = Some(
        CloudSide::start_with_limits(
            f.root.path(),
            f.clock.clone(),
            f.connector.clone(),
            f.adapter.clone(),
            false,
            f.web,
            f.limits.clone(),
        )
        .await,
    );
}
async fn read_pool(f: &Fixture) -> sqlx::SqlitePool {
    sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(
            sqlx::sqlite::SqliteConnectOptions::new().filename(f.root.path().join("cloud.sqlite")),
        )
        .await
        .unwrap()
}

#[tokio::test]
async fn process_kill_preserves_wait_approval_native_submission_and_window_handoff() {
    for stage in ["wait", "approval", "submission", "rotation"] {
        eprintln!("process kill and recovery stage: {stage}");
        let action = match stage {
            "wait" => call(
                "wait_task",
                json!({"reason":"external event"}),
                "durable-wait",
            ),
            "rotation" => call(
                "Read",
                json!({"file_path":"source.txt"}),
                "read-before-rotation",
            ),
            _ => tool_write(),
        };
        let limits = if stage == "rotation" {
            peri_cloud::limits::RuntimeLimits {
                worker_window_tokens: 512,
                worker_reserve_tokens: 64,
                ..Default::default()
            }
        } else {
            Default::default()
        };
        let mut f = Fixture::new_for_limits(
            vec![
                call(
                    "delegate_task",
                    json!({"goal":format!("crash recovery {stage}")}),
                    "delegate",
                ),
                answer("delegated"),
                action,
            ],
            true,
            limits,
        )
        .await;
        std::fs::write(
            f.workspaces[0].path().join("source.txt"),
            "bounded handoff evidence ".repeat(4000),
        )
        .unwrap();
        let parent = f
            .c()
            .gateway
            .open_browser_workspace(&f.c().browser, Uuid::new_v4(), f.devices[0].id, None)
            .await
            .unwrap();
        let turn = f
            .c()
            .gateway
            .submit_browser_task(
                &f.c().browser,
                parent,
                Uuid::new_v4(),
                "delegate recoverable work".into(),
            )
            .await
            .unwrap();
        f.settled(parent, turn.turn_id).await;
        let receipt = delegate_receipt(&f.script.requests.lock()[1]);
        let task = receipt["task_id"].as_str().unwrap().to_string();
        if stage == "approval" {
            f.script.hold_review.store(true, Ordering::SeqCst);
        }
        if stage == "submission" {
            f.held_submissions[0].store(true, Ordering::SeqCst);
        }
        let mut child = launch(&mut f, stage == "rotation").await;
        if stage == "rotation" {
            wait_file(&mut child, &f.root.path().join("rotation-ready")).await;
        } else if stage == "submission" {
            wait_file(&mut child, &f.workspaces[0].path().join("agent.txt")).await;
        } else if stage == "approval" {
            tokio::time::timeout(Duration::from_secs(15), async {
                while f.script.review_requests.lock().is_empty() {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            })
            .await
            .unwrap();
        } else {
            let pool = read_pool(&f).await;
            tokio::time::timeout(Duration::from_secs(15), async {
                loop {
                    let state: Option<String> = sqlx::query_scalar(
                        "SELECT json_extract(record_json,'$.state') FROM agent_tasks WHERE id=?",
                    )
                    .bind(&task)
                    .fetch_optional(&pool)
                    .await
                    .unwrap();
                    if state.as_deref() == Some("waiting_external") {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            })
            .await
            .unwrap();
            pool.close().await;
        }
        assert!(
            child.try_wait().unwrap().is_none(),
            "kill a live owner, not an already stopped process: {stage}"
        );
        child.kill().await.unwrap();
        assert!(!child.wait().await.unwrap().success());
        f.script.hold_review.store(false, Ordering::SeqCst);
        f.script.review_release.notify_one();
        f.held_submissions[0].store(false, Ordering::SeqCst);
        f.submission_release[0].notify_one();
        if stage == "submission" {
            f.hidden_receipts[0].store(true, Ordering::SeqCst);
        }
        reopen(&mut f).await;
        if stage == "wait" {
            let pool = read_pool(&f).await;
            let revision: i64 = sqlx::query_scalar(
                "SELECT json_extract(record_json,'$.revision') FROM agent_tasks WHERE id=?",
            )
            .bind(&task)
            .fetch_one(&pool)
            .await
            .unwrap();
            pool.close().await;
            f.script.replies.lock().extend([
                call(
                    "control_task",
                    json!({"task_id":task,"expected_revision":revision,"action":"resume"}),
                    "resume",
                ),
                answer("resumed"),
                answer("worker continued"),
                answer("result received"),
            ]);
            let turn = f
                .c()
                .gateway
                .submit_browser_task(
                    &f.c().browser,
                    parent,
                    Uuid::new_v4(),
                    "resume the wait".into(),
                )
                .await
                .unwrap();
            f.settled(parent, turn.turn_id).await;
            pump(&f, 7).await;
        } else {
            if stage == "submission" {
                for _ in 0..3 {
                    f.c().gateway.dispatch().await.unwrap();
                }
                assert_eq!(f.submissions[0].load(Ordering::SeqCst), 1);
                assert_eq!(
                    f.script.requests.lock().len(),
                    3,
                    "missing original receipt must prevent new model work"
                );
                f.hidden_receipts[0].store(false, Ordering::SeqCst);
            }
            if stage == "approval" {
                f.script.replies.lock().push_back(tool_write());
            }
            f.script.replies.lock().extend([
                answer("worker resumed from durable facts"),
                answer("result received"),
            ]);
            pump(&f, if stage == "approval" { 6 } else { 5 }).await;
        }
        assert_eq!(
            f.submissions[0].load(Ordering::SeqCst),
            if stage == "wait" { 0 } else { 1 },
            "no replay at actual process kill point {stage}"
        );
        let pool = read_pool(&f).await;
        let record: String = sqlx::query_scalar("SELECT record_json FROM agent_tasks WHERE id=?")
            .bind(&task)
            .fetch_one(&pool)
            .await
            .unwrap();
        let record: Value = serde_json::from_str(&record).unwrap();
        assert_eq!(record["state"], "completed", "stage {stage}");
        assert!(
            record["run_number"].as_u64().unwrap() > 0,
            "recovery has a distinct durable run"
        );
        if stage == "rotation" {
            assert_ne!(record["worker_session"], receipt["worker_session"]);
        }
        pool.close().await;
        f.finish().await;
    }
}
