use super::*;
use crate::state::CloudJournal;
use sha2::{Digest, Sha256};
use sqlx::{sqlite::SqliteRow, Row, SqliteConnection, SqlitePool};

/// The authenticated caller captures the owner and capacity once. No request
/// payload can override them. All indexes and receipts share the fact transaction.
#[derive(Clone)]
pub struct MemoryStore {
    pool: SqlitePool,
    owner: Uuid,
    max_bytes: usize,
    scope: MemoryScope,
    management: bool,
}
impl MemoryStore {
    pub fn new(journal: &CloudJournal, owner: Uuid, max_bytes: usize) -> Self {
        Self {
            pool: journal.identity_pool(),
            owner,
            max_bytes,
            scope: MemoryScope::User,
            management: false,
        }
    }

    pub fn scoped(mut self, scope: MemoryScope) -> Self {
        self.scope = scope;
        self
    }
    /// Human entry points only. Never exposes revoked facts to model tools or snapshots.
    pub(crate) fn for_management(mut self) -> Self {
        self.management = true;
        self
    }
    pub async fn privacy_epoch(&self) -> MemoryResult<u64> {
        let epoch: Option<i64> =
            sqlx::query_scalar("SELECT privacy_epoch FROM memory_accounts WHERE owner=?")
                .bind(self.owner.to_string())
                .fetch_optional(&self.pool)
                .await?;
        Ok(epoch.unwrap_or(0) as u64)
    }
    pub async fn next_expiry(&self) -> MemoryResult<Option<i64>> {
        Ok(sqlx::query_scalar("SELECT MIN(valid_until) FROM memory_items WHERE owner=? AND deleted=0 AND valid_until>?")
            .bind(self.owner.to_string()).bind(now()).fetch_one(&self.pool).await?)
    }

    pub async fn list(&self, query: &str, offset: usize, limit: usize) -> MemoryResult<MemoryPage> {
        self.page(query, offset, limit, false).await
    }
    pub async fn inventory(
        &self,
        query: &str,
        offset: usize,
        limit: usize,
    ) -> MemoryResult<MemoryPage> {
        self.page(query, offset, limit, true).await
    }
    async fn page(
        &self,
        query: &str,
        offset: usize,
        limit: usize,
        include_expired: bool,
    ) -> MemoryResult<MemoryPage> {
        if query.len() > MAX_QUERY_BYTES || offset > MAX_ITEMS || !(1..=20).contains(&limit) {
            return Err(MemoryError::Invalid);
        }
        let mut tx = self.pool.begin().await?;
        self.scope
            .authorize_management(&mut tx, self.owner, include_expired && self.management)
            .await?;
        let (generation, total, used_bytes) = self.stats(&mut tx).await?;
        let rows = sqlx::query("SELECT * FROM memory_items WHERE owner=? AND scope=? AND deleted=0 AND (? OR valid_until IS NULL OR valid_until>?) ORDER BY pinned DESC,updated_at DESC,id")
            .bind(self.owner.to_string()).bind(self.scope.key()).bind(include_expired).bind(now()).fetch_all(&mut *tx).await?;
        let query = query.trim().to_lowercase();
        let expression = search::expression(&query);
        let hits: Vec<String> = if expression.is_empty() {
            Vec::new()
        } else {
            sqlx::query_scalar(
                "SELECT item_id FROM memory_fts WHERE owner=? AND memory_fts MATCH ?",
            )
            .bind(self.owner.to_string())
            .bind(expression)
            .fetch_all(&mut *tx)
            .await?
        };
        let query_terms = search::terms(&query);
        let mut matches = Vec::new();
        for row in rows {
            let text: String = row.get("search_text");
            let exact = text.contains(&query);
            if query.is_empty() || exact || hits.contains(&row.get::<String, _>("id")) {
                let score = usize::from(exact) * 10000
                    + query_terms
                        .iter()
                        .filter(|t| text.contains(t.as_str()))
                        .count();
                matches.push((score, item(&row)?));
            }
        }
        if !query.is_empty() {
            matches.sort_by_key(|entry| std::cmp::Reverse(entry.0));
        }
        let match_count = matches.len();
        let mut items = Vec::new();
        let mut bytes = 0;
        for (_, item) in matches.into_iter().skip(offset).take(limit) {
            let size = serde_json::to_vec(&item)?.len();
            if bytes + size > RESULT_BYTES {
                break;
            }
            bytes += size;
            items.push(item);
        }
        let next = offset + items.len();
        let has_more = next < match_count;
        tx.commit().await?;
        Ok(MemoryPage {
            generation,
            total,
            used_bytes,
            items,
            has_more,
            next_offset: has_more.then_some(next),
        })
    }

    pub async fn get(&self, id: Uuid) -> MemoryResult<MemoryItem> {
        self.read(id, false).await
    }
    pub async fn inspect(&self, id: Uuid) -> MemoryResult<MemoryItem> {
        self.read(id, true).await
    }
    async fn read(&self, id: Uuid, include_expired: bool) -> MemoryResult<MemoryItem> {
        let mut tx = self.pool.begin().await?;
        self.scope
            .authorize_management(&mut tx, self.owner, include_expired && self.management)
            .await?;
        let row = sqlx::query("SELECT * FROM memory_items WHERE owner=? AND scope=? AND id=? AND deleted=0 AND (? OR valid_until IS NULL OR valid_until>?)")
            .bind(self.owner.to_string()).bind(self.scope.key()).bind(id.to_string()).bind(include_expired).bind(now()).fetch_optional(&mut *tx).await?
            .ok_or(MemoryError::NotFound)?;
        item(&row)
    }

    pub async fn add(
        &self,
        id: Uuid,
        expected_generation: u64,
        value: MemoryContent,
        source: MemorySource,
    ) -> MemoryResult<MemoryItem> {
        if expected_generation >= i64::MAX as u64 {
            return Err(MemoryError::Invalid);
        }
        validate(&value)?;
        let hash = format!(
            "{:x}",
            Sha256::digest(serde_json::to_vec(&(
                expected_generation,
                &value,
                &source,
                &self.scope
            ))?)
        );
        let mut tx = self.pool.begin().await?;
        self.scope.authorize(&mut tx, self.owner).await?;
        self.lock(&mut tx).await?;
        if let Some(row) = sqlx::query("SELECT * FROM memory_items WHERE id=?")
            .bind(id.to_string())
            .fetch_optional(&mut *tx)
            .await?
        {
            if row.get::<String, _>("owner") != self.owner.to_string()
                || row.get::<String, _>("scope") != self.scope.key()
                || row.get::<String, _>("create_hash") != hash
                || row.get::<bool, _>("deleted")
            {
                return Err(MemoryError::Conflict);
            }
            return item(&row);
        }
        let (generation, _, _) = self.stats(&mut tx).await?;
        if generation != expected_generation {
            return Err(MemoryError::Conflict);
        }
        if let Some(row) = sqlx::query("SELECT * FROM memory_items WHERE owner=? AND scope=? AND search_text=? AND deleted=0 AND (valid_until IS NULL OR valid_until>?)")
            .bind(self.owner.to_string()).bind(self.scope.key()).bind(value.content.to_lowercase()).bind(now()).fetch_optional(&mut *tx).await? {
            let existing=item(&row)?;
            if existing.value!=value { return Err(MemoryError::Conflict); }
            return Ok(existing);
        }
        let at = now();
        sqlx::query("INSERT INTO memory_items(id,owner,revision,content,kind,pinned,valid_until,source_json,created_at,updated_at,create_hash,search_text,scope) VALUES(?,?,1,?,?,?,?,?,?,?,?,?,?)")
            .bind(id.to_string()).bind(self.owner.to_string()).bind(&value.content).bind(kind(value.kind)).bind(value.pinned).bind(value.valid_until)
            .bind(serde_json::to_string(&source)?).bind(at).bind(at).bind(hash).bind(value.content.to_lowercase()).bind(self.scope.key()).execute(&mut *tx).await?;
        self.capacity(&mut tx).await?;
        self.index(&mut tx, id, &value.content).await?;
        self.audit(&mut tx, id, 1, "add", &source).await?;
        tx.commit().await?;
        self.get(id).await
    }

    pub async fn update(
        &self,
        id: Uuid,
        revision: u64,
        value: MemoryContent,
        source: MemorySource,
    ) -> MemoryResult<MemoryItem> {
        validate(&value)?;
        revision_valid(revision)?;
        let mut tx = self.pool.begin().await?;
        self.scope.authorize(&mut tx, self.owner).await?;
        self.lock(&mut tx).await?;
        let changed = sqlx::query("UPDATE memory_items SET revision=revision+1,content=?,kind=?,pinned=?,valid_until=?,source_json=?,updated_at=?,search_text=? WHERE id=? AND owner=? AND scope=? AND revision=? AND deleted=0")
            .bind(&value.content).bind(kind(value.kind)).bind(value.pinned).bind(value.valid_until).bind(serde_json::to_string(&source)?).bind(now())
            .bind(value.content.to_lowercase()).bind(id.to_string()).bind(self.owner.to_string()).bind(self.scope.key()).bind(revision as i64).execute(&mut *tx).await?.rows_affected();
        if changed != 1 {
            return Err(MemoryError::Conflict);
        }
        self.capacity(&mut tx).await?;
        self.index(&mut tx, id, &value.content).await?;
        self.audit(&mut tx, id, revision + 1, "update", &source)
            .await?;
        tx.commit().await?;
        self.get(id).await
    }

    pub async fn forget(&self, id: Uuid, revision: u64, source: MemorySource) -> MemoryResult<()> {
        revision_valid(revision)?;
        let mut tx = self.pool.begin().await?;
        self.scope
            .authorize_management(&mut tx, self.owner, self.management)
            .await?;
        self.lock(&mut tx).await?;
        let changed = sqlx::query("UPDATE memory_items SET revision=revision+1,deleted=1,content='',search_text='',pinned=0,source_json=?,updated_at=? WHERE id=? AND owner=? AND scope=? AND revision=? AND deleted=0")
            .bind(serde_json::to_string(&source)?).bind(now()).bind(id.to_string()).bind(self.owner.to_string()).bind(self.scope.key()).bind(revision as i64).execute(&mut *tx).await?.rows_affected();
        if changed != 1 {
            return Err(MemoryError::Conflict);
        }
        sqlx::query("DELETE FROM memory_fts WHERE owner=? AND item_id=?")
            .bind(self.owner.to_string())
            .bind(id.to_string())
            .execute(&mut *tx)
            .await?;
        // No revision history stores a body. Retain only ownership, source IDs,
        // revision and mutation receipt so stale operations cannot revive it.
        self.audit(&mut tx, id, revision + 1, "forget", &source)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn snapshot(&self) -> MemoryResult<MemorySnapshot> {
        let mut tx = self.pool.begin().await?;
        self.scope.authorize(&mut tx, self.owner).await?;
        let (generation, _, _) = self.stats(&mut tx).await?;
        let rows = sqlx::query("SELECT * FROM memory_items WHERE owner=? AND scope=? AND deleted=0 AND pinned=1 AND (valid_until IS NULL OR valid_until>?) ORDER BY updated_at DESC,id")
            .bind(self.owner.to_string()).bind(self.scope.key()).bind(now()).fetch_all(&mut *tx).await?;
        let total = rows.len();
        let mut items = Vec::new();
        let mut bytes = 0;
        let mut chars = 0;
        for row in rows {
            let item = item(&row)?;
            let size = serde_json::to_vec(&item)?.len();
            let count = item.value.content.chars().count();
            if bytes + size > SNAPSHOT_BYTES || chars + count > self.scope.chars() {
                continue;
            }
            bytes += size;
            chars += count;
            items.push(item);
        }
        tx.commit().await?;
        Ok(MemorySnapshot {
            generation,
            omitted: total - items.len(),
            items,
        })
    }

    /// Rebuild only from authoritative, active facts. Tombstones never index.
    pub async fn rebuild_index(&self) -> MemoryResult<()> {
        let mut tx = self.pool.begin().await?;
        self.scope.authorize(&mut tx, self.owner).await?;
        self.lock(&mut tx).await?;
        sqlx::query("DELETE FROM memory_fts WHERE owner=?")
            .bind(self.owner.to_string())
            .execute(&mut *tx)
            .await?;
        let rows = sqlx::query("SELECT id,content FROM memory_items WHERE owner=? AND deleted=0 AND (valid_until IS NULL OR valid_until>?)")
            .bind(self.owner.to_string()).bind(now()).fetch_all(&mut *tx).await?;
        for row in rows {
            self.index(
                &mut tx,
                Uuid::parse_str(row.get("id")).map_err(|_| MemoryError::Invalid)?,
                row.get("content"),
            )
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    async fn lock(&self, conn: &mut SqliteConnection) -> MemoryResult<()> {
        sqlx::query("INSERT INTO memory_accounts(owner,generation) VALUES(?,0) ON CONFLICT(owner) DO UPDATE SET generation=generation")
            .bind(self.owner.to_string()).execute(conn).await?;
        Ok(())
    }
    async fn stats(&self, conn: &mut SqliteConnection) -> MemoryResult<(u64, usize, usize)> {
        let generation: Option<i64> =
            sqlx::query_scalar("SELECT generation FROM memory_accounts WHERE owner=?")
                .bind(self.owner.to_string())
                .fetch_optional(&mut *conn)
                .await?;
        let (count, bytes): (i64, i64) = sqlx::query_as("SELECT COUNT(*),COALESCE(SUM(length(CAST(content AS BLOB))),0) FROM memory_items WHERE owner=? AND deleted=0")
            .bind(self.owner.to_string()).fetch_one(conn).await?;
        Ok((
            generation.unwrap_or(0) as u64,
            count as usize,
            bytes as usize,
        ))
    }
    async fn capacity(&self, conn: &mut SqliteConnection) -> MemoryResult<()> {
        let (_, count, bytes) = self.stats(conn).await?;
        if count > MAX_ITEMS || bytes > self.max_bytes {
            return Err(MemoryError::Limit);
        }
        Ok(())
    }
    async fn index(
        &self,
        conn: &mut SqliteConnection,
        id: Uuid,
        content: &str,
    ) -> MemoryResult<()> {
        sqlx::query("DELETE FROM memory_fts WHERE item_id=? AND owner=?")
            .bind(id.to_string())
            .bind(self.owner.to_string())
            .execute(&mut *conn)
            .await?;
        sqlx::query("INSERT INTO memory_fts(item_id,owner,terms) VALUES(?,?,?)")
            .bind(id.to_string())
            .bind(self.owner.to_string())
            .bind(search::terms(content).join(" "))
            .execute(conn)
            .await?;
        Ok(())
    }
    async fn audit(
        &self,
        conn: &mut SqliteConnection,
        id: Uuid,
        revision: u64,
        action: &str,
        source: &MemorySource,
    ) -> MemoryResult<()> {
        sqlx::query("UPDATE memory_accounts SET generation=generation+1 WHERE owner=?")
            .bind(self.owner.to_string())
            .execute(&mut *conn)
            .await?;
        if matches!(action, "update" | "forget") {
            sqlx::query("UPDATE memory_accounts SET privacy_epoch=privacy_epoch+1 WHERE owner=?")
                .bind(self.owner.to_string())
                .execute(&mut *conn)
                .await?;
        }
        sqlx::query("INSERT INTO memory_audit(owner,item_id,revision,action,source_json,created_at) VALUES(?,?,?,?,?,?)")
            .bind(self.owner.to_string()).bind(id.to_string()).bind(revision as i64).bind(action).bind(serde_json::to_string(source)?).bind(now()).execute(conn).await?;
        Ok(())
    }
}

fn revision_valid(revision: u64) -> MemoryResult<()> {
    if revision == 0 || revision >= i64::MAX as u64 {
        return Err(MemoryError::Invalid);
    }
    Ok(())
}
fn validate(value: &MemoryContent) -> MemoryResult<()> {
    if value.content.trim().is_empty()
        || value.content.contains('\0')
        || value.valid_until.is_some_and(|at| at <= now())
    {
        return Err(MemoryError::Invalid);
    }
    if value.content.len() > MAX_ITEM_BYTES {
        return Err(MemoryError::Limit);
    }
    Ok(())
}
fn kind(kind: MemoryKind) -> &'static str {
    match kind {
        MemoryKind::Fact => "fact",
        MemoryKind::Decision => "decision",
    }
}
fn item(row: &SqliteRow) -> MemoryResult<MemoryItem> {
    Ok(MemoryItem {
        id: Uuid::parse_str(row.get("id")).map_err(|_| MemoryError::Invalid)?,
        revision: row.get::<i64, _>("revision") as u64,
        scope: MemoryScope::from_key(row.get("scope"))?,
        value: MemoryContent {
            content: row.get("content"),
            kind: match row.get::<&str, _>("kind") {
                "fact" => MemoryKind::Fact,
                "decision" => MemoryKind::Decision,
                _ => return Err(MemoryError::Invalid),
            },
            pinned: row.get("pinned"),
            valid_until: row.get("valid_until"),
        },
        source: serde_json::from_str(row.get("source_json"))?,
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
    })
}
