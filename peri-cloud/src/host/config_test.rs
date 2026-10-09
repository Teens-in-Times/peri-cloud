use super::*;
use serde_json::{json, Value};

fn config() -> Value {
    json!({"state_dir":"state","listen":"127.0.0.1:8765","public_origin":"http://127.0.0.1:8765","bootstrap_env":"FIXTURE_SETUP_TOKEN","model":{"api_base":"https://model.example.test/v1","api_key_env":"FIXTURE_MODEL_KEY","model":"fixture-model"},"devices":[],"qq":null})
}

#[test]
fn configuration_defaults_keep_credentials_and_channel_optional() {
    let config: HostConfig = serde_json::from_value(config()).unwrap();
    config.validate().unwrap();
    assert!(config.qq.is_none());
    assert_eq!(config.max_iterations, 64);
    assert_eq!(config.model.context_window, 128000);
    assert_eq!(config.model.max_tokens, 32000);
    assert!(config.devices.is_empty());
}

#[test]
fn invalid_deployment_credentials_endpoints_or_shell_aliases_are_rejected() {
    for (field, value) in [
        ("listen", json!("0.0.0.0:8765")),
        ("public_origin", json!("http://agent.example.test")),
        ("bootstrap_env", json!("KEY; malicious")),
        ("max_iterations", json!(0)),
    ] {
        let mut input = config();
        input[field] = value;
        let config: HostConfig = serde_json::from_value(input).unwrap();
        assert!(matches!(config.validate(), Err(HostError::Configuration)));
    }
    let mut input = config();
    input["model"]["api_base"] = json!("https://secret@model.example.test/v1");
    assert!(matches!(
        serde_json::from_value::<HostConfig>(input)
            .unwrap()
            .validate(),
        Err(HostError::Configuration)
    ));
    let mut input = config();
    input["devices"] = json!([{"device_id":"00000000-0000-0000-0000-000000000042","endpoint":"127.0.0.1:8750","token_file":"device-token","ssh":{"host_alias":"-oProxyCommand=bad","remote_executor":"127.0.0.1:8740"}}]);
    assert!(matches!(
        serde_json::from_value::<HostConfig>(input)
            .unwrap()
            .validate(),
        Err(HostError::Configuration)
    ));
}

#[test]
fn duplicate_devices_or_forward_ports_cannot_replace_another_device() {
    let mut input = config();
    let device = json!({"device_id":"00000000-0000-0000-0000-000000000042","endpoint":"127.0.0.1:8750","token_file":"device-token"});
    input["devices"] = json!([device.clone(), device]);
    assert!(matches!(
        serde_json::from_value::<HostConfig>(input)
            .unwrap()
            .validate(),
        Err(HostError::Configuration)
    ));
    let mut input = config();
    input["model"]["api_key"] = json!("must-not-be-inline");
    assert!(serde_json::from_value::<HostConfig>(input).is_err());
}

#[tokio::test]
async fn relative_paths_resolve_from_config_file_instead_of_process_cwd() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("host.json");
    let mut input = config();
    input["devices"] = json!([{"device_id":"00000000-0000-0000-0000-000000000042","endpoint":"127.0.0.1:8750","token_file":"device-token"}]);
    tokio::fs::write(&path, serde_json::to_vec(&input).unwrap())
        .await
        .unwrap();
    let config = HostConfig::read(&path).await.unwrap();
    let root = tokio::fs::canonicalize(root.path()).await.unwrap();
    assert_eq!(config.state_dir, root.join("state"));
    assert_eq!(config.devices[0].token_file, root.join("device-token"));
}
