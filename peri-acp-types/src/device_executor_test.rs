use super::*;

#[test]
fn request_roundtrip_preserves_invocation_and_target() {
    let request = SubmitJob {
        invocation_id: Uuid::new_v4(),
        session_id: Uuid::new_v4(),
        tool: "Write".into(),
        input: serde_json::json!({"file_path":"测试.md", "content":"你好\n"}),
    };
    let encoded = serde_json::to_string(&request).unwrap();
    let decoded: SubmitJob = serde_json::from_str(&encoded).unwrap();
    assert_eq!(decoded, request);
    assert!(!encoded.contains("ssh"));
}

#[test]
fn protocol_rejects_unrecognized_execution_parameters() {
    let request = serde_json::json!({
        "invocation_id":Uuid::new_v4(), "session_id":Uuid::new_v4(),
        "tool":"Bash", "input":{"command":"true"}, "workspace":"/other"
    });
    assert!(serde_json::from_value::<SubmitJob>(request).is_err());
}

#[test]
fn recovery_and_running_are_not_terminal() {
    for status in [
        JobStatus::Queued,
        JobStatus::Running,
        JobStatus::RecoveryRequired,
    ] {
        let json = serde_json::to_string(&status).unwrap();
        assert_eq!(serde_json::from_str::<JobStatus>(&json).unwrap(), status);
        assert!(!status.is_terminal());
    }
    assert_eq!(
        serde_json::to_string(&JobStatus::RecoveryRequired).unwrap(),
        "\"recovery_required\""
    );
    assert!(JobStatus::Cancelled.is_terminal());
}
