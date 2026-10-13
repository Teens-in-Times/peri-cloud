use super::*;

async fn browser_login(process: &Process, client: &Client) -> Account {
    let response = client
        .post(format!("{}/api/login", process.base))
        .header("Origin", &process.base)
        .json(&json!({"login":"owner","password":PASSWORD}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let cookies: Vec<_> = response
        .headers()
        .get_all("set-cookie")
        .iter()
        .map(|h| h.to_str().unwrap().split(';').next().unwrap().to_owned())
        .collect();
    Account {
        cookie: cookies.join("; "),
        csrf: cookies
            .iter()
            .find(|s| s.starts_with("peri_csrf="))
            .unwrap()
            .split_once('=')
            .unwrap()
            .1
            .into(),
        access: "nonempty-no-native-grant".into(),
        principal: response.json::<Value>().await.unwrap()["principal"]["id"]
            .as_str()
            .unwrap()
            .into(),
    }
}

async fn wait_menu(wire: &Wire, title: &str) -> Value {
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            if let Some(body) = wire
                .messages
                .lock()
                .iter()
                .rev()
                .find(|v| {
                    v["markdown"]["content"]
                        .as_str()
                        .is_some_and(|s| s.contains(title))
                })
                .cloned()
            {
                break body;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("menu reply missing")
}

#[tokio::test]
async fn local_instance_works_without_native_oauth_and_uses_same_approval_and_restart_flow() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("local workspace");
    let stop = CancellationToken::new();
    let wire = Arc::new(Wire::default());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let app = Router::new()
        .route(
            "/token",
            post(|| async { Json(json!({"access_token":"fixture-token","expires_in":3600})) }),
        )
        .route("/v2/users/{user}/messages", post(outgoing))
        .route("/interactions/{id}", put(acknowledge))
        .route("/v1/chat/completions", post(model))
        .with_state(wire.clone());
    let signal = stop.clone();
    let server = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(signal.cancelled_owned())
            .await
            .unwrap();
    });
    let client = Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap();
    let (path, base) = config(
        root.path(),
        &origin,
        json!([]),
        json!({"instance_id":"fixture-qq","app_id_env":"FIXTURE_QQ_APP","client_secret_env":"FIXTURE_QQ_SECRET",
        "transport":"webhook","relay":{"api":format!("{origin}/"),"token":format!("{origin}/token")}}),
    );
    let mut conf: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    conf["local_instance"] = json!({"name":"Cloud local","workspace":workspace});
    std::fs::write(&path, serde_json::to_vec(&conf).unwrap()).unwrap();
    let first = Process::launch(&path, base.clone(), &client).await;
    client.post(format!("{base}/api/setup")).header("Origin",&base).json(&json!({"bootstrap_token":SETUP,"login":"owner","password":PASSWORD,"display_name":"Owner"})).send().await.unwrap().error_for_status().unwrap();
    let account = browser_login(&first, &client).await;
    let data = first.get(&client, &account, "/api/account").await;
    assert_eq!(data["devices"].as_array().unwrap().len(), 1);
    assert_eq!(data["devices"][0]["is_local"], true);
    let device = data["devices"][0]["id"].as_str().unwrap().to_owned();
    // Invited users inherit neither server execution nor service administration.
    let invite = first
        .post(&client, &account, "/api/admin/invites", json!({}))
        .await;
    client.post(format!("{base}/api/register")).header("Origin",&base)
        .json(&json!({"invite_code":invite["code"],"login":"guest","password":PASSWORD,"display_name":"Guest"}))
        .send().await.unwrap().error_for_status().unwrap();
    let response = client
        .post(format!("{base}/api/login"))
        .header("Origin", &base)
        .json(&json!({"login":"guest","password":PASSWORD}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let cookies: Vec<_> = response
        .headers()
        .get_all("set-cookie")
        .iter()
        .map(|h| h.to_str().unwrap().split(';').next().unwrap().to_owned())
        .collect();
    let guest_cookie = cookies.join("; ");
    let guest_csrf = cookies
        .iter()
        .find(|s| s.starts_with("peri_csrf="))
        .unwrap()
        .split_once('=')
        .unwrap()
        .1;
    let guest_account: Value = client
        .get(format!("{base}/api/account"))
        .header("Cookie", &guest_cookie)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(guest_account["is_admin"], false);
    assert!(guest_account["devices"].as_array().unwrap().is_empty());
    for (path, body) in [
        (
            format!("/api/instances/{device}/browse"),
            json!({"path":null,"offset":0}),
        ),
        ("/api/service/shutdown".into(), json!({})),
        ("/api/admin/invites".into(), json!({})),
    ] {
        assert_eq!(
            client
                .post(format!("{base}{path}"))
                .header("Origin", &base)
                .header("Cookie", &guest_cookie)
                .header("X-Peri-CSRF", guest_csrf)
                .json(&body)
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::FORBIDDEN
        );
    }
    let memory:Value = client.post(format!("{base}/api/user-data")).header("Origin",&base).header("Cookie",&guest_cookie).header("X-Peri-CSRF",guest_csrf).json(&json!({"revision":0,"instructions":"GUEST_ONLY_PREFERENCE","memory":"GUEST_ONLY_MEMORY"})).send().await.unwrap().error_for_status().unwrap().json().await.unwrap();
    assert_eq!(memory["memory"], "GUEST_ONLY_MEMORY");
    // JSON escaping can exceed the ordinary 16 KiB request cap even when the
    // decoded personal context is within its 8 KiB account quota.
    let escaped = json!({"revision":1,"instructions":"","memory":"\n".repeat(8192)});
    assert!(serde_json::to_vec(&escaped).unwrap().len() > 16 * 1024);
    client
        .post(format!("{base}/api/user-data"))
        .header("Origin", &base)
        .header("Cookie", &guest_cookie)
        .header("X-Peri-CSRF", guest_csrf)
        .json(&escaped)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    assert_eq!(
        first.get(&client, &account, "/api/user-data").await["data"]["memory"],
        ""
    );
    let model_body =
        json!({"provider":"default","model":"guest-model","api_key":"GUEST_WRITE_ONLY_KEY"});
    client
        .post(format!("{base}/api/model"))
        .header("Origin", &base)
        .header("Cookie", &guest_cookie)
        .header("X-Peri-CSRF", guest_csrf)
        .json(&model_body)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let model_view = client
        .get(format!("{base}/api/model"))
        .header("Cookie", &guest_cookie)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(!model_view.contains("GUEST_WRITE_ONLY_KEY"));
    assert!(first.get(&client, &account, "/api/model").await["choice"].is_null());
    let original_default = data["devices"][0]["default_workspace"]
        .as_str()
        .unwrap()
        .to_owned();
    let browse_url = format!("{base}/api/instances/{device}/browse");
    let body = json!({"path":null,"offset":0});
    assert_eq!(
        client
            .post(&browse_url)
            .header("Origin", &base)
            .header("Cookie", &account.cookie)
            .json(&body)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );
    let listing = first
        .post(
            &client,
            &account,
            &format!("/api/instances/{device}/browse"),
            body,
        )
        .await;
    assert_eq!(listing["path"], original_default);
    assert!(!listing["roots"].as_array().unwrap().is_empty());
    let child = workspace.join("new project");
    std::fs::create_dir(&child).unwrap();
    let pair = first
        .post(&client, &account, "/api/pairing/code", json!({}))
        .await;
    first
        .message(
            &client,
            "pair",
            &format!("/绑定 {}", pair["code"].as_str().unwrap()),
        )
        .await;
    let claims = first.get(&client, &account, "/api/account").await;
    first
        .post(
            &client,
            &account,
            &format!(
                "/api/pairing/{}/confirm",
                claims["pending_pair_claims"][0]["claim_id"]
                    .as_str()
                    .unwrap()
            ),
            json!({"allow":true}),
        )
        .await;
    first.message(&client, "choose-menu", "实例").await;
    let menu = wait_menu(&wire, "选择执行实例").await;
    let action = menu["keyboard"]["content"]["rows"][0]["buttons"][0]["action"]["data"]
        .as_str()
        .unwrap();
    first.event(&client,"INTERACTION_CREATE",json!({"id":"choose-local","chat_type":2,"user_openid":"owner-openid","data":{"type":11,"resolved":{"button_data":action}}})).await;
    let selected = wait_menu(&wire, "实例与工作区已就绪").await;
    assert_eq!(selected["event_id"], "choose-local");
    assert!(selected.get("msg_id").is_none());
    first.message(&client, "local-write", "写文件").await;
    let card = wait_message(&wire, "local-write", true).await;
    let action = card["keyboard"]["content"]["rows"][0]["buttons"][0]["action"]["data"]
        .as_str()
        .unwrap();
    assert!(!workspace.join("approved.txt").exists());
    first.event(&client,"INTERACTION_CREATE",json!({"id":"local-approval","chat_type":2,"user_openid":"owner-openid","data":{"type":11,"resolved":{"button_data":action}}})).await;
    wait_message(&wire, "local-write", false).await;
    assert_eq!(
        std::fs::read_to_string(workspace.join("approved.txt")).unwrap(),
        "PRIVATE_TOOL_CONTENT"
    );
    let contexts = first.get(&client, &account, "/api/workspaces").await;
    assert_eq!(contexts[0]["instance_kind"], "local");
    let current_session = contexts[0]["session_id"].as_str().unwrap().to_owned();
    let saved = first
        .post(
            &client,
            &account,
            &format!("/api/instances/{device}/default-workspace"),
            json!({"workspace":child}),
        )
        .await;
    assert_eq!(
        first.get(&client, &account, "/api/workspaces").await[0]["workspace"],
        original_default
    );
    first.stop(&client, &account).await;
    let second = Process::launch(&path, base, &client).await;
    let account = browser_login(&second, &client).await;
    let data = second.get(&client, &account, "/api/account").await;
    assert_eq!(data["devices"][0]["id"], device);
    assert_eq!(data["devices"][0]["default_workspace"], saved["workspace"]);
    assert_eq!(
        second.get(&client, &account, "/api/workspaces").await[0]["session_id"],
        current_session
    );
    second
        .post(
            &client,
            &account,
            &format!("/api/workspaces/{current_session}/switch"),
            json!({"workspace":null}),
        )
        .await;
    assert_eq!(
        second.get(&client, &account, "/api/workspaces").await[0]["workspace"],
        saved["workspace"]
    );
    second.stop(&client, &account).await;
    stop.cancel();
    server.await.unwrap();
}
