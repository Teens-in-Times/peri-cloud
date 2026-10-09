use std::time::Duration;

use futures::StreamExt;
use peri_model::{
    Model, ModelMessage, ModelRequest, ModelRuntimeConfig, ModelStreamEvent, OpenAiConfig,
    OpenAiModel, RetryConfig,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn native_client_identifies_itself_to_a_gateway_that_rejects_anonymous_requests() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut bytes = Vec::new();
        let mut buffer = [0; 2048];
        let headers_end = loop {
            let count = stream.read(&mut buffer).await.unwrap();
            assert_ne!(count, 0, "request closed before its headers");
            bytes.extend_from_slice(&buffer[..count]);
            if let Some(index) = bytes.windows(4).position(|value| value == b"\r\n\r\n") {
                break index + 4;
            }
            assert!(bytes.len() <= 16384);
        };
        let headers = String::from_utf8(bytes[..headers_end].to_vec()).unwrap();
        let user_agent = headers.lines().find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("user-agent")
                .then(|| value.trim().to_owned())
        });
        let content_length = headers
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().unwrap())
            })
            .unwrap();
        while bytes.len() < headers_end + content_length {
            let count = stream.read(&mut buffer).await.unwrap();
            assert_ne!(count, 0, "request closed before its body");
            bytes.extend_from_slice(&buffer[..count]);
        }
        let accepted = user_agent
            .as_deref()
            .is_some_and(|value| value.starts_with("Peri/"));
        let (status, content_type, body) = if accepted {
            ("200 OK", "text/event-stream", "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n")
        } else {
            (
                "403 Forbidden",
                "application/json",
                "{\"error\":{\"message\":\"client identity required\"}}",
            )
        };
        let response = format!(
            "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(response.as_bytes()).await.unwrap();
        stream.shutdown().await.unwrap();
        user_agent
    });
    let config = OpenAiConfig::new(
        format!("http://{address}/v1").parse().unwrap(),
        "fixture-secret",
        "fixture-model",
    )
    .with_runtime(
        ModelRuntimeConfig::default().with_retry(RetryConfig::default().with_max_attempts(1)),
    );
    let model = OpenAiModel::new(config);
    let events = tokio::time::timeout(Duration::from_secs(5), async {
        model
            .stream(
                ModelRequest::new(vec![ModelMessage::user_text("go")]),
                CancellationToken::new(),
            )
            .await
            .unwrap()
            .collect::<Vec<_>>()
            .await
    })
    .await
    .expect("gateway response deadline");
    assert!(events.iter().all(Result::is_ok), "{events:?}");
    assert!(events
        .iter()
        .any(|event| matches!(event, Ok(ModelStreamEvent::Completed(_)))));
    assert_eq!(
        server.await.unwrap().as_deref(),
        Some(concat!("Peri/", env!("CARGO_PKG_VERSION")))
    );
}
