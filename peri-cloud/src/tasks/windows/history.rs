//! Bounded, source-addressable search; a scanned page is never presented as all history.
use super::*;
use serde_json::{json, Value};
use sqlx::Row;

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(in crate::tasks) struct HistoryFilter {
    pub source: Option<String>,
    pub tool: Option<String>,
    pub call_id: Option<String>,
    pub status: Option<String>,
    pub path: Option<String>,
    pub direction: Option<String>,
}

impl WorkerService {
    pub(in crate::tasks) async fn resume_facts(&self, record: &TaskRecord) -> Result<Value> {
        let raw:Option<String>=sqlx::query_scalar("SELECT record_json FROM agent_window_rotations WHERE task_id=? AND json_extract(record_json,'$.destination')=? AND phase='committed' ORDER BY rowid DESC LIMIT 1")
            .bind(record.id.to_string()).bind(record.worker_session.to_string()).fetch_optional(&self.store.pool).await?;
        let Some(raw) = raw else {
            return Ok(Value::Null);
        };
        let capsule: Rotation = serde_json::from_str(&raw)?;
        let newer:Vec<i64>=sqlx::query_scalar("SELECT seq FROM agent_task_messages WHERE task_id=? AND direction='parent' AND seq>? ORDER BY seq")
            .bind(record.id.to_string()).bind(capsule.instruction_watermark).fetch_all(&self.store.pool).await?;
        let rows=sqlx::query("SELECT seq,window_id,call_id,invocation_id,COALESCE(reconciled_json,receipt_json) AS receipt FROM agent_task_operations WHERE task_id=? ORDER BY seq DESC LIMIT 8")
            .bind(record.id.to_string()).fetch_all(&self.store.pool).await?;
        let mut operations = Vec::new();
        for row in rows {
            let raw: Option<String> = row.get("receipt");
            let receipt: Option<Value> = raw.as_deref().map(serde_json::from_str).transpose()?;
            operations.push(json!({"operation":row.get::<i64,_>("seq"),"window_id":row.get::<String,_>("window_id"),
                "call_id":row.get::<String,_>("call_id"),"invocation_id":row.get::<Option<String>,_>("invocation_id"),
                "execution":receipt.as_ref().map(|r|r["execution"].clone()),"receipt_recorded":receipt.is_some()}));
        }
        Ok(
            json!({"handoff":capsule.id,"summary_instruction_watermark":capsule.instruction_watermark,
            "newer_parent_messages":newer,"newer_message_bodies":"present in the current full parent_messages snapshot",
            "current_recent_operation_facts":operations,"operations_are_recent_preview":true,
            "history_entry":{"view":"overview","task_id":record.id}}),
        )
    }
    pub(in crate::tasks) async fn window_overview(&self, record: &TaskRecord) -> Result<Value> {
        let windows: Vec<(i64,String)> = sqlx::query_as("SELECT window_number,cloud_session FROM agent_execution_windows WHERE execution_session=? ORDER BY window_number DESC LIMIT 8")
            .bind(record.execution_session.to_string()).fetch_all(&self.store.pool).await?;
        let rows:Vec<String>=sqlx::query_scalar("SELECT record_json FROM agent_window_rotations WHERE task_id=? ORDER BY rowid DESC LIMIT 4")
            .bind(record.id.to_string()).fetch_all(&self.store.pool).await?;
        let capsules = rows
            .into_iter()
            .map(|s| serde_json::from_str::<Rotation>(&s))
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let budget =
            super::budget::observation(&self.store.pool, record.principal, record.worker_session)
                .await?;
        let current_goal =
            super::active_parent_goal(&self.store.pool, record.principal, record.parent_session)
                .await?;
        Ok(
            json!({"task":self.summary(record),"parent_goal":record.parent_goal,
            "linked_goal_active":record.parent_goal.is_some()&&record.parent_goal==current_goal,
            "latest_instruction_watermark":self.instruction_watermark(record.id).await?,
            "contract":{"view":"contract"},"budget":super::budget::summary(&budget),
            "windows":windows.into_iter().map(|(number,id)|json!({"window_number":number,"window_id":id})).collect::<Vec<_>>(),
            "window_directory_is_recent":true,"full_window_directory":{"view":"windows"},
            "handoffs":capsules.into_iter().map(|c|json!({"id":c.id,"source_window":c.source_window,
                "destination":c.destination,"phase":c.phase,"status":c.summary_status,
                "summary_preview":c.summary.as_ref().map(|s|clip(s,1200)),"instruction_watermark":c.instruction_watermark,
                "detail":{"source":"handoff","id":c.id},"usage":c.usage})).collect::<Vec<_>>(),
            "operations":self.operations(record.id,0,8).await?,
            "search":{"view":"search","sources":["operations","messages","conversation"]},
            "complete":false,"evidence_note":"Previews and worker summaries are attributed evidence; latest requirements and confirmed receipts govern."}),
        )
    }

    pub(in crate::tasks) async fn search_history(
        &self,
        record: &TaskRecord,
        window: Option<Uuid>,
        after: i64,
        limit: usize,
        query: Option<&str>,
        filter: HistoryFilter,
    ) -> Result<Value> {
        if after < 0
            || !(1..=32).contains(&limit)
            || query.is_some_and(|s| s.len() > 200)
            || [
                &filter.tool,
                &filter.call_id,
                &filter.status,
                &filter.path,
                &filter.direction,
            ]
            .iter()
            .any(|v| v.as_ref().is_some_and(|s| s.len() > 256))
        {
            return Err(TaskError::Invalid);
        }
        let source = filter.source.as_deref().unwrap_or("operations");
        let mut candidates = Vec::new();
        let more;
        match source {
            "operations" => {
                let rows=sqlx::query("SELECT seq,window_id,call_id,call_json,COALESCE(reconciled_json,receipt_json) AS receipt FROM agent_task_operations WHERE task_id=? AND seq>? AND (? IS NULL OR window_id=?) ORDER BY seq LIMIT 65")
                    .bind(record.id.to_string()).bind(after).bind(window.map(|v|v.to_string())).bind(window.map(|v|v.to_string())).fetch_all(&self.store.pool).await?;
                more = rows.len() > 64;
                for row in rows.into_iter().take(64) {
                    let id = row.get::<i64, _>("seq");
                    let call: Value = serde_json::from_str(row.get("call_json"))?;
                    let receipt: Option<String> = row.get("receipt");
                    let result: Option<Value> =
                        receipt.as_deref().map(serde_json::from_str).transpose()?;
                    candidates.push(json!({"cursor":id,"window_id":row.get::<String,_>("window_id"),
                        "call_id":row.get::<String,_>("call_id"),"tool":call["name"],"input":call["input"],"result":result,
                        "detail":{"source":"operation_output","id":id.to_string()},"input_detail":{"source":"operation_input","id":id.to_string()}}));
                }
            }
            "messages" => {
                if window.is_some() || filter.tool.is_some() || filter.call_id.is_some() {
                    return Err(TaskError::Invalid);
                }
                let rows=sqlx::query("SELECT seq,direction,body,state FROM agent_task_messages WHERE task_id=? AND seq>? ORDER BY seq LIMIT 65")
                    .bind(record.id.to_string()).bind(after).fetch_all(&self.store.pool).await?;
                more = rows.len() > 64;
                for row in rows.into_iter().take(64) {
                    let id = row.get::<i64, _>("seq");
                    candidates.push(json!({"cursor":id,"direction":row.get::<String,_>("direction"),"body":row.get::<String,_>("body"),"status":row.get::<String,_>("state"),"detail":{"source":"task_message","id":id.to_string()}}));
                }
            }
            "conversation" => {
                if filter.tool.is_some()
                    || filter.call_id.is_some()
                    || filter.status.is_some()
                    || filter.direction.is_some()
                {
                    return Err(TaskError::Invalid);
                }
                let page = self.conversation(record, after, 16, window).await?;
                more = page["has_more"].as_bool().unwrap_or(false);
                for entry in page["entries"].as_array().ok_or(TaskError::Invalid)? {
                    let mut entry = entry.clone();
                    entry["window_id"] = page["window_id"].clone();
                    entry["detail"] = json!({"source":"conversation","id":entry["message_id"]});
                    candidates.push(entry);
                }
            }
            _ => return Err(TaskError::Invalid),
        }
        let mut next = after;
        let mut scanned = 0;
        let mut hits = Vec::new();
        let mut used = 0;
        for entry in candidates.iter() {
            let text = entry.to_string();
            let matches = query.is_none_or(|q| text.to_lowercase().contains(&q.to_lowercase()))
                && filter.path.as_ref().is_none_or(|p| text.contains(p))
                && filter
                    .tool
                    .as_ref()
                    .is_none_or(|t| entry["tool"] == t.as_str())
                && filter
                    .call_id
                    .as_ref()
                    .is_none_or(|id| entry["call_id"] == id.as_str())
                && filter
                    .direction
                    .as_ref()
                    .is_none_or(|d| entry["direction"] == d.as_str())
                && filter.status.as_ref().is_none_or(|s| {
                    entry["status"] == s.as_str()
                        || entry["result"]["execution"]["status"] == s.as_str()
                });
            let preview = json!({"cursor":entry["cursor"],"window_id":entry["window_id"],"tool":entry["tool"],
                "call_id":entry["call_id"],"direction":entry["direction"],"preview":clip(&text,1600),
                "truncated":text.chars().count()>1600,"detail":entry["detail"],"input_detail":entry["input_detail"]});
            let bytes = serde_json::to_vec(&preview)?.len();
            if matches && (hits.len() >= limit || used + bytes > 24 * 1024) {
                break;
            }
            next = entry["cursor"].as_i64().ok_or(TaskError::Invalid)?;
            scanned += 1;
            if matches {
                used += bytes;
                hits.push(preview);
            }
        }
        let content_complete =
            source != "conversation" || candidates.iter().all(|e| e["truncated"] == false);
        Ok(
            json!({"source":source,"entries":hits,"next_cursor":next,"has_more":more||scanned<candidates.len(),
            "scanned":scanned,"scan_limit":64,"complete_scan":after==0&&!more&&scanned==candidates.len(),
            "complete_content_search":content_complete&&after==0&&!more&&scanned==candidates.len(),
            "searches_conversation_previews":source=="conversation",
            "lexical_search":true,"preview_only":true,"hidden_reasoning_excluded":true}),
        )
    }
}
