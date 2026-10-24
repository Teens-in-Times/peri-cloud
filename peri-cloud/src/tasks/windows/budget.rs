//! Measure complete requests and calibrate conservative estimates from provider usage.
use super::*;
use peri_model::{ModelRequest, ModelResponse, ProviderProtocol};
use serde_json::json;
use sqlx::SqlitePool;

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct BudgetObservation {
    pub requests: u64,
    pub estimate: usize,
    pub observed_input: Option<u64>,
    pub output_tokens: Option<u32>,
    pub cache_read: Option<u32>,
    pub cache_created: Option<u32>,
    pub ratio_millis: u64,
    pub fixed_prefix_estimate: usize,
    pub usage_samples: u64,
    pub known_input_tokens: u64,
    pub known_output_tokens: u64,
    pub known_cache_read_tokens: u64,
    pub known_cache_created_tokens: u64,
}

pub(crate) async fn observation(
    pool: &SqlitePool,
    principal: Uuid,
    window: Uuid,
) -> Result<BudgetObservation> {
    let raw: Option<String> = sqlx::query_scalar(
        "SELECT record_json FROM agent_window_budget WHERE window_id=? AND principal=?",
    )
    .bind(window.to_string())
    .bind(principal.to_string())
    .fetch_optional(pool)
    .await?;
    Ok(raw
        .as_deref()
        .map(serde_json::from_str)
        .transpose()?
        .unwrap_or_default())
}

pub(crate) fn estimate(request: &ModelRequest) -> usize {
    // Includes system, tools, protocol envelopes and current messages; explicitly an estimate.
    serde_json::to_vec(request)
        .map_or(usize::MAX / 4, |v| v.len().div_ceil(2))
        .saturating_add(512)
}

pub(crate) fn calibrate(estimate: usize, previous: &BudgetObservation) -> usize {
    estimate
        .saturating_mul(previous.ratio_millis.max(1000) as usize)
        .div_ceil(1000)
}

pub(crate) fn fixed_prefix(request: &ModelRequest) -> usize {
    let system: Vec<_> = request
        .messages
        .iter()
        .filter(|m| matches!(m, peri_model::ModelMessage::System { .. }))
        .collect();
    (serde_json::to_vec(&system).map_or(0, |v| v.len())
        + serde_json::to_vec(&request.tools).map_or(0, |v| v.len()))
    .div_ceil(2)
        + 512
}

pub(crate) async fn record(
    pool: &SqlitePool,
    principal: Uuid,
    window: Uuid,
    estimate: usize,
    prefix_estimate: usize,
    response: &ModelResponse,
    protocol: Option<ProviderProtocol>,
) -> Result<()> {
    let mut old = observation(pool, principal, window).await?;
    old.requests += 1;
    old.estimate = estimate;
    old.fixed_prefix_estimate = prefix_estimate;
    old.observed_input = None;
    old.output_tokens = None;
    old.cache_read = None;
    old.cache_created = None;
    if let Some(usage) = response.usage() {
        let total = u64::from(usage.input_tokens)
            + if protocol == Some(ProviderProtocol::Anthropic) {
                u64::from(usage.cache_read_input_tokens.unwrap_or(0))
                    + u64::from(usage.cache_creation_input_tokens.unwrap_or(0))
            } else {
                0
            };
        old.observed_input = Some(total);
        old.output_tokens = Some(usage.output_tokens);
        old.cache_read = usage.cache_read_input_tokens;
        old.cache_created = usage.cache_creation_input_tokens;
        old.usage_samples += 1;
        old.known_input_tokens = old.known_input_tokens.saturating_add(total);
        old.known_output_tokens = old
            .known_output_tokens
            .saturating_add(u64::from(usage.output_tokens));
        old.known_cache_read_tokens = old
            .known_cache_read_tokens
            .saturating_add(u64::from(usage.cache_read_input_tokens.unwrap_or(0)));
        old.known_cache_created_tokens = old
            .known_cache_created_tokens
            .saturating_add(u64::from(usage.cache_creation_input_tokens.unwrap_or(0)));
        if estimate > 0 {
            old.ratio_millis = old
                .ratio_millis
                .max(total.saturating_mul(1000).div_ceil(estimate as u64));
        }
    }
    sqlx::query("INSERT INTO agent_window_budget VALUES(?,?,?) ON CONFLICT(window_id) DO UPDATE SET record_json=excluded.record_json WHERE principal=excluded.principal")
        .bind(window.to_string()).bind(principal.to_string()).bind(serde_json::to_string(&old)?).execute(pool).await?;
    Ok(())
}

pub(crate) async fn preflight(
    pool: &SqlitePool,
    principal: Uuid,
    window: Uuid,
    estimate: usize,
    capacity: u32,
    reserve: u32,
) -> Result<bool> {
    let previous = observation(pool, principal, window).await?;
    let used = calibrate(estimate, &previous);
    if used < capacity.saturating_sub(reserve.min(capacity / 2)) as usize {
        return Ok(true);
    }
    let state = if previous.requests > 0 {
        "rotate_requested"
    } else {
        "waiting_external"
    };
    let note = if previous.requests > 0 {
        "Full request budget reached; window rotation requested."
    } else {
        "Fresh window cannot fit the required context and output reserve. Waiting for a smaller contract or larger capacity; no rotation loop."
    };
    let note=format!("{note} Estimated input {used}, context capacity {capacity}, reserved output/headroom {reserve}.");
    sqlx::query("UPDATE agent_tasks SET record_json=json_set(record_json,'$.state',?,'$.note',?,'$.wake_at',NULL,'$.revision',json_extract(record_json,'$.revision')+1) WHERE principal=? AND worker_session=? AND json_extract(record_json,'$.state')='running'")
        .bind(state).bind(note).bind(principal.to_string()).bind(window.to_string()).execute(pool).await?;
    Ok(false)
}

pub(crate) fn summary(value: &BudgetObservation) -> serde_json::Value {
    json!({"requests":value.requests,"last_estimate":value.estimate,"observed_input_tokens":value.observed_input,
        "output_tokens":value.output_tokens,"cache_read_tokens":value.cache_read,"cache_created_tokens":value.cache_created,
        "fixed_prefix_estimate":value.fixed_prefix_estimate,"calibration_ratio_millis":value.ratio_millis.max(1000),"estimate_is_exact":false,
        "known_usage_samples":value.usage_samples,"unknown_usage_requests":value.requests.saturating_sub(value.usage_samples),
        "known_input_tokens":value.known_input_tokens,"known_output_tokens":value.known_output_tokens,
        "known_cache_read_tokens":value.known_cache_read_tokens,"known_cache_created_tokens":value.known_cache_created_tokens})
}
