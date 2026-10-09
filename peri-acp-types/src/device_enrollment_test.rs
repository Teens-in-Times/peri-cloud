use super::*;

fn authorization() -> NativeAuthorization {
    NativeAuthorization {
        client_id: NATIVE_CLIENT_ID.into(),
        device_id: Uuid::new_v4(),
        device_name: "电脑".into(),
        redirect_uri: "http://127.0.0.1:45678/oauth/callback".into(),
        code_challenge: URL_SAFE_NO_PAD.encode([1; 32]),
        code_challenge_method: "S256".into(),
        state: "fixture-random-state".into(),
    }
}

#[test]
fn test_enrollment_json_roundtrip_preserves_binding_and_rejects_unknown_fields() {
    let authorization = authorization();
    let mut value = serde_json::to_value(&authorization).unwrap();
    let decoded: NativeAuthorization = serde_json::from_value(value.clone()).unwrap();
    assert_eq!(decoded.device_id, authorization.device_id);
    assert_eq!(decoded.state, authorization.state);
    decoded.validate().unwrap();
    value["ssh_host"] = serde_json::json!("not-a-model-argument");
    assert!(serde_json::from_value::<NativeAuthorization>(value).is_err());
    let tokens = NativeTokens {
        access_token: "a".repeat(64),
        refresh_token: "b".repeat(64),
        token_type: "Bearer".into(),
        expires_in: 1800,
        principal_id: Uuid::new_v4(),
        device_id: authorization.device_id,
    };
    let decoded: NativeTokens =
        serde_json::from_slice(&serde_json::to_vec(&tokens).unwrap()).unwrap();
    assert_eq!(decoded.device_id, tokens.device_id);
    assert_eq!(decoded.principal_id, tokens.principal_id);
    assert_eq!(decoded.token_type, "Bearer");
}

#[test]
fn test_authorization_accepts_only_fixed_client_pkce_and_literal_loopback_callback() {
    for redirect in [
        "https://attacker.test/oauth/callback",
        "http://localhost:45678/oauth/callback",
        "http://127.0.0.1/oauth/callback",
        "http://127.0.0.1:45678/oauth/callback?next=bad",
    ] {
        let mut authorization = authorization();
        authorization.redirect_uri = redirect.into();
        assert!(
            authorization.validate().is_err(),
            "不接受非回环或带参数的回调"
        );
    }
    let mut authorization = authorization();
    authorization.code_challenge_method = "plain".into();
    assert!(authorization.validate().is_err());
    authorization.code_challenge_method = "S256".into();
    authorization.client_id = "different-client".into();
    assert!(authorization.validate().is_err());
}
