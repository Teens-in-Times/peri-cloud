//! Narrow archive port for deployment-owned journals. It grants no task or workspace authority.
use async_trait::async_trait;
use peri_acp_types::{
    context_archive::*,
    messages::MessageId,
    session_resources::{
        SessionResourceError, SessionResourceErrorKind, SessionResourceResult, SessionResources,
    },
    store::{CompactionChange, MessageFlags, PersistedPayload},
    thread::ThreadId,
};
use std::{collections::HashMap, sync::Arc};

#[async_trait]
pub trait ArchiveStorage: Send + Sync {
    fn permits_recovery(&self, _payload: &PersistedPayload) -> bool {
        true
    }
    async fn read_archive_metadata(
        &self,
        id: &ThreadId,
        query: &ArchiveMetadataQuery,
    ) -> SessionResourceResult<ArchiveMetadataPage>;
    async fn read_archive_chunk(
        &self,
        id: &ThreadId,
        query: &ArchiveChunkQuery,
    ) -> SessionResourceResult<ArchiveChunk>;
    async fn read_archive_chunks(
        &self,
        id: &ThreadId,
        batch: &ArchiveChunkBatch,
    ) -> SessionResourceResult<Vec<ArchiveChunk>> {
        let mut chunks = Vec::with_capacity(batch.queries().len());
        for query in batch.queries() {
            chunks.push(self.read_archive_chunk(id, query).await?);
        }
        Ok(chunks)
    }
    async fn inherited_archive_root(
        &self,
        _reader: &ThreadId,
        _source: &ThreadId,
        _anchor: MessageId,
    ) -> SessionResourceResult<ArchiveRoot> {
        Err(SessionResourceError::new(
            SessionResourceErrorKind::NotFound,
        ))
    }
}

/// The deployment persists originals before exposing projections, with typed outcome certainty.
#[async_trait]
pub trait DeploymentArchive: ArchiveStorage {
    async fn checkpoint(
        &self,
        id: &ThreadId,
        payloads: &[PersistedPayload],
        flags: &HashMap<MessageId, MessageFlags>,
    ) -> SessionResourceResult<()>;
    async fn apply_compaction(
        &self,
        id: &ThreadId,
        change: &CompactionChange,
    ) -> SessionResourceResult<()>;
}

pub(crate) struct SessionArchiveStorage(pub Arc<dyn SessionResources>);
#[async_trait]
impl ArchiveStorage for SessionArchiveStorage {
    async fn read_archive_metadata(
        &self,
        id: &ThreadId,
        query: &ArchiveMetadataQuery,
    ) -> SessionResourceResult<ArchiveMetadataPage> {
        self.0.read_archive_metadata(id, query).await
    }
    async fn read_archive_chunk(
        &self,
        id: &ThreadId,
        query: &ArchiveChunkQuery,
    ) -> SessionResourceResult<ArchiveChunk> {
        self.0.read_archive_chunk(id, query).await
    }
    async fn read_archive_chunks(
        &self,
        id: &ThreadId,
        batch: &ArchiveChunkBatch,
    ) -> SessionResourceResult<Vec<ArchiveChunk>> {
        self.0.read_archive_chunks(id, batch).await
    }
    async fn inherited_archive_root(
        &self,
        reader: &ThreadId,
        source: &ThreadId,
        anchor: MessageId,
    ) -> SessionResourceResult<ArchiveRoot> {
        self.0.inherited_archive_root(reader, source, anchor).await
    }
}
