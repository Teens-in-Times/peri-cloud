//! Authorized, versioned chunks of execution evidence. Never exposes hidden reasoning.
use super::*;
use serde_json::{json, Value};
use sha2::Digest;
use sqlx::Row;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Detail {
    source: String,
    id: String,
    #[serde(default)]
    offset: usize,
    #[serde(default = "chunk_size")]
    chars: usize,
    version: Option<String>,
}
fn chunk_size() -> usize {
    4096
}

pub(super) fn chunk(
    text: &str,
    offset: usize,
    chars: usize,
    expected: Option<&str>,
) -> Result<Value> {
    let version = format!(
        "sha256:{}",
        (sha2::Sha256::digest(text.as_bytes()))
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    );
    if expected.is_some_and(|v| v != version) {
        return Err(TaskError::Conflict);
    }
    let total = text.chars().count();
    if offset > total || chars == 0 || chars > 16384 {
        return Err(TaskError::Invalid);
    }
    let body: String = text.chars().skip(offset).take(chars).collect();
    let next = offset + body.chars().count();
    Ok(
        json!({"text":body,"offset":offset,"next_offset":next,"total_chars":total,"total_bytes":text.len(),"version":version,"complete":next==total,"truncated":offset>0||next<total,"untrusted_evidence":true}),
    )
}

impl WorkerService {
    pub(super) async fn detail(
        &self,
        record: &TaskRecord,
        window: Option<Uuid>,
        detail: Detail,
    ) -> Result<Value> {
        if detail.id.len() > 256 || detail.version.as_ref().is_some_and(|s| s.len() > 80) {
            return Err(TaskError::Invalid);
        }
        let text = match detail.source.as_str() {
            "operation_input" | "operation_output" => {
                let id = detail.id.parse::<i64>().map_err(|_| TaskError::Invalid)?;
                let row=sqlx::query("SELECT call_json,receipt_json,reconciled_json FROM agent_task_operations WHERE task_id=? AND seq=?")
                    .bind(record.id.to_string()).bind(id).fetch_optional(&self.store.pool).await?.ok_or(TaskError::Forbidden)?;
                if detail.source == "operation_input" {
                    row.get::<String, _>("call_json")
                } else {
                    let result: Option<String> = row
                        .get::<Option<String>, _>("reconciled_json")
                        .or(row.get("receipt_json"));
                    let result = result.ok_or(TaskError::Unavailable)?;
                    let result: peri_agent::agent::react::ToolResult =
                        serde_json::from_str(&result)?;
                    serde_json::to_string(
                        &json!({"output":result.output,"execution":result.execution,"is_error":result.is_error,"tool_call_id":result.tool_call_id,"tool_name":result.tool_name}),
                    )?
                }
            }
            "approval_input" => {
                let id = Uuid::parse_str(&detail.id).map_err(|_| TaskError::Invalid)?;
                sqlx::query_scalar::<_,String>("SELECT request_json FROM agent_task_approvals WHERE task_id=? AND id=? AND principal=?")
                    .bind(record.id.to_string()).bind(id.to_string()).bind(record.principal.to_string()).fetch_optional(&self.store.pool).await?.ok_or(TaskError::Forbidden)?
            }
            "conversation" => {
                let window = window.unwrap_or(record.worker_session);
                let allowed:i64=sqlx::query_scalar("SELECT COUNT(*) FROM agent_execution_windows WHERE execution_session=? AND cloud_session=?")
                    .bind(record.execution_session.to_string()).bind(window.to_string()).fetch_one(&self.store.pool).await?;
                if allowed != 1 {
                    return Err(TaskError::Forbidden);
                }
                let live = self
                    .transcripts
                    .read()
                    .get(&record.id)
                    .and_then(Weak::upgrade)
                    .filter(|_| window == record.worker_session);
                let (history, flags) = if let Some(live) = live {
                    let transcript = live.read();
                    let history = transcript.canonical_payloads();
                    let flags = history
                        .iter()
                        .map(|p| (p.id(), transcript.flags(p.id())))
                        .collect::<std::collections::HashMap<_, _>>();
                    (history, flags)
                } else {
                    let state = self
                        .journal
                        .session(record.principal, window)
                        .await
                        .map_err(|_| TaskError::Forbidden)?;
                    (state.history, state.history_flags)
                };
                let id = Uuid::parse_str(&detail.id).map_err(|_| TaskError::Invalid)?;
                let payload = history
                    .iter()
                    .find(|p| p.id().as_uuid() == id)
                    .ok_or(TaskError::Forbidden)?;
                if flags
                    .get(&payload.id())
                    .is_some_and(|f| f.excluded || f.truncated)
                {
                    return Err(TaskError::Forbidden);
                }
                payload.as_message().ok_or(TaskError::Forbidden)?.content()
            }
            _ => return Err(TaskError::Invalid),
        };
        let mut value = chunk(
            &text,
            detail.offset,
            detail.chars,
            detail.version.as_deref(),
        )?;
        value["source"] = json!(detail.source);
        value["id"] = json!(detail.id);
        Ok(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn chunks_preserve_unicode_boundaries_and_reject_stale_versions() {
        let a = chunk("甲🙂乙abc", 0, 2, None).unwrap();
        assert_eq!(a["text"], "甲🙂");
        assert_eq!(a["next_offset"], 2);
        assert_eq!(a["complete"], false);
        let b = chunk("甲🙂乙abc", 2, 8, a["version"].as_str()).unwrap();
        assert_eq!(b["text"], "乙abc");
        assert_eq!(b["complete"], true);
        assert_eq!(b["total_bytes"], 13);
        assert!(matches!(
            chunk("changed", 0, 2, a["version"].as_str()),
            Err(TaskError::Conflict)
        ));
        assert!(chunk("x", 2, 1, None).is_err());
        assert!(chunk("x", 0, 20000, None).is_err());
    }
}
