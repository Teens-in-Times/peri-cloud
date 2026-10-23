use super::*;
use reqwest::Method;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use sqlx::Row;
use url::Url;

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ChannelFields {
    pub name: String,
    #[serde(rename = "type")]
    pub kind: i64,
    pub models: String,
    pub group: String,
    pub base_url: String,
    pub model_mapping: String,
    pub priority: i64,
    pub weight: i64,
    pub key: Option<String>,
}
#[derive(Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ChannelAction {
    Create {
        fields: ChannelFields,
    },
    Update {
        id: i64,
        revision: String,
        fields: ChannelFields,
    },
    Status {
        id: i64,
        revision: String,
        enabled: bool,
    },
    Delete {
        id: i64,
        revision: String,
    },
    Test {
        id: i64,
        model: Option<String>,
    },
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ChannelCommand {
    pub request_id: Uuid,
    pub confirmed: bool,
    pub action: ChannelAction,
}

impl Aggregation {
    pub async fn channel_command(
        &self,
        actor: &ControlActor<'_>,
        command: ChannelCommand,
    ) -> AggregateResult<Value> {
        let principal = self.principal(actor, true).await?;
        if !command.confirmed {
            return Err(AggregateError::Invalid);
        }
        let hash = (Sha256::digest(
            serde_json::to_vec(&command.action).map_err(|_| AggregateError::Invalid)?,
        ))
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
        let _guard = self.writes.lock().await;
        let pool = self.journal.identity_pool();
        if let Some(row) = sqlx::query(
            "SELECT principal,request_hash,state,result_json FROM control_operations WHERE id=?",
        )
        .bind(command.request_id.to_string())
        .fetch_optional(&pool)
        .await?
        {
            if row.get::<String, _>("principal") != principal.to_string()
                || row.get::<String, _>("request_hash") != hash
            {
                return Err(AggregateError::Conflict);
            }
            if row.get::<String, _>("state") == "succeeded" {
                return serde_json::from_str(row.get("result_json"))
                    .map_err(|_| AggregateError::Invalid);
            }
            return Err(AggregateError::Unknown);
        }
        let (method, path, body, target) = match &command.action {
            ChannelAction::Create { fields } => {
                validate_fields(fields, true, None)?;
                (
                    Method::POST,
                    "/api/channel/".into(),
                    Some(json!({"mode":"single","channel":field_body(fields,None)})),
                    "new".into(),
                )
            }
            ChannelAction::Update {
                id,
                revision,
                fields,
            } => {
                let current = self.client.channel(*id).await?;
                if current.revision != *revision {
                    return Err(AggregateError::Conflict);
                }
                validate_fields(fields, false, current.base_url.as_deref())?;
                (
                    Method::PUT,
                    "/api/channel/".into(),
                    Some(field_body(fields, Some(*id))),
                    id.to_string(),
                )
            }
            ChannelAction::Status {
                id,
                revision,
                enabled,
            } => {
                let current = self.client.channel(*id).await?;
                if current.revision != *revision {
                    return Err(AggregateError::Conflict);
                }
                (
                    Method::POST,
                    format!("/api/channel/{id}/status"),
                    Some(json!({"status":if *enabled{1}else{2}})),
                    id.to_string(),
                )
            }
            ChannelAction::Delete { id, revision } => {
                let current = self.client.channel(*id).await?;
                if current.revision != *revision {
                    return Err(AggregateError::Conflict);
                }
                (
                    Method::DELETE,
                    format!("/api/channel/{id}"),
                    None,
                    id.to_string(),
                )
            }
            ChannelAction::Test { id, model } => {
                self.client.channel(*id).await?;
                let mut path = format!("/api/channel/test/{id}");
                if let Some(model) = model {
                    if !client::valid_model(model) {
                        return Err(AggregateError::Invalid);
                    }
                    path.push_str("?model=");
                    path.push_str(
                        &url::form_urlencoded::byte_serialize(model.as_bytes()).collect::<String>(),
                    );
                }
                (Method::GET, path, None, id.to_string())
            }
        };
        self.principal(actor, true).await?;
        sqlx::query("INSERT INTO control_operations(id,principal,kind,target,request_hash,state,result_json,created_at) VALUES(?,?,'channel.command',?,?,'pending','{}',?)")
            .bind(command.request_id.to_string()).bind(principal.to_string()).bind(target).bind(hash).bind(self.identity.now()).execute(&pool).await?;
        let result = self.client.request(method, &path, body, false).await;
        let safe = match result {
            Ok(value) => Ok(
                json!({"success":true,"request_id":command.request_id,"test_seconds":value.get("time").and_then(Value::as_f64)}),
            ),
            Err(error) => Err(error),
        };
        let (state, value) = match &safe {
            Ok(value) => ("succeeded", value.clone()),
            Err(AggregateError::Unknown) => ("unknown", json!({"error":"outcome_unknown"})),
            Err(_) => ("failed", json!({"error":"operation_failed"})),
        };
        sqlx::query("UPDATE control_operations SET state=?,result_json=? WHERE id=?")
            .bind(state)
            .bind(value.to_string())
            .bind(command.request_id.to_string())
            .execute(&pool)
            .await?;
        safe
    }
}
fn field_body(f: &ChannelFields, id: Option<i64>) -> Value {
    let mut body = json!({"name":f.name,"type":f.kind,"models":f.models,"group":f.group,"base_url":f.base_url,"model_mapping":f.model_mapping,"priority":f.priority,"weight":f.weight});
    if let Some(id) = id {
        body["id"] = json!(id)
    }
    if let Some(key) = f.key.as_ref().filter(|k| !k.is_empty()) {
        body["key"] = json!(key)
    }
    body
}
fn validate_fields(
    f: &ChannelFields,
    creating: bool,
    original_base: Option<&str>,
) -> AggregateResult<()> {
    let text = |s: &str, max: usize| {
        !s.trim().is_empty() && s.len() <= max && !s.chars().any(char::is_control)
    };
    if !matches!(f.kind, 1 | 14)
        || !text(&f.name, 128)
        || !text(&f.models, 8192)
        || !text(&f.group, 1024)
        || !(0..=10000).contains(&f.priority)
        || !(0..=10000).contains(&f.weight)
    {
        return Err(AggregateError::Invalid);
    }
    if !f.models.split(',').all(client::valid_model) {
        return Err(AggregateError::Invalid);
    }
    let base = Url::parse(&f.base_url).map_err(|_| AggregateError::Invalid)?;
    if !creating
        && original_base != Some(f.base_url.as_str())
        && f.key.as_ref().is_none_or(|k| k.is_empty())
    {
        // A retained upstream credential must not silently follow a new address.
        return Err(AggregateError::Invalid);
    }
    let existing_http = original_base == Some(f.base_url.as_str()) && base.scheme() == "http";
    if (base.scheme() != "https" && !existing_http)
        || base.host_str().is_none()
        || !base.username().is_empty()
        || base.password().is_some()
        || base.query().is_some()
        || base.fragment().is_some()
    {
        return Err(AggregateError::Invalid);
    }
    let mapping: std::collections::BTreeMap<String, String> =
        serde_json::from_str(&f.model_mapping).map_err(|_| AggregateError::Invalid)?;
    if f.model_mapping.len() > 16384
        || !mapping
            .iter()
            .all(|(a, b)| client::valid_model(a) && client::valid_model(b))
    {
        return Err(AggregateError::Invalid);
    }
    if creating && f.key.as_ref().is_none_or(|k| k.is_empty()) {
        return Err(AggregateError::Invalid);
    }
    if f.key
        .as_ref()
        .is_some_and(|k| k.len() > 8192 || k.chars().any(char::is_control))
    {
        return Err(AggregateError::Invalid);
    }
    Ok(())
}
