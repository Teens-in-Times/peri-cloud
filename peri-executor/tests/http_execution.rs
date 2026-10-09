use std::sync::Arc;
use std::time::Duration;

use peri_acp_types::device_executor::*;
use peri_executor::{web, Executor};
use reqwest::Client;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

struct Harness {
    executor: Arc<Executor>,
    state: tempfile::TempDir,
    workspace: tempfile::TempDir,
    client: Client,
    base: String,
    token: String,
    session: Uuid,
    stop: CancellationToken,
    server: tokio::task::JoinHandle<()>,
}

impl Harness {
    async fn new() -> Self {
        let state = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let executor = Arc::new(Executor::open(state.path(), "test-device").await.unwrap());
        let token = std::fs::read_to_string(state.path().join("transport-token")).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let app = web::router(executor.clone());
        let stop = CancellationToken::new();
        let signal = stop.clone();
        let server = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(signal.cancelled_owned())
                .await
                .unwrap();
        });
        let client = Client::builder().no_proxy().build().unwrap();
        let session = Uuid::new_v4();
        let response = client
            .put(format!("{base}/v1/sessions/{session}"))
            .bearer_auth(&token)
            .json(&OpenSession {
                workspace: workspace.path().to_string_lossy().into_owned(),
            })
            .send()
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            reqwest::StatusCode::OK,
            "{}",
            response.text().await.unwrap()
        );
        Self {
            executor,
            state,
            workspace,
            client,
            base,
            token,
            session,
            stop,
            server,
        }
    }

    fn request(&self, tool: &str, input: serde_json::Value) -> SubmitJob {
        SubmitJob {
            invocation_id: Uuid::new_v4(),
            session_id: self.session,
            tool: tool.into(),
            input,
        }
    }

    async fn submit(&self, request: &SubmitJob) -> JobReceipt {
        let response = self
            .client
            .post(format!("{}/v1/jobs", self.base))
            .bearer_auth(&self.token)
            .json(request)
            .send()
            .await
            .unwrap();
        assert!(
            response.status().is_success(),
            "{}",
            response.text().await.unwrap()
        );
        response.json().await.unwrap()
    }

    async fn job(&self, id: Uuid) -> JobSnapshot {
        self.client
            .get(format!("{}/v1/jobs/{id}", self.base))
            .bearer_auth(&self.token)
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap()
    }

    async fn finished(&self, id: Uuid) -> JobSnapshot {
        tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                let job = self.job(id).await;
                if job.status.is_terminal() || job.status == JobStatus::RecoveryRequired {
                    return job;
                }
                tokio::time::sleep(Duration::from_millis(30)).await;
            }
        })
        .await
        .expect("invocation should settle")
    }

    async fn started(&self, id: Uuid) -> JobSnapshot {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let job = self.job(id).await;
                if job.pid.is_some() {
                    return job;
                }
                assert!(
                    !job.status.is_terminal(),
                    "Shell stopped before publishing process evidence: {job:?}"
                );
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("shell should start")
    }

    async fn close(self) {
        self.stop.cancel();
        self.server.await.unwrap();
        self.executor.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn authentication_and_core_tool_catalog_use_the_real_protocol() {
    let h = Harness::new().await;
    let url = format!("{}/v1/info", h.base);
    assert_eq!(h.client.get(&url).send().await.unwrap().status(), 401);
    assert_eq!(
        h.client
            .get(&url)
            .bearer_auth("incorrect")
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    let info: ExecutorInfo = h
        .client
        .get(&url)
        .bearer_auth(&h.token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(info.device_name, "test-device");
    assert_eq!(info.protocol_version, 1);
    let tools: Vec<NativeToolDescriptor> = h
        .client
        .get(format!("{}/v1/sessions/{}/tools", h.base, h.session))
        .bearer_auth(&h.token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        tools
            .iter()
            .map(|t| t.definition.name.as_str())
            .collect::<Vec<_>>(),
        ["Read", "Write", "Edit", "Glob", "Grep", "Bash"]
    );
    assert_eq!(tools.last().unwrap().aliases, ["Shell"]);
    let changed = h
        .client
        .put(format!("{}/v1/sessions/{}", h.base, h.session))
        .bearer_auth(&h.token)
        .json(&OpenSession {
            workspace: h.state.path().to_string_lossy().into_owned(),
        })
        .send()
        .await
        .unwrap();
    assert_eq!(changed.status(), 409);
    h.close().await;
}

#[tokio::test]
async fn write_retry_is_deduplicated_and_read_edit_search_work_on_the_bound_workspace() {
    let h = Harness::new().await;
    let write = h.request(
        "Write",
        serde_json::json!({"file_path":"note.md", "content":"你好 needle\n", "append":true}),
    );
    assert!(h.submit(&write).await.created);
    assert_eq!(
        h.finished(write.invocation_id).await.status,
        JobStatus::Completed
    );
    assert!(!h.submit(&write).await.created);
    assert_eq!(
        std::fs::read_to_string(h.workspace.path().join("note.md")).unwrap(),
        "你好 needle\n"
    );
    let mut changed = write.clone();
    changed.input["content"] = serde_json::json!("different");
    let response = h
        .client
        .post(format!("{}/v1/jobs", h.base))
        .bearer_auth(&h.token)
        .json(&changed)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 409);
    let read = h.request("Read", serde_json::json!({"file_path":"note.md"}));
    h.submit(&read).await;
    assert!(h
        .finished(read.invocation_id)
        .await
        .output
        .unwrap()
        .text
        .contains("你好 needle"));
    let edit = h.request(
        "Edit",
        serde_json::json!({"file_path":"note.md", "old_string":"needle", "new_string":"changed"}),
    );
    h.submit(&edit).await;
    assert_eq!(
        h.finished(edit.invocation_id).await.status,
        JobStatus::Completed
    );
    let glob = h.request("Glob", serde_json::json!({"pattern":"*.md"}));
    h.submit(&glob).await;
    assert!(h
        .finished(glob.invocation_id)
        .await
        .output
        .unwrap()
        .text
        .contains("note.md"));
    let grep = h.request(
        "Grep",
        serde_json::json!({"pattern":"changed", "path":".", "output_mode":"content"}),
    );
    h.submit(&grep).await;
    assert!(h
        .finished(grep.invocation_id)
        .await
        .output
        .unwrap()
        .text
        .contains("changed"));
    let missing = h.request("Read", serde_json::json!({"file_path":"missing"}));
    h.submit(&missing).await;
    assert_eq!(
        h.finished(missing.invocation_id).await.status,
        JobStatus::Failed
    );
    h.close().await;
}

#[tokio::test]
async fn shell_alias_captures_stdout_stderr_and_nonzero_exit_evidence() {
    let h = Harness::new().await;
    let command = if cfg!(windows) {
        "[Console]::Write('shell-你好'); [Console]::Error.Write('problem'); exit 7"
    } else {
        "printf 'shell-你好'; printf 'problem' >&2; exit 7"
    };
    let request = h.request("Shell", serde_json::json!({"command":command}));
    h.submit(&request).await;
    let job = h.finished(request.invocation_id).await;
    assert_eq!(job.status, JobStatus::Failed, "{job:?}");
    let output = job.output.unwrap();
    assert!(output.text.contains("shell-你好"), "{}", output.text);
    assert!(output.text.contains("problem"));
    assert_eq!(output.execution.unwrap().exit_code, Some(7));
    assert!(std::path::Path::new(job.stdout_path.as_deref().unwrap()).exists());
    h.close().await;
}

#[tokio::test]
async fn cancel_settles_only_after_process_tree_exit() {
    let h = Harness::new().await;
    let command = if cfg!(windows) {
        "Start-Sleep -Seconds 30"
    } else {
        "sleep 30"
    };
    let request = h.request(
        "Bash",
        serde_json::json!({"command":command,"run_in_background":true}),
    );
    h.submit(&request).await;
    let started = h.started(request.invocation_id).await;
    assert_eq!(started.status, JobStatus::Running);
    let response = h
        .client
        .post(format!(
            "{}/v1/jobs/{}/cancel",
            h.base, request.invocation_id
        ))
        .bearer_auth(&h.token)
        .send()
        .await
        .unwrap();
    assert!(response.status().is_success());
    let job = h.finished(request.invocation_id).await;
    assert_eq!(job.status, JobStatus::Cancelled, "{job:?}");
    assert!(job.cancel_requested);
    assert!(job.finished_at.is_some());
    assert_eq!(
        job.output.unwrap().execution.unwrap().status,
        peri_acp_types::tools::ToolExecutionStatus::Cancelled
    );
    h.close().await;
}

#[tokio::test]
async fn background_timeout_requests_termination_and_records_the_actual_terminal_state() {
    let h = Harness::new().await;
    let command = if cfg!(windows) {
        "Start-Sleep -Seconds 30"
    } else {
        "sleep 30"
    };
    let request = h.request(
        "Bash",
        serde_json::json!({"command":command,"run_in_background":true,"timeout":200}),
    );
    h.submit(&request).await;
    let job = h.finished(request.invocation_id).await;
    assert_eq!(job.status, JobStatus::TimedOut, "{job:?}");
    assert!(!job.cancel_requested);
    assert!(job.finished_at.is_some());
    h.close().await;
}

#[tokio::test]
async fn stopping_the_http_transport_does_not_cancel_admitted_shell_work() {
    let mut h = Harness::new().await;
    let command = if cfg!(windows) {
        "Start-Sleep -Milliseconds 700; [IO.File]::WriteAllText((Join-Path (Get-Location) 'finished.txt'), 'done')"
    } else {
        "sleep 0.7; printf done > finished.txt"
    };
    let request = h.request("Bash", serde_json::json!({"command":command}));
    h.submit(&request).await;
    h.started(request.invocation_id).await;
    h.stop.cancel();
    h.server.await.unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    h.base = format!("http://{}", listener.local_addr().unwrap());
    let app = web::router(h.executor.clone());
    h.stop = CancellationToken::new();
    let stop = h.stop.clone();
    h.server = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(stop.cancelled_owned())
            .await
            .unwrap();
    });
    let job = h.finished(request.invocation_id).await;
    assert_eq!(job.status, JobStatus::Completed, "{job:?}");
    assert_eq!(
        std::fs::read_to_string(h.workspace.path().join("finished.txt")).unwrap(),
        "done"
    );
    assert!(!h.submit(&request).await.created);
    h.close().await;
}

#[tokio::test]
async fn replaced_workspace_is_rejected_without_writing_to_the_replacement() {
    let h = Harness::new().await;
    let bound = h.workspace.path().join("bound");
    let moved = h.workspace.path().join("moved");
    std::fs::create_dir(&bound).unwrap();
    let session = Uuid::new_v4();
    let response = h
        .client
        .put(format!("{}/v1/sessions/{session}", h.base))
        .bearer_auth(&h.token)
        .json(&OpenSession {
            workspace: bound.to_string_lossy().into_owned(),
        })
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    std::fs::rename(&bound, &moved).unwrap();
    std::fs::create_dir(&bound).unwrap();
    let mut request = h.request(
        "Write",
        serde_json::json!({"file_path":"side-effect", "content":"no"}),
    );
    request.session_id = session;
    let response = h
        .client
        .post(format!("{}/v1/jobs", h.base))
        .bearer_auth(&h.token)
        .json(&request)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 409);
    assert!(!bound.join("side-effect").exists());
    h.close().await;
}
