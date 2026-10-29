//! Authenticated HTTP adapter on loopback/SSH or an explicitly verified WireGuard address.

use std::sync::Arc;

use axum::extract::{DefaultBodyLimit, Path, Request, State};
use axum::http::StatusCode;
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use serde_json::json;
use uuid::Uuid;

use crate::{Error, Executor};
use peri_acp_types::device_executor::*;

pub fn router(executor: Arc<Executor>) -> Router {
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
            executor.clone(),
            authenticate,
        ))
        .with_state(executor)
}

async fn authenticate(
    State(executor): State<Arc<Executor>>,
    request: Request,
    next: Next,
) -> Response {
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
