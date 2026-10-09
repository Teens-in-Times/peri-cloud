use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use axum::extract::{RawQuery, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use subtle::ConstantTimeEq;
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use super::{LoginError, Result};

struct CallbackState {
    authority: String,
    state: String,
    consumed: AtomicBool,
    sender: mpsc::Sender<Result<String>>,
}

pub(super) struct Callback {
    pub redirect_uri: String,
    receiver: mpsc::Receiver<Result<String>>,
    stop: CancellationToken,
    server: JoinHandle<()>,
}

impl Callback {
    pub async fn start(state: String) -> Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .map_err(|_| LoginError::Callback)?;
        let address = listener.local_addr().map_err(|_| LoginError::Callback)?;
        let (sender, receiver) = mpsc::channel(1);
        let shared = Arc::new(CallbackState {
            authority: address.to_string(),
            state,
            consumed: AtomicBool::new(false),
            sender,
        });
        let router = Router::new()
            .route("/oauth/callback", get(receive))
            .with_state(shared);
        let stop = CancellationToken::new();
        let stopped = stop.clone();
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, router)
                .with_graceful_shutdown(stopped.cancelled_owned())
                .await;
        });
        Ok(Self {
            redirect_uri: format!("http://{address}/oauth/callback"),
            receiver,
            stop,
            server,
        })
    }

    pub async fn code(&mut self, timeout: std::time::Duration) -> Result<String> {
        let result = tokio::time::timeout(timeout, self.receiver.recv())
            .await
            .map_err(|_| LoginError::Cancelled)?;
        result.ok_or(LoginError::Callback)?
    }
}

impl Drop for Callback {
    fn drop(&mut self) {
        self.stop.cancel();
        // Do not leave a listening callback behind if its owner is dropped.
        self.server.abort();
    }
}

async fn receive(
    State(callback): State<Arc<CallbackState>>,
    headers: HeaderMap,
    RawQuery(query): RawQuery,
) -> Response {
    let result = parse(&callback, &headers, query.as_deref());
    let Ok(result) = result else {
        return page(StatusCode::BAD_REQUEST, "授权回调无效，请回到原登录页面。");
    };
    if callback.consumed.swap(true, Ordering::SeqCst) {
        return page(StatusCode::CONFLICT, "授权回调已收到。");
    }
    let declined = result.is_err();
    if callback.sender.try_send(result).is_err() {
        return page(StatusCode::GONE, "本次登录已结束，请重新开始。");
    }
    page(
        StatusCode::OK,
        if declined {
            "已取消授权。可以关闭此页面。"
        } else {
            "已收到授权，请返回执行器查看连接结果。"
        },
    )
}

fn parse(
    callback: &CallbackState,
    headers: &HeaderMap,
    query: Option<&str>,
) -> Result<Result<String>> {
    if headers.get_all(header::HOST).iter().count() != 1
        || headers
            .get(header::HOST)
            .and_then(|value| value.to_str().ok())
            != Some(&callback.authority)
    {
        return Err(LoginError::Protocol);
    }
    let query = query
        .filter(|value| value.len() <= 2048)
        .ok_or(LoginError::Protocol)?;
    let mut fields = HashMap::new();
    for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
        if fields
            .insert(key.into_owned(), value.into_owned())
            .is_some()
        {
            return Err(LoginError::Protocol);
        }
    }
    let state = fields.remove("state").ok_or(LoginError::Protocol)?;
    if !bool::from(state.as_bytes().ct_eq(callback.state.as_bytes())) {
        return Err(LoginError::Protocol);
    }
    let code = fields.remove("code");
    let error = fields.remove("error");
    if !fields.is_empty() {
        return Err(LoginError::Protocol);
    }
    match (code, error) {
        (Some(code), None)
            if code.len() == 64 && code.bytes().all(|byte| byte.is_ascii_hexdigit()) =>
        {
            Ok(Ok(code))
        }
        (None, Some(error)) if error == "access_denied" => Ok(Err(LoginError::Declined)),
        _ => Err(LoginError::Protocol),
    }
}

fn page(status: StatusCode, message: &'static str) -> Response {
    let mut response = (status, Html(format!("<!doctype html><html lang=\"zh-CN\"><meta charset=\"utf-8\"><title>Peri 登录</title><p>{message}</p></html>"))).into_response();
    let headers = response.headers_mut();
    headers.insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        "default-src 'none'; frame-ancestors 'none'"
            .parse()
            .unwrap(),
    );
    headers.insert(header::REFERRER_POLICY, "no-referrer".parse().unwrap());
    response
}

#[cfg(test)]
#[path = "callback_test.rs"]
mod tests;
