use super::*;
use serde_json::{json, Value};

fn config() -> Value {
    json!({"state_dir":"state","listen":"127.0.0.1:8765","public_origin":"http://127.0.0.1:8765","bootstrap_env":"FIXTURE_SETUP_TOKEN","model":{"api_base":"https://model.example.test/v1","api_key_env":"FIXTURE_MODEL_KEY","model":"fixture-model"},"devices":[],"qq":null})
}

#[test]
fn wireguard_is_explicit_and_does_not_replace_ssh_fallback() {
    let mut input = config();
    input["devices"] = json!([{"device_id":"00000000-0000-0000-0000-000000000042","endpoint":"127.0.0.1:8750","token_file":"device-token", "wireguard":{"interface":"wg77","endpoint":"10.77.0.3:42371"}}]);
    serde_json::from_value::<HostConfig>(input.clone())
        .unwrap()
        .validate()
        .unwrap();
    for endpoint in ["8.8.8.8:42371", "0.0.0.0:42371", "127.0.0.1:42371"] {
        input["devices"][0]["wireguard"]["endpoint"] = json!(endpoint);
        assert!(serde_json::from_value::<HostConfig>(input.clone())
            .unwrap()
            .validate()
            .is_err());
    }
}

#[test]
fn configuration_defaults_keep_credentials_and_channel_optional() {
    let config: HostConfig = serde_json::from_value(config()).unwrap();
    config.validate().unwrap();
    assert!(config.qq.is_none());
    assert!(config.onebot.is_none());
    assert_eq!(config.max_iterations, 64);
    assert_eq!(config.model.context_window, 128000);
    assert_eq!(config.model.max_tokens, 32000);
    assert!(config.devices.is_empty());
}

#[test]
fn onebot_is_optional_independent_and_requires_a_private_distinct_listener() {
    let mut input = config();
    input["qq"] = json!({"instance_id":"official","app_id_env":"QQ_APP","client_secret_env":"QQ_SECRET","transport":"websocket"});
    input["onebot"] = json!({"instance_id":"personal","self_id":"12345678","listen":"127.0.0.1:8766","access_token_env":"ONEBOT_TOKEN"});
    serde_json::from_value::<HostConfig>(input.clone())
        .unwrap()
        .validate()
        .unwrap();
    for (field, value) in [
        ("instance_id", json!("official")),
        ("self_id", json!("00123")),
        ("self_id", json!("-42")),
        ("listen", json!("0.0.0.0:8766")),
        ("listen", json!("127.0.0.1:8765")),
        ("access_token_env", json!("bad token")),
    ] {
        let mut bad = input.clone();
        bad["onebot"][field] = value;
        assert!(serde_json::from_value::<HostConfig>(bad)
            .unwrap()
            .validate()
            .is_err());
    }
    input["qq"] = Value::Null;
    serde_json::from_value::<HostConfig>(input)
        .unwrap()
        .validate()
        .unwrap();
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

#[tokio::test]
async fn isolated_instance_uses_a_unique_loopback_connection_and_excludes_legacy_local() {
    let mut input = config();
    input["isolated_instance"] = json!({"name":"Owner only","workspace":"/workspace","connection":{
        "device_id":"00000000-0000-0000-0000-000000000042","endpoint":"127.0.0.1:52371","token_file":"owner-token"
    }});
    serde_json::from_value::<HostConfig>(input.clone())
        .unwrap()
        .validate()
        .unwrap();
    for change in [
        json!({"local_instance":{"name":"Legacy","workspace":std::env::temp_dir()}}),
        json!({"devices":[input["isolated_instance"]["connection"].clone()]}),
    ] {
        let mut changed = input.clone();
        for (key, value) in change.as_object().unwrap() {
            changed[key] = value.clone();
        }
        assert!(serde_json::from_value::<HostConfig>(changed)
            .unwrap()
            .validate()
            .is_err());
    }
    let mut changed = input.clone();
    changed["isolated_instance"]["connection"]["endpoint"] = json!("0.0.0.0:52371");
    assert!(serde_json::from_value::<HostConfig>(changed)
        .unwrap()
        .validate()
        .is_err());
    let mut changed = input.clone();
    changed["isolated_instance"]["workspace"] = json!("relative/workspace");
    assert!(serde_json::from_value::<HostConfig>(changed)
        .unwrap()
        .validate()
        .is_err());
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("host.json");
    std::fs::write(&path, serde_json::to_vec(&input).unwrap()).unwrap();
    let read = HostConfig::read(&path).await.unwrap();
    assert_eq!(
        read.isolated_instance.unwrap().connection.token_file,
        tokio::fs::canonicalize(root.path())
            .await
            .unwrap()
            .join("owner-token")
    );
}

#[test]
fn account_limits_and_provider_ids_cannot_bypass_operator_boundaries() {
    for limits in [
        json!({"max_active_turns":0}),
        json!({"max_active_turns":1,"max_active_per_user":2}),
    ] {
        let mut input = config();
        input["limits"] = limits;
        assert!(serde_json::from_value::<HostConfig>(input)
            .unwrap()
            .validate()
            .is_err());
    }
    for provider in [
        json!({"id":"default","name":"Duplicate","api_base":"https://model.example.test/v1"}),
        json!({"id":"fixture","name":"Unsafe","api_base":"http://public.example.test/v1"}),
        json!({"id":"fixture","name":"Credentials","api_base":"https://user:password@model.example.test/v1"}),
    ] {
        let mut input = config();
        input["model_providers"] = json!([provider]);
        assert!(serde_json::from_value::<HostConfig>(input)
            .unwrap()
            .validate()
            .is_err());
    }
}
