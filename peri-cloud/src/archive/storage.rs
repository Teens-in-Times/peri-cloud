use super::*;
use async_trait::async_trait;
use peri_acp_types::{
    context_archive::*,
    store::{serialize_persisted_payload, CompactionChange, MessageFlags},
    thread::ThreadId,
};
use peri_agent::context_archive_storage::{ArchiveStorage, DeploymentArchive};
use std::collections::{HashMap, HashSet};

fn authorized_ids(
    payloads: &[PersistedPayload],
    flags: &HashMap<MessageId, MessageFlags>,
) -> HashSet<MessageId> {
    let positions: HashMap<_, _> = payloads
        .iter()
        .enumerate()
        .map(|(i, p)| (p.id(), i))
        .collect();
    let mut allowed: HashSet<_> = payloads
        .iter()
        .filter(|p| !flags.get(&p.id()).is_some_and(|f| f.excluded))
        .map(|p| p.id())
        .collect();
    let roots: Vec<_> = allowed
        .iter()
        .filter_map(|id| flags.get(id).and_then(|f| f.projection.as_ref()))
        .flat_map(|p| &p.archive_roots)
        .collect();
    for root in roots {
        let (Some(first), Some(last)) = (
            positions.get(&root.first_anchor),
            positions.get(&root.last_anchor),
        ) else {
            continue;
        };
        for payload in payloads.iter().take(last + 1).skip(*first) {
            if let Some(catalog) = flags
                .get(&payload.id())
                .and_then(|f| f.projection.as_ref())
                .and_then(|p| p.archive_catalog.as_ref())
            {
                allowed.insert(payload.id());
                allowed.extend(catalog.members.iter().map(|m| m.message_id));
                allowed.extend(catalog.dependencies.iter().copied());
            }
        }
        // A root's fixed tail is a metadata bound, not a grant to read every body before it.
        allowed.insert(root.committed_tail);
    }
    allowed
}

impl CloudArchive {
    async fn ordinal(
        &self,
        conn: &mut SqliteConnection,
        id: MessageId,
    ) -> SessionResourceResult<i64> {
        sqlx::query_scalar(
            "SELECT ordinal FROM cloud_archive_entries WHERE scope=? AND message_id=?",
        )
        .bind(self.key())
        .bind(id.as_uuid().to_string())
        .fetch_optional(conn)
        .await
        .map_err(unavailable)?
        .ok_or_else(not_found)
    }

    async fn save(
        &self,
        conn: &mut SqliteConnection,
        payload: &PersistedPayload,
        ordinal: i64,
        projection: Option<&peri_acp_types::projection::MessageProjectionDirective>,
    ) -> SessionResourceResult<()> {
        if !readable(payload) {
            return Ok(());
        }
        if let Some(projection) = projection {
            validate_projection_metadata(projection).map_err(unavailable)?;
        }
        let canonical = serialize_persisted_payload(payload).map_err(unavailable)?;
        let projection = projection
            .map(serde_json::to_string)
            .transpose()
            .map_err(unavailable)?;
        let changed = sqlx::query("INSERT INTO cloud_archive_entries(scope,session_id,message_id,ordinal,canonical,projection_json) VALUES(?,?,?,?,?,?) ON CONFLICT(scope,message_id) DO UPDATE SET projection_json=excluded.projection_json,ordinal=excluded.ordinal WHERE canonical=excluded.canonical")
            .bind(self.key()).bind(self.scope.session_id.to_string()).bind(payload.id().as_uuid().to_string()).bind(ordinal).bind(canonical.as_bytes()).bind(projection)
            .execute(conn).await.map_err(unavailable)?.rows_affected();
        if changed != 1 {
            return Err(SessionResourceError::conflict(
                "archive canonical identity changed",
            ));
        }
        Ok(())
    }
}

#[async_trait]
impl ArchiveStorage for CloudArchive {
    fn permits_recovery(&self, payload: &PersistedPayload) -> bool {
        readable(payload)
    }

    async fn read_archive_metadata(
        &self,
        id: &ThreadId,
        query: &ArchiveMetadataQuery,
    ) -> SessionResourceResult<ArchiveMetadataPage> {
        if !(1..=32).contains(&query.limit) {
            return Err(invalid("archive metadata limit must be 1..32"));
        }
        let pool = self.scope.journal.identity_pool();
        let mut tx = pool.begin().await.map_err(unavailable)?;
        self.authorize(&mut tx, id).await?;
        let latest: String = sqlx::query_scalar("SELECT message_id FROM cloud_archive_entries WHERE scope=? ORDER BY ordinal DESC LIMIT 1")
            .bind(self.key()).fetch_optional(&mut *tx).await.map_err(unavailable)?.ok_or_else(not_found)?;
        let tail = query.committed_tail.unwrap_or(message_id(latest)?);
        let end = self.ordinal(&mut tx, tail).await?;
        let first = match query.first_anchor {
            Some(id) => self.ordinal(&mut tx, id).await?,
            None => 0,
        };
        let last = match query.last_anchor {
            Some(id) => self.ordinal(&mut tx, id).await?,
            None => end,
        };
        let after = match query.after {
            Some(id) => self.ordinal(&mut tx, id).await?,
            None => -1,
        };
        if first > last || last > end || after > end {
            return Err(invalid("archive range is outside committed tail"));
        }
        let anchor = match query.anchor {
            Some(id) => Some(self.ordinal(&mut tx, id).await?),
            None => None,
        };
        let rows = sqlx::query("SELECT message_id,projection_json FROM cloud_archive_entries WHERE scope=? AND ordinal>=? AND ordinal<=? AND ordinal>? AND (? IS NULL OR ordinal=?) ORDER BY ordinal LIMIT ?")
            .bind(self.key()).bind(first).bind(last).bind(after).bind(anchor).bind(anchor).bind(i64::from(query.limit)+1)
            .fetch_all(&mut *tx).await.map_err(unavailable)?;
        let more = rows.len() > query.limit as usize;
        let mut items = Vec::new();
        for row in rows.into_iter().take(query.limit as usize) {
            let projection = row
                .get::<Option<String>, _>("projection_json")
                .map(|raw| serde_json::from_str(&raw))
                .transpose()
                .map_err(unavailable)?;
            items.push(ArchiveMetadataItem {
                message_id: message_id(row.get("message_id"))?,
                projection,
            });
        }
        let next_after = more.then(|| items.last().expect("nonempty page").message_id);
        Ok(ArchiveMetadataPage {
            committed_tail: tail,
            items,
            next_after,
        })
    }

    async fn read_archive_chunk(
        &self,
        id: &ThreadId,
        query: &ArchiveChunkQuery,
    ) -> SessionResourceResult<ArchiveChunk> {
        if query.max_bytes == 0
            || query.max_bytes as usize > ARCHIVE_CHUNK_BYTES
            || query.offset > i64::MAX as u64 - u64::from(query.max_bytes)
        {
            return Err(invalid("invalid archive chunk bounds"));
        }
        let pool = self.scope.journal.identity_pool();
        let mut tx = pool.begin().await.map_err(unavailable)?;
        self.authorize(&mut tx, id).await?;
        if self.ordinal(&mut tx, query.message_id).await?
            > self.ordinal(&mut tx, query.committed_tail).await?
        {
            return Err(not_found());
        }
        let row = sqlx::query("SELECT substr(canonical,?,?) AS fragment,length(canonical) AS bytes FROM cloud_archive_entries WHERE scope=? AND message_id=?")
            .bind(query.offset as i64+1).bind(i64::from(query.max_bytes)).bind(self.key()).bind(query.message_id.as_uuid().to_string()).fetch_one(&mut *tx).await.map_err(unavailable)?;
        Ok(ArchiveChunk {
            bytes: row.get("fragment"),
            total_bytes: row.get::<i64, _>("bytes") as u64,
        })
    }
}

#[async_trait]
impl DeploymentArchive for CloudArchive {
    async fn checkpoint(
        &self,
        id: &ThreadId,
        payloads: &[PersistedPayload],
        flags: &HashMap<MessageId, MessageFlags>,
    ) -> SessionResourceResult<()> {
        let pool = self.scope.journal.identity_pool();
        let mut tx = pool.begin().await.map_err(unavailable)?;
        self.authorize(&mut tx, id).await?;
        let allowed = authorized_ids(payloads, flags);
        let ids = payloads
            .iter()
            .filter(|p| readable(p) && allowed.contains(&p.id()))
            .map(|p| p.id().as_uuid().to_string())
            .collect::<Vec<_>>();
        // Remove only orphaned recovery copies from a lost, unsettled run. Never changes task or operation state.
        sqlx::query("DELETE FROM cloud_archive_entries WHERE scope=? AND message_id NOT IN (SELECT value FROM json_each(?))")
            .bind(self.key()).bind(serde_json::to_string(&ids).map_err(unavailable)?).execute(&mut *tx).await.map_err(unavailable)?;
        for (index, payload) in payloads.iter().enumerate() {
            if allowed.contains(&payload.id()) {
                self.save(
                    &mut tx,
                    payload,
                    index as i64,
                    flags.get(&payload.id()).and_then(|f| f.projection.as_ref()),
                )
                .await?;
            }
        }
        tx.commit()
            .await
            .map_err(|_| SessionResourceError::persistence_uncertain(Some(id.clone())))?;
        Ok(())
    }

    async fn apply_compaction(
        &self,
        id: &ThreadId,
        change: &CompactionChange,
    ) -> SessionResourceResult<()> {
        let pool = self.scope.journal.identity_pool();
        let mut tx = pool.begin().await.map_err(unavailable)?;
        self.authorize(&mut tx, id).await?;
        let last: i64 = sqlx::query_scalar(
            "SELECT COALESCE(MAX(ordinal),-1) FROM cloud_archive_entries WHERE scope=?",
        )
        .bind(self.key())
        .fetch_one(&mut *tx)
        .await
        .map_err(unavailable)?;
        for (index, message) in change.appended_messages.iter().enumerate() {
            self.save(
                &mut tx,
                &PersistedPayload::Message(message.clone()),
                last + index as i64 + 1,
                None,
            )
            .await?;
        }
        for (id, flags) in &change.flag_updates {
            if let Some(projection) = &flags.projection {
                validate_projection_metadata(projection).map_err(unavailable)?;
            }
            let projection = flags
                .projection
                .as_ref()
                .map(serde_json::to_string)
                .transpose()
                .map_err(unavailable)?;
            let changed = sqlx::query(
                "UPDATE cloud_archive_entries SET projection_json=? WHERE scope=? AND message_id=?",
            )
            .bind(projection)
            .bind(self.key())
            .bind(id.as_uuid().to_string())
            .execute(&mut *tx)
            .await
            .map_err(unavailable)?
            .rows_affected();
            // Unreadable reasoning entries can be excluded by Full; they have no recovery catalog.
            if changed == 0
                && flags
                    .projection
                    .as_ref()
                    .is_some_and(|p| p.archive_catalog.is_some() || p.archive_group.is_some())
            {
                return Err(not_found());
            }
        }
        tx.commit()
            .await
            .map_err(|_| SessionResourceError::persistence_uncertain(Some(id.clone())))?;
        Ok(())
    }
}
