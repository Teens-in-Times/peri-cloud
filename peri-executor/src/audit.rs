//! 设备侧审计日志（executor review 2026-10-26 P1-5）。
//!
//! 云端有 trace，但设备属主需要本地可答的问题：「这台机器上的执行器替谁在
//! 何时执行过什么、结果如何」。JSONL 追加写 `state_dir/audit.jsonl`，按大小
//! 轮转（5 MiB，保留一代）。只记录身份与结论——不含参数正文与输出，
//! 避免把用户代码/密钥二次落盘。

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde::Serialize;
use uuid::Uuid;

const AUDIT_FILE: &str = "audit.jsonl";
const AUDIT_ROTATED: &str = "audit.jsonl.1";
/// 单文件上限；达到后滚动为 `.jsonl.1`（仅保留一代）。
const AUDIT_MAX_BYTES: u64 = 5 * 1024 * 1024;

#[derive(Serialize)]
pub(crate) struct AuditEvent<'a> {
    /// RFC 3339 时间戳。
    pub ts: String,
    /// `admitted`（受理）或 `settled`（结算）。
    pub kind: &'a str,
    pub session_id: Uuid,
    pub invocation_id: Uuid,
    pub tool: &'a str,
    /// settled 时的终态；admitted 时为空。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<&'a str>,
    /// settled 时的执行时长（毫秒）；admitted 时为空。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
}

#[derive(Clone)]
pub(crate) struct AuditLog {
    path: PathBuf,
    write_gate: Arc<Mutex<()>>,
}

impl AuditLog {
    pub fn open(root: &Path) -> Self {
        Self {
            path: root.join(AUDIT_FILE),
            write_gate: Arc::new(Mutex::new(())),
        }
    }

    /// 追加一条审计事件。阻塞写很小（<300B），在 spawn_blocking 中调用；
    /// 任何失败只记 warn——审计不可用绝不阻塞执行受理。
    pub async fn record(&self, event: &AuditEvent<'_>) {
        let line = match serde_json::to_string(event) {
            Ok(line) => line,
            Err(error) => {
                tracing::warn!(%error, "audit serialization failed");
                return;
            }
        };
        let audit = self.clone();
        if let Err(error) = tokio::task::spawn_blocking(move || audit.append(line)).await {
            tracing::warn!(%error, "audit worker failed");
        }
    }

    fn append(&self, mut line: String) {
        let _gate = self
            .write_gate
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if let Err(error) = self.rotate_if_large() {
            tracing::warn!(%error, "audit rotation check failed");
        }
        let mut options = std::fs::OpenOptions::new();
        options.create(true).append(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        line.push('\n');
        match options
            .open(&self.path)
            .and_then(|mut file| file.write_all(line.as_bytes()))
        {
            Ok(()) => {}
            Err(error) => tracing::warn!(%error, "audit append failed"),
        }
    }

    fn rotate_if_large(&self) -> std::io::Result<()> {
        let Ok(metadata) = std::fs::metadata(&self.path) else {
            return Ok(());
        };
        if metadata.len() < AUDIT_MAX_BYTES {
            return Ok(());
        }
        let rotated = self.path.with_file_name(AUDIT_ROTATED);
        let _ = std::fs::remove_file(&rotated);
        std::fs::rename(&self.path, &rotated)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn records_and_rotates() {
        let dir = tempfile::tempdir().unwrap();
        let audit = AuditLog::open(dir.path());
        audit
            .record(&AuditEvent {
                ts: "2026-10-26T00:00:00Z".into(),
                kind: "admitted",
                session_id: Uuid::new_v4(),
                invocation_id: Uuid::new_v4(),
                tool: "Bash",
                status: None,
                duration_ms: None,
            })
            .await;
        let content = std::fs::read_to_string(dir.path().join(AUDIT_FILE)).unwrap();
        assert!(content.contains("\"kind\":\"admitted\""));

        // 超限滚动：把当前文件填充过上限再写一条。
        let current = dir.path().join(AUDIT_FILE);
        let padding = vec![b' '; AUDIT_MAX_BYTES as usize];
        std::fs::write(&current, padding).unwrap();
        audit
            .record(&AuditEvent {
                ts: "2026-10-26T00:00:01Z".into(),
                kind: "settled",
                session_id: Uuid::new_v4(),
                invocation_id: Uuid::new_v4(),
                tool: "Write",
                status: Some("Completed"),
                duration_ms: Some(3),
            })
            .await;
        assert!(dir.path().join(AUDIT_ROTATED).exists());
        let fresh = std::fs::read_to_string(&current).unwrap();
        assert!(fresh.contains("\"kind\":\"settled\""));
    }
}
