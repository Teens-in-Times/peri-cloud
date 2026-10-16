use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tokio_tungstenite::tungstenite::{
    self,
    handshake::server::{Request, Response},
    Message,
};
use tokio_tungstenite::{accept_hdr_async_with_config, WebSocketStream};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::{ApiRequest, Connection, OneBotAdapter, OneBotError};
use crate::gateway::Gateway;

impl OneBotAdapter {
    /// Deployment owns every socket and admission worker until stop. Only
    /// loopback listeners are accepted; remote bridges use SSH or a WSS proxy.
    pub async fn run(
        self: Arc<Self>,
        listener: TcpListener,
        gateway: Arc<Gateway>,
        stop: CancellationToken,
    ) -> Result<(), OneBotError> {
        if !listener
            .local_addr()
            .map_err(|_| OneBotError::Worker)?
            .ip()
            .is_loopback()
        {
            return Err(OneBotError::Configuration);
        }
        let mut sockets = JoinSet::new();
        let connections_stop = stop.child_token();
        let mut failed = false;
        loop {
            tokio::select! {
                biased;
                _ = stop.cancelled() => break,
                Some(result) = sockets.join_next(), if !sockets.is_empty() => {
                    if result.is_err() { failed = true; break; }
                }
                accepted = listener.accept() => {
                    let (stream, _) = match accepted { Ok(value) => value, Err(_) => { failed = true; break; } };
                    if sockets.len() >= 16 { drop(stream); continue; }
                    let adapter = self.clone();
                    let gateway = gateway.clone();
                    let stop = connections_stop.child_token();
                    sockets.spawn(async move { adapter.accept(stream, gateway, stop).await; });
                }
            }
        }
        // Stop is also signalled for listener failure, without cancelling the
        // deployment token owned by the host. No spawned socket is detached.
        connections_stop.cancel();
        while sockets.join_next().await.is_some() {}
        self.connection.lock().take();
        if failed {
            Err(OneBotError::Worker)
        } else {
            Ok(())
        }
    }

    // tungstenite's handshake callback requires an unboxed HTTP error response.
    #[allow(clippy::result_large_err)]
    async fn accept(
        self: Arc<Self>,
        stream: TcpStream,
        gateway: Arc<Gateway>,
        stop: CancellationToken,
    ) {
        let adapter = self.clone();
        let mut config = tungstenite::protocol::WebSocketConfig::default();
        config.max_message_size = Some(65536);
        config.max_frame_size = Some(65536);
        let handshake = accept_hdr_async_with_config(
            stream,
            move |request: &Request, response: Response| {
                let headers = request.headers();
                let unambiguous = ["authorization", "x-self-id", "x-client-role"]
                    .iter()
                    .all(|name| headers.get_all(*name).iter().count() == 1);
                let authorized = unambiguous
                    && headers
                        .get("authorization")
                        .and_then(|v| v.to_str().ok())
                        .and_then(|v| v.strip_prefix("Bearer "))
                        .is_some_and(|token| same_secret(token, &adapter.token));
                let code =
                    if request.uri().path() != "/onebot/v11" || request.uri().query().is_some() {
                        404
                    } else if !authorized
                        || headers.get("x-self-id").and_then(|v| v.to_str().ok())
                            != Some(&adapter.self_id)
                    {
                        401
                    } else if headers.get("x-client-role").and_then(|v| v.to_str().ok())
                        != Some("Universal")
                    {
                        400
                    } else if adapter.connection.lock().is_some() {
                        409
                    } else {
                        return Ok(response);
                    };
                Err(tungstenite::http::Response::builder()
                    .status(code)
                    .body(Some("OneBot connection rejected".into()))
                    .unwrap())
            },
            Some(config),
        );
        let socket = tokio::select! {
            biased;
            _ = stop.cancelled() => return,
            result = tokio::time::timeout(Duration::from_secs(10), handshake) => match result { Ok(Ok(socket)) => socket, _ => return },
        };
        let (requests, receiver) = mpsc::channel(32);
        let id = Uuid::new_v4();
        {
            let mut connection = self.connection.lock();
            if connection.is_some() {
                return;
            }
            *connection = Some(Connection { id, requests });
        }
        // A guard also clears the connection if the deployment aborts this
        // future. The UUID prevents an old owner clearing a newer connection.
        let _guard = ConnectionGuard {
            adapter: self.clone(),
            id,
        };
        tracing::info!(
            component = "onebot",
            "OneBot Universal connection authenticated"
        );
        self.socket(socket, receiver, gateway, stop).await;
    }

    async fn socket(
        self: &Arc<Self>,
        mut socket: WebSocketStream<TcpStream>,
        mut requests: mpsc::Receiver<ApiRequest>,
        gateway: Arc<Gateway>,
        stop: CancellationToken,
    ) {
        let mut pending: HashMap<String, tokio::sync::oneshot::Sender<Value>> = HashMap::new();
        let mut admissions = JoinSet::new();
        let (events, mut incoming) = mpsc::channel(64);
        let adapter = self.clone();
        admissions.spawn(async move {
            // Preserve socket order for successive selections and answers,
            // while the socket itself continues reading API responses.
            while let Some(value) = incoming.recv().await {
                adapter.clone().inbound(gateway.clone(), value).await;
            }
        });
        let mut tick = tokio::time::interval(Duration::from_secs(20));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut last_read = tokio::time::Instant::now();
        loop {
            tokio::select! {
                biased;
                _ = stop.cancelled() => break,
                Some(result) = admissions.join_next(), if !admissions.is_empty() => {
                    let _ = result;
                    break;
                }
                _ = tick.tick() => {
                    pending.retain(|_, result| !result.is_closed());
                    if last_read.elapsed() > Duration::from_secs(60) { break; }
                    if !send(&mut socket, Message::Ping(Vec::new().into())).await { break; }
                }
                message = socket.next() => {
                    last_read = tokio::time::Instant::now();
                    let value: Value = match message {
                        Some(Ok(Message::Text(text))) => match serde_json::from_str(&text) { Ok(value) => value, Err(_) => break },
                        Some(Ok(Message::Ping(bytes))) => { if !send(&mut socket, Message::Pong(bytes)).await { break; } continue; },
                        Some(Ok(Message::Pong(_))) => continue,
                        _ => break,
                    };
                    if let Some(echo) = value["echo"].as_str() {
                        if let Some(result) = pending.remove(echo) { let _ = result.send(value); }
                    } else if value["post_type"] == "message" && events.try_send(value).is_err() {
                        tracing::warn!(component="onebot", "OneBot input queue is full; closing connection");
                        break;
                    }
                }
                Some(request) = requests.recv(), if pending.len() < 32 => {
                    if request.response.is_closed() { continue; }
                    let echo = Uuid::new_v4().to_string();
                    let value = json!({"action":request.action,"params":request.params,"echo":echo});
                    pending.insert(echo, request.response);
                    if !send(&mut socket, Message::Text(value.to_string().into())).await { break; }
                }
            }
        }
        // Closing the queue and dropping pending responses wakes all waiters
        // with an unknown outcome. Never carry requests to a new socket.
        requests.close();
        drop(pending);
        drop(requests);
        drop(events);
        while admissions.join_next().await.is_some() {}
        let _ = tokio::time::timeout(Duration::from_secs(1), socket.close(None)).await;
    }
}

struct ConnectionGuard {
    adapter: Arc<OneBotAdapter>,
    id: Uuid,
}
impl Drop for ConnectionGuard {
    fn drop(&mut self) {
        let mut connection = self.adapter.connection.lock();
        if connection.as_ref().is_some_and(|c| c.id == self.id) {
            connection.take();
        }
    }
}

fn same_secret(left: &str, right: &str) -> bool {
    let left = Sha256::digest(left.as_bytes());
    let right = Sha256::digest(right.as_bytes());
    left.iter()
        .zip(right)
        .fold(0u8, |difference, (a, b)| difference | (a ^ b))
        == 0
}

async fn send(socket: &mut WebSocketStream<TcpStream>, message: Message) -> bool {
    matches!(
        tokio::time::timeout(Duration::from_secs(5), socket.send(message)).await,
        Ok(Ok(()))
    )
}
