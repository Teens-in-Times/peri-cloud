use super::*;
use peri_acp_types::device_executor::WorkspaceIdentity;

fn binding(root: &Path) -> SessionBinding {
    SessionBinding {
        session_id: Uuid::new_v4(),
        workspace: root.to_string_lossy().into_owned(),
        workspace_identity: WorkspaceIdentity {
            device: 1,
            inode: 2,
        },
    }
}

fn invocation(session: Uuid) -> SubmitJob {
    SubmitJob {
        invocation_id: Uuid::new_v4(),
        session_id: session,
        tool: "Write".into(),
        input: serde_json::json!({"file_path":"hello", "content":"one"}),
    }
}

#[tokio::test]
async fn invocation_identity_deduplicates_and_rejects_changed_requests() {
    let root = tempfile::tempdir().unwrap();
    let store = Store::open(root.path()).await.unwrap();
    let session = store.bind(binding(root.path())).await.unwrap();
    let request = invocation(session.session_id);
    assert!(store.create_job(&request).await.unwrap().0);
    assert!(!store.create_job(&request).await.unwrap().0);
    let mut changed = request.clone();
    changed.input["content"] = serde_json::json!("two");
    assert!(matches!(
        store.create_job(&changed).await,
        Err(Error::InvocationConflict)
    ));
    changed = request.clone();
    changed.session_id = Uuid::new_v4();
    assert!(matches!(
        store.create_job(&changed).await,
        Err(Error::InvocationConflict)
    ));
    assert_eq!(
        store.job(request.invocation_id).await.unwrap().status,
        JobStatus::Queued
    );
    store.close().await;
}

#[tokio::test]
async fn completed_results_survive_reopening_without_replay() {
    let root = tempfile::tempdir().unwrap();
    let store = Store::open(root.path()).await.unwrap();
    let session = store.bind(binding(root.path())).await.unwrap();
    let request = invocation(session.session_id);
    store.create_job(&request).await.unwrap();
    store
        .update(request.invocation_id, |job| {
            job.status = JobStatus::Completed;
            job.output = Some(peri_acp_types::tools::ToolOutput::from_legacy("saved"));
            job.finished_at = Some("finished".into());
        })
        .await
        .unwrap();
    store.close().await;
    let reopened = Store::open(root.path()).await.unwrap();
    let (created, job) = reopened.create_job(&request).await.unwrap();
    assert!(!created);
    assert_eq!(job.status, JobStatus::Completed);
    assert_eq!(job.output.unwrap().text, "saved");
    assert_eq!(job.finished_at.as_deref(), Some("finished"));
    reopened.close().await;
}

#[tokio::test]
async fn restart_preserves_unknown_execution_and_blocks_new_side_effects() {
    let root = tempfile::tempdir().unwrap();
    let store = Store::open(root.path()).await.unwrap();
    let session = store.bind(binding(root.path())).await.unwrap();
    let running = invocation(session.session_id);
    let queued = invocation(session.session_id);
    store.create_job(&running).await.unwrap();
    store.create_job(&queued).await.unwrap();
    store
        .update(running.invocation_id, |job| {
            job.status = JobStatus::Running;
            job.pid = Some(123);
            job.started_at = Some("started".into());
        })
        .await
        .unwrap();
    store.close().await;
    let reopened = Store::open(root.path()).await.unwrap();
    let (created, job) = reopened.create_job(&running).await.unwrap();
    assert!(!created);
    assert_eq!(job.status, JobStatus::RecoveryRequired);
    assert_eq!(job.pid, Some(123));
    assert!(job.finished_at.is_none());
    assert_eq!(
        reopened.job(queued.invocation_id).await.unwrap().status,
        JobStatus::Cancelled
    );
    assert!(matches!(
        reopened.create_job(&invocation(session.session_id)).await,
        Err(Error::RecoveryRequired)
    ));
    reopened.close().await;
}

#[tokio::test]
async fn workspace_binding_cannot_change_directory_or_object_identity() {
    let root = tempfile::tempdir().unwrap();
    let store = Store::open(root.path()).await.unwrap();
    let session = binding(root.path());
    assert_eq!(store.bind(session.clone()).await.unwrap(), session);
    let mut changed = session.clone();
    changed.workspace.push_str("/another");
    assert!(matches!(
        store.bind(changed).await,
        Err(Error::SessionConflict)
    ));
    let mut replaced = session.clone();
    replaced.workspace_identity.inode += 1;
    assert!(matches!(
        store.bind(replaced).await,
        Err(Error::SessionConflict)
    ));
    assert_eq!(store.session(session.session_id).await.unwrap(), session);
    store.close().await;
}
