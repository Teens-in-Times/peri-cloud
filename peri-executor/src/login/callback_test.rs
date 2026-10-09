use std::time::Duration;

use super::*;

#[tokio::test]
async fn test_callback_forged_state_and_duplicate_queries_do_not_consume_login() {
    let state = "fixture-random-state-long-enough";
    let mut callback = Callback::start(state.into()).await.unwrap();
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    for query in [
        format!("state=wrong&code={}", "a".repeat(64)),
        format!("state={state}&state={state}&code={}", "a".repeat(64)),
    ] {
        let response = client
            .get(format!("{}?{query}", callback.redirect_uri))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }
    let wrong_host = client
        .get(format!(
            "{}?state={state}&code={}",
            callback.redirect_uri,
            "a".repeat(64)
        ))
        .header("Host", "attacker.example.test")
        .send()
        .await
        .unwrap();
    assert_eq!(wrong_host.status(), StatusCode::BAD_REQUEST);
    let valid = client
        .get(format!(
            "{}?state={state}&code={}",
            callback.redirect_uri,
            "a".repeat(64)
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(valid.status(), StatusCode::OK);
    assert_eq!(
        callback.code(Duration::from_secs(1)).await.unwrap(),
        "a".repeat(64)
    );
    let duplicate = client
        .get(format!(
            "{}?state={state}&code={}",
            callback.redirect_uri,
            "a".repeat(64)
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(duplicate.status(), StatusCode::CONFLICT);
}

#[tokio::test]
async fn test_callback_decline_is_terminal_and_never_claims_connected() {
    let mut callback = Callback::start("fixture-state-for-declined-login".into())
        .await
        .unwrap();
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let response = client
        .get(format!(
            "{}?state=fixture-state-for-declined-login&error=access_denied",
            callback.redirect_uri
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(response.text().await.unwrap().contains("已取消授权"));
    assert!(matches!(
        callback.code(Duration::from_secs(1)).await,
        Err(LoginError::Declined)
    ));
}

#[tokio::test]
async fn test_callback_timeout_owner_drop_closes_listener() {
    let mut callback = Callback::start("fixture-state-for-timeout-login".into())
        .await
        .unwrap();
    let address = url::Url::parse(&callback.redirect_uri)
        .unwrap()
        .socket_addrs(|| None)
        .unwrap()[0];
    assert!(matches!(
        callback.code(Duration::ZERO).await,
        Err(LoginError::Cancelled)
    ));
    let server = callback.server.abort_handle();
    drop(callback);
    tokio::task::yield_now().await;
    let listener = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let Ok(listener) = TcpListener::bind(address).await {
                break listener;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(server.is_finished());
    drop(listener);
}
