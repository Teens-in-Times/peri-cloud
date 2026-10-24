use super::*;
use async_trait::async_trait;
use peri_acp_types::context_archive::{ArchiveReadRequest, ContextArchiveReadPort};
use peri_agent::{
    context_archive::ContextArchiveReader,
    tools::{BaseTool, ContextRetention, ToolContext},
};

pub(crate) struct ArchiveTool(pub Arc<CloudArchive>);
#[async_trait]
impl BaseTool for ArchiveTool {
    fn name(&self) -> &str {
        "ReadContextArchive"
    }
    fn description(&self) -> &str {
        "Read original archived context by its reference, with a bounded token budget and continuation cursor. Fragments use byte offsets and declare utf8 or hex encoding; complete and next_cursor describe completeness. References do not grant access to other sessions."
    }
    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({"type":"object","properties":{"archive_ref":{"type":"string"},"cursor":{"type":["string","null"]},"budget_tokens":{"type":"integer","minimum":256,"maximum":4096}},"required":["archive_ref"],"additionalProperties":false})
    }
    fn is_direct(&self) -> bool {
        true
    }
    fn context_retention(&self) -> ContextRetention {
        ContextRetention::ArchiveRead
    }
    async fn invoke(
        &self,
        input: serde_json::Value,
        ctx: ToolContext<'_>,
    ) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        if ctx.session_id.as_deref() != Some(self.0.scope.session_id.to_string().as_str()) {
            return Err(forbidden().into());
        }
        let request: ArchiveReadRequest = serde_json::from_value(input)?;
        let reader =
            ContextArchiveReader::from_storage(self.0.clone(), self.0.scope.session_id.to_string());
        let result = tokio::select! {
            biased;
            _ = ctx.cancellation.cancelled() => return Err("archive read cancelled".into()),
            result = reader.read(request) => result?,
        };
        Ok(serde_json::to_string(&result)?)
    }
}
