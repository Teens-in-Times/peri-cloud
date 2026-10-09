use std::collections::HashSet;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use url::Url;
use uuid::Uuid;

use super::{HostError, HostResult};
use crate::qq::{QqEndpoints, QqInteractionMode};

/// Operator configuration. Credentials are environment/file references and
/// never have a Debug or Serialize projection into logs, models or browsers.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostConfig {
    pub state_dir: PathBuf,
    pub listen: SocketAddr,
    pub public_origin: String,
    pub bootstrap_env: String,
    pub model: ModelConfig,
    #[serde(default)]
    pub devices: Vec<DeviceConnection>,
    pub qq: Option<QqConfig>,
    #[serde(default = "max_iterations")]
    pub max_iterations: usize,
    #[serde(default = "system_prompt")]
    pub system_prompt: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelConfig {
    pub api_base: Url,
    pub api_key_env: String,
    pub model: String,
    #[serde(default = "context_window")]
    pub context_window: u32,
    #[serde(default = "max_tokens")]
    pub max_tokens: u32,
    #[serde(default)]
    pub thinking_enabled: bool,
    #[serde(default)]
    pub supports_thinking_content: bool,
    pub reasoning_effort: Option<String>,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeviceConnection {
    pub device_id: Uuid,
    /// The local endpoint, either a same-host executor or an SSH forward.
    pub endpoint: SocketAddr,
    pub token_file: PathBuf,
    /// Optional direct SSH tunnel managed by this cloud host. Reverse tunnels
    /// originate at the device; omit this when using a device-owned reverse SSH.
    pub ssh: Option<SshConnection>,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SshConnection {
    /// Existing SSH config alias; known-host and key selection belong to SSH.
    pub host_alias: String,
    pub remote_executor: SocketAddr,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QqConfig {
    pub instance_id: String,
    pub app_id_env: String,
    pub client_secret_env: String,
    pub transport: QqTransport,
    #[serde(default)]
    pub interaction: QqInteractionMode,
    /// Optional authenticated deployment-local relay for protocol testing.
    pub relay: Option<QqRelay>,
}

#[derive(Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum QqTransport {
    Websocket,
    Webhook,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QqRelay {
    pub api: Url,
    pub token: Url,
}

impl HostConfig {
    pub async fn read(path: &Path) -> HostResult<Self> {
        let path = tokio::fs::canonicalize(path)
            .await
            .map_err(|_| HostError::Configuration)?;
        let metadata = tokio::fs::metadata(&path)
            .await
            .map_err(|_| HostError::Configuration)?;
        if !metadata.is_file() || metadata.len() > 65536 {
            return Err(HostError::Configuration);
        }
        let bytes = tokio::fs::read(&path)
            .await
            .map_err(|_| HostError::Configuration)?;
        let mut config: Self =
            serde_json::from_slice(&bytes).map_err(|_| HostError::Configuration)?;
        let directory = path.parent().ok_or(HostError::Configuration)?;
        if config.state_dir.is_relative() {
            config.state_dir = directory.join(&config.state_dir);
        }
        for device in &mut config.devices {
            if device.token_file.is_relative() {
                device.token_file = directory.join(&device.token_file);
            }
        }
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> HostResult<()> {
        if !self.listen.ip().is_loopback()
            || self.state_dir.as_os_str().is_empty()
            || !(1..=1024).contains(&self.max_iterations)
            || self.system_prompt.len() > 65536
        {
            return Err(HostError::Configuration);
        }
        crate::portal::PortalConfig::new(&self.public_origin)
            .map_err(|_| HostError::Configuration)?;
        let model = &self.model;
        let loopback = matches!(model.api_base.host_str(), Some("127.0.0.1" | "[::1]"));
        if model.api_base.host().is_none()
            || !(model.api_base.scheme() == "https"
                || (model.api_base.scheme() == "http" && loopback))
            || !model.api_base.username().is_empty()
            || model.api_base.password().is_some()
            || model.api_base.query().is_some()
            || model.api_base.fragment().is_some()
            || model.model.trim().is_empty()
            || model.model.len() > 256
            || !(1024..=16777216).contains(&model.context_window)
            || model.max_tokens == 0
            || model.max_tokens > model.context_window
        {
            return Err(HostError::Configuration);
        }
        environment_name(&self.bootstrap_env)?;
        environment_name(&model.api_key_env)?;
        if model
            .reasoning_effort
            .as_ref()
            .is_some_and(|value| value.len() > 32 || value.is_empty())
        {
            return Err(HostError::Configuration);
        }
        if self.devices.len() > 128 {
            return Err(HostError::Configuration);
        }
        let mut devices = HashSet::new();
        let mut endpoints = HashSet::new();
        for device in &self.devices {
            if !devices.insert(device.device_id)
                || !endpoints.insert(device.endpoint)
                || !device.endpoint.ip().is_loopback()
                || device.endpoint.port() == 0
                || device.token_file.as_os_str().is_empty()
            {
                return Err(HostError::Configuration);
            }
            if let Some(ssh) = &device.ssh {
                if ssh.host_alias.is_empty()
                    || ssh.host_alias.len() > 128
                    || ssh.host_alias.starts_with('-')
                    || !ssh
                        .host_alias
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || b"_.-".contains(&byte))
                    || !ssh.remote_executor.ip().is_loopback()
                    || ssh.remote_executor.port() == 0
                {
                    return Err(HostError::Configuration);
                }
            }
        }
        if let Some(qq) = &self.qq {
            environment_name(&qq.app_id_env)?;
            environment_name(&qq.client_secret_env)?;
            if qq.instance_id.is_empty()
                || qq.instance_id.len() > 128
                || qq.instance_id.chars().any(char::is_control)
            {
                return Err(HostError::Configuration);
            }
            qq.endpoints()?;
        }
        Ok(())
    }
}

impl QqConfig {
    pub(crate) fn endpoints(&self) -> HostResult<QqEndpoints> {
        match &self.relay {
            Some(relay) => QqEndpoints::loopback(relay.api.clone(), relay.token.clone())
                .map_err(|_| HostError::Configuration),
            None => Ok(QqEndpoints::default()),
        }
    }
}

fn environment_name(name: &str) -> HostResult<()> {
    if name.is_empty()
        || name.len() > 128
        || name.starts_with(|c: char| c.is_ascii_digit())
        || !name.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_')
    {
        return Err(HostError::Configuration);
    }
    Ok(())
}

pub(crate) fn credential(name: &str) -> HostResult<String> {
    environment_name(name)?;
    std::env::var(name)
        .ok()
        .filter(|value| !value.is_empty() && value.len() <= 8192)
        .ok_or(HostError::Credential)
}

fn max_iterations() -> usize {
    64
}
fn context_window() -> u32 {
    128000
}
fn max_tokens() -> u32 {
    32000
}
fn system_prompt() -> String {
    "You are a personal cloud agent. Work only on the session's selected device and workspace. Use its reported operating system. Keep tool output and reasoning internal; give the user clear, concise replies. When an execution is unconfirmed, query its existing task ID before requesting another operation.".into()
}

#[cfg(test)]
#[path = "config_test.rs"]
mod tests;
