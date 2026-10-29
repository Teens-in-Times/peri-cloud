use peri_acp_types::device_executor::SubmitJob;
use peri_executor::{web, Executor};
use peri_remote_tools::{DeviceClient, Error, ExecutorEndpoint, WireGuardEndpoint};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

#[tokio::test]
async fn capability_inspection_rejects_changed_identity_and_skips_legacy_endpoints() {
    use peri_acp_types::device_executor::{
        ExecutorCapabilities, ExecutorInfo, DEVICE_CAPABILITIES_FEATURE, PROTOCOL_VERSION,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};
    for advertised in [false, true] {
        let device = Uuid::new_v4();
        let info = ExecutorInfo {
            protocol_version: PROTOCOL_VERSION,
            device_id: device,
            device_name: "fixture".into(),
            platform: "linux".into(),
            version: "fixture".into(),
            features: if advertised {
                vec![DEVICE_CAPABILITIES_FEATURE.into()]
            } else {
                vec![]
            },
        };
        let changed = ExecutorCapabilities {
            info: ExecutorInfo {
                device_id: Uuid::new_v4(),
                ..info.clone()
            },
            tools: vec![],
        };
        let hits = Arc::new(AtomicUsize::new(0));
        let counter = hits.clone();
        let app = axum::Router::new()
            .route(
                "/v1/info",
                axum::routing::get(move || {
                    let info = info.clone();
                    async move { axum::Json(info) }
                }),
            )
            .route(
                "/v1/capabilities",
                axum::routing::get(move || {
                    let changed = changed.clone();
                    let counter = counter.clone();
                    async move {
                        counter.fetch_add(1, Ordering::SeqCst);
                        axum::Json(changed)
                    }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let stop = CancellationToken::new();
        let shutdown = stop.clone();
        let task = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(shutdown.cancelled_owned())
                .await
                .unwrap();
        });
        let client = DeviceClient::connect(address, "fixture-token".into(), device)
            .await
            .unwrap();
        if advertised {
            assert!(matches!(
                client.capabilities().await,
                Err(Error::DeviceMismatch)
            ));
            assert_eq!(hits.load(Ordering::SeqCst), 1);
        } else {
            assert!(client.capabilities().await.unwrap().is_none());
            assert_eq!(hits.load(Ordering::SeqCst), 0);
        }
        stop.cancel();
        task.await.unwrap();
    }
}

async fn server(
    executor: Arc<Executor>,
) -> (SocketAddr, CancellationToken, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let stop = CancellationToken::new();
    let signal = stop.clone();
    let task = tokio::spawn(async move {
        axum::serve(listener, web::router(executor))
            .with_graceful_shutdown(signal.cancelled_owned())
            .await
            .unwrap();
    });
    (address, stop, task)
}

#[tokio::test]
async fn unavailable_wireguard_falls_back_but_authentication_never_downgrades() {
    let root = tempfile::tempdir().unwrap();
    let executor = Arc::new(Executor::open(root.path(), "routes").await.unwrap());
    let token = std::fs::read_to_string(root.path().join("transport-token")).unwrap();
    let (address, stop, task) = server(executor.clone()).await;
    let routes = vec![
        ExecutorEndpoint::WireGuard(WireGuardEndpoint {
            interface: "peri-missing".into(),
            endpoint: "10.77.0.3:42371".parse().unwrap(),
        }),
        ExecutorEndpoint::Loopback(address),
    ];
    let client = DeviceClient::connect_routes(routes, token.clone(), executor.info().device_id)
        .await
        .unwrap();
    assert_eq!(client.info().device_id, executor.info().device_id);
    let wrong_identity = DeviceClient::connect_routes(
        vec![
            ExecutorEndpoint::Loopback(address),
            ExecutorEndpoint::Loopback(address),
        ],
        token,
        Uuid::new_v4(),
    )
    .await;
    assert!(matches!(wrong_identity, Err(Error::DeviceMismatch)));
    let wrong_token = DeviceClient::connect_routes(
        vec![
            ExecutorEndpoint::Loopback(address),
            ExecutorEndpoint::Loopback(address),
        ],
        "wrong".into(),
        executor.info().device_id,
    )
    .await;
    assert!(matches!(wrong_token, Err(Error::Rejected(401))));
    stop.cancel();
    task.await.unwrap();
    executor.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_read_recovers_on_the_second_path_with_the_same_session() {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let executor = Arc::new(Executor::open(root.path(), "routes").await.unwrap());
    let token = std::fs::read_to_string(root.path().join("transport-token")).unwrap();
    let (first, first_stop, first_task) = server(executor.clone()).await;
    let (second, second_stop, second_task) = server(executor.clone()).await;
    let client = DeviceClient::connect_routes(
        vec![
            ExecutorEndpoint::Loopback(first),
            ExecutorEndpoint::Loopback(second),
        ],
        token,
        executor.info().device_id,
    )
    .await
    .unwrap();
    let id = Uuid::new_v4();
    let binding = client
        .bind(id, workspace.path().to_str().unwrap())
        .await
        .unwrap();
    first_stop.cancel();
    first_task.await.unwrap();
    assert!(!client.catalog(id).await.unwrap().is_empty());
    assert_eq!(
        client
            .bind(id, workspace.path().to_str().unwrap())
            .await
            .unwrap(),
        binding
    );
    second_stop.cancel();
    second_task.await.unwrap();
    executor.shutdown().await.unwrap();
}

async fn read_request(socket: &mut tokio::net::TcpStream) -> (String, Vec<u8>) {
    let mut data = Vec::new();
    let (header, offset, length) = loop {
        let mut chunk = [0; 4096];
        let count = socket.read(&mut chunk).await.unwrap();
        assert!(count > 0);
        data.extend_from_slice(&chunk[..count]);
        if let Some(offset) = data.windows(4).position(|w| w == b"\r\n\r\n") {
            let header = String::from_utf8(data[..offset].to_vec()).unwrap();
            let length = header
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().unwrap())
                })
                .unwrap_or(0);
            break (header, offset + 4, length);
        }
    };
    while data.len() < offset + length {
        let mut chunk = [0; 4096];
        let count = socket.read(&mut chunk).await.unwrap();
        assert!(count > 0);
        data.extend_from_slice(&chunk[..count]);
    }
    (header, data[offset..offset + length].to_vec())
}

#[tokio::test]
async fn lost_submit_response_is_not_replayed_and_original_job_can_be_queried() {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let executor = Arc::new(Executor::open(root.path(), "lost-response").await.unwrap());
    let token = std::fs::read_to_string(root.path().join("transport-token")).unwrap();
    let (fallback, stop, task) = server(executor.clone()).await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let primary = listener.local_addr().unwrap();
    let info = serde_json::to_vec(&executor.info()).unwrap();
    let backend_token = token.clone();
    let relay = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let (header, _) = read_request(&mut socket).await;
        assert!(header.starts_with("GET /v1/info "));
        let header = format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", info.len());
        socket.write_all(header.as_bytes()).await.unwrap();
        socket.write_all(&info).await.unwrap();
        drop(socket);
        let (mut socket, _) = listener.accept().await.unwrap();
        let (header, body) = read_request(&mut socket).await;
        assert!(header.starts_with("POST /v1/jobs "));
        reqwest::Client::new()
            .post(format!("http://{fallback}/v1/jobs"))
            .bearer_auth(backend_token)
            .header("Content-Type", "application/json")
            .body(body)
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap();
        // The backend accepted the write, but the caller receives no response.
        drop(socket);
        drop(listener);
    });
    let session = Uuid::new_v4();
    executor
        .bind_session(
            session,
            peri_acp_types::device_executor::OpenSession {
                workspace: workspace.path().to_str().unwrap().into(),
            },
        )
        .await
        .unwrap();
    let client = DeviceClient::connect_routes(
        vec![
            ExecutorEndpoint::Loopback(primary),
            ExecutorEndpoint::Loopback(fallback),
        ],
        token,
        executor.info().device_id,
    )
    .await
    .unwrap();
    let invocation = Uuid::new_v4();
    let request = SubmitJob {
        invocation_id: invocation,
        session_id: session,
        tool: "Write".into(),
        input: serde_json::json!({"file_path":"once.txt","content":"once","append":true}),
    };
    assert!(matches!(
        client.submit(&request).await,
        Err(Error::Disconnected)
    ));
    relay.await.unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            if client
                .job(session, invocation)
                .await
                .unwrap()
                .status
                .is_terminal()
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(client.jobs(session).await.unwrap().len(), 1);
    assert_eq!(
        std::fs::read_to_string(workspace.path().join("once.txt")).unwrap(),
        "once"
    );
    stop.cancel();
    task.await.unwrap();
    executor.shutdown().await.unwrap();
}
