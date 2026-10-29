use super::*;

#[tokio::test]
async fn topology_and_read_only_connection_checks_are_authenticated_and_account_scoped() {
    let mut f = Fixture::new(Vec::new()).await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let app = peri_cloud::portal::router_with_gateway(
        f.c().identity.clone(),
        peri_cloud::portal::PortalConfig::new(&origin)
            .unwrap()
            .with_service_name(Some("Fixture control")),
        f.c().gateway.clone(),
    );
    let stop = f.stop.clone();
    f.servers.push(tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(stop.cancelled_owned())
            .await
            .unwrap();
    }));
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let owner = f
        .c()
        .identity
        .login("owner", PASSWORD.into())
        .await
        .unwrap();
    let cookie = format!(
        "peri_session={}; peri_csrf={}",
        owner.session_token, owner.csrf_token
    );
    let url = format!("{origin}/api/account");
    assert_eq!(client.get(&url).send().await.unwrap().status(), 401);
    let data: Value = client
        .get(&url)
        .header("Cookie", &cookie)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(data["service"]["name"], "Fixture control");
    assert_eq!(data["service"]["origin"], origin);
    assert_eq!(data["gateways"][0]["adapter"], "fixture-qq");
    assert_eq!(data["gateways"][0]["linked_identities"], 0);
    assert!(!data.to_string().contains("fixture-model-key"));

    let invite = f.c().identity.create_invite(&f.c().browser).await.unwrap();
    f.c()
        .identity
        .register_invited(&invite.code, "guest", PASSWORD.into(), "Guest")
        .await
        .unwrap();
    let guest = f
        .c()
        .identity
        .login("guest", PASSWORD.into())
        .await
        .unwrap();
    let guest_cookie = format!(
        "peri_session={}; peri_csrf={}",
        guest.session_token, guest.csrf_token
    );
    let data: Value = client
        .get(&url)
        .header("Cookie", &guest_cookie)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(data["devices"], json!([]));
    assert_eq!(data["device_management"], json!({}));
    assert_eq!(
        data["gateways"],
        json!([]),
        "unlinked members must not enumerate configured gateways"
    );
    for endpoint in ["connection-check", "inspect", "bookmarks"] {
        let check = format!("{origin}/api/instances/{}/{endpoint}", f.devices[0].id);
        let payload = json!({"action":"save", "workspace":f.workspaces[0].path().to_string_lossy(), "label":"project"});
        for (cookies, csrf, source, expected) in [
            ("", "", origin.as_str(), 401),
            (cookie.as_str(), "", origin.as_str(), 401),
            (
                cookie.as_str(),
                owner.csrf_token.as_str(),
                "https://untrusted.example",
                403,
            ),
            (
                guest_cookie.as_str(),
                guest.csrf_token.as_str(),
                origin.as_str(),
                403,
            ),
            (
                cookie.as_str(),
                owner.csrf_token.as_str(),
                origin.as_str(),
                200,
            ),
        ] {
            let response = client
                .post(&check)
                .header("Cookie", cookies)
                .header("X-Peri-CSRF", csrf)
                .header("Origin", source)
                .json(&payload)
                .send()
                .await
                .unwrap();
            assert_eq!(response.status().as_u16(), expected);
            if expected == 200 {
                let body: Value = response.json().await.unwrap();
                match endpoint {
                    "connection-check" => assert_eq!(body["transport"], "loopback"),
                    "inspect" => assert_eq!(body["check"]["authenticated"], true),
                    _ => {
                        assert_eq!(body["bookmarks"].as_array().unwrap().len(), 1);
                        let bookmark = &body["bookmarks"][0];
                        let renamed = client.post(&check).header("Cookie", &cookie).header("X-Peri-CSRF", &owner.csrf_token).header("Origin", &origin)
                            .json(&json!({"action":"rename","id":bookmark["id"],"revision":bookmark["revision"],"label":"new name"})).send().await.unwrap();
                        assert_eq!(renamed.status(), 200);
                        let stale = client.post(&check).header("Cookie", &cookie).header("X-Peri-CSRF", &owner.csrf_token).header("Origin", &origin)
                            .json(&json!({"action":"remove","id":bookmark["id"],"revision":bookmark["revision"]})).send().await.unwrap();
                        assert_eq!(stale.status(), 409);
                        assert_eq!(
                            stale.json::<Value>().await.unwrap()["error"],
                            "bookmark_conflict"
                        );
                    }
                }
            }
        }
    }
    f.c()
        .identity
        .revoke_device(&f.c().browser, f.devices[0].id)
        .await
        .unwrap();
    for endpoint in ["connection-check", "inspect", "bookmarks"] {
        let check = format!("{origin}/api/instances/{}/{endpoint}", f.devices[0].id);
        assert_eq!(
            client
                .post(&check)
                .header("Cookie", &cookie)
                .header("X-Peri-CSRF", &owner.csrf_token)
                .header("Origin", &origin)
                .json(&json!({"action":"save","workspace":"/revoked","label":"no"}))
                .send()
                .await
                .unwrap()
                .status(),
            403
        );
        let unknown = format!("{origin}/api/instances/{}/{endpoint}", Uuid::new_v4());
        assert_eq!(
            client
                .post(&unknown)
                .header("Cookie", &cookie)
                .header("X-Peri-CSRF", &owner.csrf_token)
                .header("Origin", &origin)
                .json(&json!({"action":"save","workspace":"/unknown","label":"no"}))
                .send()
                .await
                .unwrap()
                .status(),
            403
        );
    }
    let pool = sqlx::SqlitePool::connect_with(
        sqlx::sqlite::SqliteConnectOptions::new()
            .filename(f.root.path().join("cloud.sqlite"))
            .read_only(true),
    )
    .await
    .unwrap();
    let (sessions,): (i64,) = sqlx::query_as("SELECT count(*) FROM cloud_sessions")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        sessions, 0,
        "connection checks must not create sessions or run jobs"
    );
    pool.close().await;
    assert!(f.script.requests.lock().is_empty());
    f.finish().await;
}
