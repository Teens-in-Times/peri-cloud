use super::*;

#[test]
fn test_cli_preserves_executor_start_and_adds_native_login_entry() {
    let serve = Args::try_parse_from([
        "peri-executor",
        "--state-dir",
        "fixture",
        "--listen",
        "127.0.0.1:42371",
    ])
    .unwrap();
    assert!(serve.command.is_none());
    assert_eq!(serve.listen, "127.0.0.1:42371".parse().unwrap());
    let login = Args::try_parse_from([
        "peri-executor",
        "--state-dir",
        "fixture",
        "login",
        "--cloud-url",
        "https://agent.example.test",
        "--workspace",
        ".",
        "--no-browser",
    ])
    .unwrap();
    assert_eq!(login.state_dir, PathBuf::from("fixture"));
    assert!(matches!(
        login.command,
        Some(Command::Login {
            no_browser: true,
            ..
        })
    ));
}

#[test]
fn test_cli_requires_explicit_cloud_and_persistent_state() {
    let error = Args::try_parse_from(["peri-executor", "--state-dir", "fixture", "login"])
        .err()
        .unwrap();
    assert_eq!(
        error.kind(),
        clap::error::ErrorKind::MissingRequiredArgument
    );
    let error = Args::try_parse_from([
        "peri-executor",
        "login-status",
        "--cloud-url",
        "https://agent.example.test",
    ])
    .err()
    .unwrap();
    assert_eq!(
        error.kind(),
        clap::error::ErrorKind::MissingRequiredArgument
    );
}
