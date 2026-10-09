use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio_tungstenite::tungstenite::Message;
use tokio_util::sync::CancellationToken;

use super::{QqAdapter, QqError, QqResult};
use crate::gateway::Gateway;

#[derive(Default)]
struct Resume {
    session: Option<String>,
    sequence: Option<u64>,
}

impl QqAdapter {
    /// Existing QQ bots can retain their WebSocket gateway. The deployment owns
    /// this future and must await it on shutdown, before draining Gateway.
    pub async fn run_websocket(
        self: Arc<Self>,
        gateway: Arc<Gateway>,
        stop: CancellationToken,
    ) -> QqResult<()> {
        let mut resume = Resume::default();
        let mut retry = Duration::from_secs(1);
        loop {
            if stop.is_cancelled() {
                return Ok(());
            }
            // Do not drop websocket_once: it owns the heartbeat JoinHandle.
            let result = self.websocket_once(&gateway, &stop, &mut resume).await;
            if stop.is_cancelled() {
                return Ok(());
            }
            if let Err(error) = result {
                tracing::warn!(
                    component = "qq_websocket",
                    error_kind = ?error,
                    "QQ connection ended; reconnecting"
                );
            }
            tokio::select! { _ = stop.cancelled() => return Ok(()), _ = tokio::time::sleep(retry) => {} }
            retry = (retry * 2).min(Duration::from_secs(30));
        }
    }

    async fn websocket_once(
        &self,
        gateway: &Arc<Gateway>,
        stop: &CancellationToken,
        resume: &mut Resume,
    ) -> QqResult<()> {
        let url = self.gateway_url().await?;
        let (socket, _) = tokio::time::timeout(
            Duration::from_secs(15),
            tokio_tungstenite::connect_async(url.as_str()),
        )
        .await
        .map_err(|_| QqError::Connection)?
        .map_err(|_| QqError::Connection)?;
        let (mut write, mut read) = socket.split();
        let hello = tokio::time::timeout(Duration::from_secs(15), read.next())
            .await
            .map_err(|_| QqError::Connection)?
            .ok_or(QqError::Connection)?
            .map_err(|_| QqError::Connection)?;
        let hello: Value = serde_json::from_str(hello.to_text().map_err(|_| QqError::Event)?)
            .map_err(|_| QqError::Event)?;
        if hello["op"].as_u64() != Some(10) {
            return Err(QqError::Event);
        }
        let interval = hello["d"]["heartbeat_interval"]
            .as_u64()
            .filter(|value| (100..=120000).contains(value))
            .ok_or(QqError::Event)?;
        let token = self.access_token().await?;
        tracing::info!(
            component = "qq_websocket",
            interval_ms = interval,
            "QQ heartbeat interval received"
        );
        let identify = match (&resume.session, resume.sequence) {
            (Some(session), Some(sequence)) => {
                json!({"op":6,"d":{"token":format!("QQBot {token}"),"session_id":session,"seq":sequence}})
            }
            _ => {
                json!({"op":2,"d":{"token":format!("QQBot {token}"),"intents":(1u64<<25)|(1u64<<26),"shard":[0,1],"properties":{}}})
            }
        };
        tokio::time::timeout(
            Duration::from_secs(5),
            write.send(Message::Text(identify.to_string().into())),
        )
        .await
        .map_err(|_| QqError::Connection)?
        .map_err(|_| QqError::Connection)?;

        let writer = Arc::new(tokio::sync::Mutex::new(write));
        let sequence = Arc::new(parking_lot::Mutex::new(resume.sequence));
        let acknowledged = Arc::new(AtomicBool::new(true));
        let connection_stop = stop.child_token();
        let failure = CancellationToken::new();
        let authenticated = CancellationToken::new();
        let heartbeat = {
            let writer = writer.clone();
            let sequence = sequence.clone();
            let acknowledged = acknowledged.clone();
            let stopped = connection_stop.clone();
            let failure = failure.clone();
            let authenticated = authenticated.clone();
            tokio::spawn(async move {
                // Tencent can discard a heartbeat sent before READY/RESUMED.
                // interval() ticks immediately, which previously left ACK=false
                // and disconnected a healthy session on its first real tick.
                tokio::select! {
                    biased;
                    _ = stopped.cancelled() => return,
                    _ = authenticated.cancelled() => (),
                }
                let period = Duration::from_millis(interval * 4 / 5);
                let mut interval =
                    tokio::time::interval_at(tokio::time::Instant::now() + period, period);
                interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                loop {
                    tokio::select! { biased; _ = stopped.cancelled() => break, _ = interval.tick() => {} }
                    if !acknowledged.swap(false, Ordering::SeqCst) {
                        tracing::warn!(
                            component = "qq_websocket",
                            "QQ heartbeat acknowledgement missing"
                        );
                        failure.cancel();
                        break;
                    }
                    let value = json!({"op":1,"d":*sequence.lock()});
                    let result = tokio::time::timeout(Duration::from_secs(5), async {
                        writer
                            .lock()
                            .await
                            .send(Message::Text(value.to_string().into()))
                            .await
                    })
                    .await;
                    if !matches!(result, Ok(Ok(()))) {
                        tracing::warn!(component = "qq_websocket", "QQ heartbeat send failed");
                        failure.cancel();
                        break;
                    }
                }
            })
        };
        let result = async {
            loop {
                let message = tokio::select! {
                    biased;
                    _ = stop.cancelled() => return Ok(()),
                    _ = failure.cancelled() => return Err(QqError::Connection),
                    message = read.next() => message.ok_or(QqError::Connection)?.map_err(|_| QqError::Connection)?,
                };
                match message {
                    Message::Ping(value) => { tokio::time::timeout(Duration::from_secs(5), writer.lock().await.send(Message::Pong(value))).await.map_err(|_| QqError::Connection)?.map_err(|_| QqError::Connection)?; continue; }
                    Message::Close(ref close) => {
                        tracing::warn!(component = "qq_websocket", code = close.as_ref().map(|frame| u16::from(frame.code)), "QQ socket closed by gateway");
                        return Err(QqError::Connection);
                    },
                    Message::Text(ref text) if text.len() <= 65536 => (),
                    Message::Text(_) => return Err(QqError::Event),
                    _ => continue,
                }
                let payload: Value = serde_json::from_str(message.to_text().map_err(|_| QqError::Event)?).map_err(|_| QqError::Event)?;
                tracing::debug!(component = "qq_websocket", opcode = payload["op"].as_u64(), "QQ control frame received");
                match payload["op"].as_u64() {
                    Some(11) => { acknowledged.store(true, Ordering::SeqCst); }
                    Some(7) => return Err(QqError::Connection),
                    Some(9) => { if payload["d"].as_bool() != Some(true) { *resume = Resume::default(); } return Err(QqError::Connection); }
                    Some(0) => {
                        let kind = payload["t"].as_str().ok_or(QqError::Event)?;
                        if kind == "READY" {
                            let session = payload["d"]["session_id"].as_str().filter(|value| !value.is_empty() && value.len() <= 256).ok_or(QqError::Event)?;
                            resume.session = Some(session.into());
                            authenticated.cancel();
                            tracing::info!(component="qq_websocket", "QQ gateway ready");
                        } else if kind == "RESUMED" {
                            authenticated.cancel();
                            tracing::info!(component="qq_websocket", "QQ gateway resumed");
                        } else {
                            tokio::select! {
                                biased;
                                _ = stop.cancelled() => return Ok(()),
                                _ = failure.cancelled() => return Err(QqError::Connection),
                                result = self.accept(gateway, kind, payload["d"].clone()) => result?,
                            }
                        }
                        // Resume only past events whose admission was acknowledged.
                        if let Some(value) = payload["s"].as_u64() { resume.sequence = Some(value); *sequence.lock() = Some(value); }
                    }
                    _ => (),
                }
            }
        }.await;
        connection_stop.cancel();
        heartbeat.await.map_err(|_| QqError::Connection)?;
        result
    }
}
