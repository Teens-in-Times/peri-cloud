//! Authenticated HTTP adapter on loopback/SSH or an explicitly verified WireGuard address.

use std::collections::{HashMap, VecDeque};
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::{ConnectInfo, DefaultBodyLimit, Path, Request, State};
use axum::http::{uri::Authority, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use serde_json::json;
use uuid::Uuid;

use crate::{Error, Executor};
use peri_acp_types::device_executor::*;

/// 请求入口防护（executor review 2026-10-26 P2-1）。
///
/// SSH 反向转发把端口绑到 VPS loopback 后，VPS 上任何本地进程都能打到本服务；
/// 纯 Bearer 虽无 cookie/CSRF 之忧，但仍拒绝 DNS-rebinding（Host 伪装）与
/// 跨源浏览器页（Origin 校验），并对每个来源 IP 做滑动窗口限速。
#[derive(Clone)]
struct Guard {
    /// 允许的 Host（不含端口，小写）。来自监听地址：loopback 监听允许
    /// localhost/127.0.0.1/[::1]，WireGuard 监听允许该接口地址。
    allowed_hosts: Vec<String>,
    /// 每来源 IP 的请求时间戳（滑动窗口）。
    seen: Arc<Mutex<HashMap<IpAddr, VecDeque<Instant>>>>,
}

/// 窗口参数：10 秒内每 IP 至多 600 个请求。协议自身的等待轮询是 100ms/次
/// （≈100 req/10s/会话），上限需容纳多会话并行轮询，同时拦住失控重试循环。
const RATE_WINDOW: Duration = Duration::from_secs(10);
const RATE_MAX_REQUESTS: usize = 600;
const MAX_TRACKED_IPS: usize = 1024;

impl Guard {
    fn new(listen: SocketAddr) -> Self {
        let mut allowed_hosts = vec![
            "localhost".to_string(),
            "127.0.0.1".to_string(),
            "[::1]".to_string(),
        ];
        let host = if listen.is_ipv6() {
            format!("[{}]", listen.ip())
        } else {
            listen.ip().to_string()
        };
        if !allowed_hosts.contains(&host) {
            allowed_hosts.push(host);
        }
        Self {
            allowed_hosts,
            seen: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    fn host_allowed(&self, authority: &Authority) -> bool {
        // A forwarded SSH port can differ from the listen port.
        self.allowed_hosts
            .iter()
            .any(|allowed| allowed.eq_ignore_ascii_case(authority.host()))
    }

    fn origin_allowed(origin: &str, authority: &Authority) -> bool {
        let Ok(url) = url::Url::parse(origin) else {
            return false;
        };
        url.scheme() == "http"
            && url.username().is_empty()
            && url.password().is_none()
            && url.path() == "/"
            && url.query().is_none()
            && url.fragment().is_none()
            && url
                .host_str()
                .is_some_and(|host| authority.host().eq_ignore_ascii_case(host))
            && url.port_or_known_default() == Some(authority.port_u16().unwrap_or(80))
    }

    fn rate_limited(&self, ip: IpAddr) -> bool {
        let Ok(mut seen) = self.seen.lock() else {
            return false; // 锁中毒时不放大故障
        };
        let now = Instant::now();
        let window = &mut *seen;
        if !window.contains_key(&ip) && window.len() >= MAX_TRACKED_IPS {
            // 防内存增长：定期清掉全空窗口。
            window.retain(|_, stamps| {
                stamps
                    .back()
                    .is_some_and(|at| now.duration_since(*at) < RATE_WINDOW)
            });
        }
        if !window.contains_key(&ip) && window.len() >= MAX_TRACKED_IPS {
            return true;
        }
        let stamps = window.entry(ip).or_default();
        while stamps
            .front()
            .is_some_and(|at| now.duration_since(*at) >= RATE_WINDOW)
        {
            stamps.pop_front();
        }
        if stamps.len() >= RATE_MAX_REQUESTS {
            return true;
        }
        stamps.push_back(now);
        false
    }
}

pub fn router(executor: Arc<Executor>, listen: SocketAddr) -> Router {
    let guard = Guard::new(listen);
    Router::new()
        .route("/v1/info", get(info))
        .route("/v1/capabilities", get(capabilities))
        .route("/v1/directories", post(directories))
        .route("/v1/sessions/{id}", put(bind_session))
        .route("/v1/sessions/{id}/tools", get(tools))
        .route("/v1/sessions/{id}/environment", get(environment))
        .route("/v1/sessions/{id}/jobs", get(session_jobs))
        .route(
            "/v1/sessions/{id}/workspace/instructions",
            get(workspace_instructions),
        )
        .route("/v1/sessions/{id}/workspace/skills", get(workspace_skills))
        .route("/v1/sessions/{id}/workspace/read", post(workspace_read))
        .route("/v1/jobs", post(submit))
        .route("/v1/jobs/{id}", get(job))
        .route("/v1/jobs/{id}/cancel", post(cancel))
        .layer(DefaultBodyLimit::max(16 * 1024 * 1024))
        .route_layer(middleware::from_fn_with_state(
            (executor.clone(), guard),
            guard_request,
        ))
        .with_state(executor)
}

/// 入口中间件：Host/Origin 校验 + 每 IP 限速，然后才做 Bearer 认证
/// （廉价拒绝在前，认证比较不消耗在乱序流量上）。
async fn guard_request(
    State((executor, guard)): State<(Arc<Executor>, Guard)>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    request: Request,
    next: Next,
) -> Response {
    let authority = request
        .headers()
        .get(axum::http::header::HOST)
        .and_then(|host| host.to_str().ok())
        .and_then(|host| host.parse::<Authority>().ok())
        .filter(|authority| guard.host_allowed(authority));
    let Some(authority) = authority else {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({"error": "host not allowed"})),
        )
            .into_response();
    };
    if request
        .headers()
        .get(axum::http::header::ORIGIN)
        .is_some_and(|origin| {
            origin
                .to_str()
                .map_or(true, |origin| !Guard::origin_allowed(origin, &authority))
        })
    {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({"error": "origin not allowed"})),
        )
            .into_response();
    }
    if guard.rate_limited(peer.ip()) {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            Json(json!({"error": "rate limit exceeded; retry shortly"})),
        )
            .into_response();
    }
    let authorized = request
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|header| header.to_str().ok())
        .and_then(|header| header.strip_prefix("Bearer "))
        .is_some_and(|token| executor.authenticate(token));
    if !authorized {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": "unauthorized"})),
        )
            .into_response();
    }
    next.run(request).await
}

async fn info(State(executor): State<Arc<Executor>>) -> Json<ExecutorInfo> {
    Json(executor.info())
}

async fn capabilities(State(executor): State<Arc<Executor>>) -> Json<ExecutorCapabilities> {
    Json(ExecutorCapabilities {
        info: executor.info(),
        tools: crate::native_tool_catalog(),
    })
}

async fn directories(
    State(executor): State<Arc<Executor>>,
    Json(query): Json<DirectoryQuery>,
) -> Result<Json<DirectoryListing>, Error> {
    Ok(Json(executor.browse_directories(query).await?))
}

async fn bind_session(
    State(executor): State<Arc<Executor>>,
    Path(id): Path<Uuid>,
    Json(request): Json<OpenSession>,
) -> Result<Json<SessionBinding>, Error> {
    Ok(Json(executor.bind_session(id, request).await?))
}

async fn tools(
    State(executor): State<Arc<Executor>>,
    Path(id): Path<Uuid>,
) -> Result<Json<Vec<NativeToolDescriptor>>, Error> {
    Ok(Json(executor.tools(id).await?))
}

async fn environment(
    State(executor): State<Arc<Executor>>,
    Path(id): Path<Uuid>,
) -> Result<Json<EnvironmentSnapshot>, Error> {
    Ok(Json(executor.environment(id).await?))
}

async fn submit(
    State(executor): State<Arc<Executor>>,
    Json(request): Json<SubmitJob>,
) -> Result<(StatusCode, Json<JobReceipt>), Error> {
    let receipt = executor.submit(request).await?;
    let status = if receipt.created {
        StatusCode::ACCEPTED
    } else {
        StatusCode::OK
    };
    Ok((status, Json(receipt)))
}

async fn job(
    State(executor): State<Arc<Executor>>,
    Path(id): Path<Uuid>,
) -> Result<Json<JobSnapshot>, Error> {
    Ok(Json(executor.job(id).await?))
}

async fn cancel(
    State(executor): State<Arc<Executor>>,
    Path(id): Path<Uuid>,
) -> Result<Json<JobSnapshot>, Error> {
    Ok(Json(executor.cancel(id).await?))
}

async fn session_jobs(
    State(executor): State<Arc<Executor>>,
    Path(id): Path<Uuid>,
) -> Result<Json<Vec<JobSnapshot>>, Error> {
    Ok(Json(executor.session_jobs(id).await?))
}

async fn workspace_instructions(
    State(executor): State<Arc<Executor>>,
    Path(id): Path<Uuid>,
) -> Result<Json<WorkspaceInstructions>, Error> {
    Ok(Json(executor.workspace_instructions(id).await?))
}

async fn workspace_skills(
    State(executor): State<Arc<Executor>>,
    Path(id): Path<Uuid>,
) -> Result<Json<WorkspaceSkills>, Error> {
    Ok(Json(executor.workspace_skills(id).await?))
}

async fn workspace_read(
    State(executor): State<Arc<Executor>>,
    Path(id): Path<Uuid>,
    Json(request): Json<WorkspaceReadRequest>,
) -> Result<Json<WorkspaceFile>, Error> {
    Ok(Json(executor.workspace_read(id, request).await?))
}

impl IntoResponse for Error {
    fn into_response(self) -> Response {
        let status = match &self {
            Self::Invalid(_) => StatusCode::BAD_REQUEST,
            Self::NotFound => StatusCode::NOT_FOUND,
            Self::ProtectedPath => StatusCode::FORBIDDEN,
            Self::InvocationConflict
            | Self::SessionConflict
            | Self::WorkspaceChanged
            | Self::RecoveryRequired => StatusCode::CONFLICT,
            Self::ShuttingDown => StatusCode::SERVICE_UNAVAILABLE,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        };
        if status.is_server_error() {
            tracing::error!(%self, "executor request failed");
        }
        (status, Json(json!({"error": self.to_string()}))).into_response()
    }
}
