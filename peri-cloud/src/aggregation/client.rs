use super::*;
use reqwest::{
    header::{HeaderValue, AUTHORIZATION},
    Method,
};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::time::Duration;
use url::Url;

pub(super) struct Client {
    http: reqwest::Client,
    origin: Url,
    management: HeaderValue,
    inference: HeaderValue,
    inference_key: String,
    permits: tokio::sync::Semaphore,
}
impl Client {
    pub fn new(
        config: &AggregationConfig,
        management: String,
        inference: String,
    ) -> AggregateResult<Self> {
        fn header(key: &str) -> AggregateResult<HeaderValue> {
            if key.is_empty() || key.len() > 8192 || key.chars().any(char::is_control) {
                return Err(AggregateError::Invalid);
            }
            let mut value = HeaderValue::from_str(&format!("Bearer {key}"))
                .map_err(|_| AggregateError::Invalid)?;
            value.set_sensitive(true);
            Ok(value)
        }
        let http = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(3))
            .timeout(Duration::from_secs(18))
            .build()
            .map_err(|_| AggregateError::Invalid)?;
        Ok(Self {
            http,
            origin: config.origin.clone(),
            management: header(&management)?,
            inference: header(&inference)?,
            inference_key: inference,
            permits: tokio::sync::Semaphore::new(4),
        })
    }
    pub fn inference_base(&self) -> Url {
        self.origin.join("v1").expect("validated origin")
    }
    pub fn inference_key(&self) -> &str {
        &self.inference_key
    }
    pub async fn request(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
        inference: bool,
    ) -> AggregateResult<Value> {
        let _permit = self
            .permits
            .try_acquire()
            .map_err(|_| AggregateError::Upstream("busy"))?;
        // New API's explicit test is a GET but can consume upstream quota.
        let writing = method != Method::GET || path.starts_with("/api/channel/test/");
        let mut req = self
            .http
            .request(
                method,
                self.origin
                    .join(path.trim_start_matches('/'))
                    .map_err(|_| AggregateError::Invalid)?,
            )
            .header(
                AUTHORIZATION,
                if inference {
                    self.inference.clone()
                } else {
                    self.management.clone()
                },
            );
        if let Some(body) = body {
            req = req.json(&body)
        }
        let mut response = req.send().await.map_err(|_| {
            if writing {
                AggregateError::Unknown
            } else {
                AggregateError::Upstream("unavailable")
            }
        })?;
        let status = response.status();
        if status.is_redirection() {
            return Err(AggregateError::Upstream("redirect_rejected"));
        }
        if response.content_length().is_some_and(|n| n > 1024 * 1024) {
            return Err(if writing {
                AggregateError::Unknown
            } else {
                AggregateError::Upstream("response_too_large")
            });
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| {
            if writing {
                AggregateError::Unknown
            } else {
                AggregateError::Upstream("unavailable")
            }
        })? {
            if bytes.len() + chunk.len() > 1024 * 1024 {
                return Err(if writing {
                    AggregateError::Unknown
                } else {
                    AggregateError::Upstream("response_too_large")
                });
            }
            bytes.extend_from_slice(&chunk);
        }
        if status.is_server_error() && writing {
            return Err(AggregateError::Unknown);
        }
        if !status.is_success() {
            return Err(AggregateError::Upstream(match status.as_u16() {
                401 | 403 => "authentication_rejected",
                429 => "rate_limited",
                _ => "gateway_error",
            }));
        }
        let value: Value = serde_json::from_slice(&bytes).map_err(|_| {
            if writing {
                AggregateError::Unknown
            } else {
                AggregateError::Upstream("invalid_response")
            }
        })?;
        if value.get("success") == Some(&json!(false)) {
            return Err(AggregateError::Upstream("operation_rejected"));
        }
        Ok(value)
    }
    pub async fn models(&self) -> AggregateResult<Vec<String>> {
        let result = self.request(Method::GET, "/v1/models", None, true).await?;
        let rows = result["data"]
            .as_array()
            .ok_or(AggregateError::Upstream("invalid_response"))?;
        let mut names: Vec<_> = rows
            .iter()
            .filter_map(|r| r["id"].as_str())
            .filter(|s| valid_model(s))
            .map(str::to_owned)
            .collect();
        names.sort();
        names.dedup();
        Ok(names)
    }
    pub async fn groups(&self) -> AggregateResult<Vec<String>> {
        let result = self
            .request(Method::GET, "/api/group/", None, false)
            .await?;
        serde_json::from_value(result["data"].clone())
            .map_err(|_| AggregateError::Upstream("invalid_response"))
    }
    pub async fn channels(&self, page: usize) -> AggregateResult<ChannelPage> {
        if page > 1000 {
            return Err(AggregateError::Invalid);
        }
        let result = self
            .request(
                Method::GET,
                &format!("/api/channel/?p={}&page_size=30", page + 1),
                None,
                false,
            )
            .await?;
        let data = &result["data"];
        let rows = data["items"]
            .as_array()
            .ok_or(AggregateError::Upstream("invalid_response"))?;
        Ok(ChannelPage {
            items: rows
                .iter()
                .map(channel_view)
                .collect::<AggregateResult<_>>()?,
            page,
            total: data["total"].as_u64().unwrap_or(rows.len() as u64) as usize,
        })
    }
    pub async fn channel(&self, id: i64) -> AggregateResult<ChannelView> {
        if id <= 0 {
            return Err(AggregateError::Invalid);
        }
        let result = self
            .request(Method::GET, &format!("/api/channel/{id}"), None, false)
            .await?;
        channel_view(&result["data"])
    }
}
pub(super) fn valid_model(s: &str) -> bool {
    !s.trim().is_empty() && s.len() <= 256 && !s.chars().any(char::is_control)
}
fn channel_view(value: &Value) -> AggregateResult<ChannelView> {
    let text = |key: &str| value[key].as_str().unwrap_or_default().to_owned();
    let id = value["id"]
        .as_i64()
        .filter(|n| *n > 0)
        .ok_or(AggregateError::Upstream("invalid_response"))?;
    let mut view = ChannelView {
        id,
        name: text("name"),
        kind: value["type"].as_i64().unwrap_or_default(),
        status: value["status"].as_i64().unwrap_or_default(),
        models: text("models"),
        group: text("group"),
        priority: value["priority"].as_i64().unwrap_or_default(),
        weight: value["weight"].as_i64().unwrap_or_default(),
        base_url: value["base_url"].as_str().and_then(public_url),
        model_mapping: value["model_mapping"].as_str().map(str::to_owned),
        test_time: value["test_time"].as_i64().unwrap_or_default(),
        response_time: value["response_time"].as_i64().unwrap_or_default(),
        revision: String::new(),
    };
    view.revision =
        (Sha256::digest(serde_json::to_vec(&view).map_err(|_| AggregateError::Invalid)?))
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
    Ok(view)
}
fn public_url(input: &str) -> Option<String> {
    let u = Url::parse(input).ok()?;
    if !matches!(u.scheme(), "http" | "https")
        || !u.username().is_empty()
        || u.password().is_some()
        || u.query().is_some()
        || u.fragment().is_some()
    {
        return None;
    }
    Some(input.to_owned())
}
