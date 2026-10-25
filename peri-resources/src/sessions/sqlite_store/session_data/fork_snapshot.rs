//! Fork 快照的事务写入；父会话与原证据不改写。
use super::*;

impl SqliteSessionData {
    pub(super) async fn save_fork_snapshot(
        &self,
        fork: &ForkSnapshot,
    ) -> SessionResourceResult<()> {
        self.writable()?;
        if fork.target.thread_id == fork.source_id {
            return Err(invalid_input("fork target must differ from its source"));
        }
        let snapshot_at = fork
            .target
            .meta
            .snapshot_at_message_id
            .map(|id| id.as_uuid().to_string());
        let mut tx = self
            .database
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(|error| map_sqlx(&error))?;
        self.require_session(&mut tx, &fork.source_id).await?;
        if !thread_exists_on(&mut tx, &fork.source_id)
            .await
            .map_err(read_failure)?
        {
            return Err(not_found());
        }
        insert_thread_row(
            &mut tx,
            &new_session_row(
                &fork.target,
                snapshot_at.as_deref(),
                Some(fork.target.frozen.as_str()),
                fork.payloads.len() as i64,
            ),
        )
        .await
        .map_err(write_failure)?;
        insert_binding_row(&mut tx, &fork.target.thread_id, &fork.target.binding)
            .await
            .map_err(write_failure)?;
        let payload_ids = insert_history_rows(&mut tx, &fork.target.thread_id, &fork.payloads)
            .await
            .map_err(write_failure)?;
        for (message_id, flags) in &fork.flags {
            if !payload_ids.contains(message_id) {
                return Err(invalid_input("fork flags reference an unknown message id"));
            }
            let updated = sqlx::query(
                "UPDATE messages SET truncated = ?1, excluded = ?2, projection = ?3
                 WHERE message_id = ?4 AND thread_id = ?5",
            )
            .bind(flags.truncated)
            .bind(flags.excluded)
            .bind(projection_json(flags)?)
            .bind(message_id.as_uuid().to_string())
            .bind(fork.target.thread_id.as_str())
            .execute(&mut *tx)
            .await
            .map_err(|error| map_sqlx(&error))?;
            if updated.rows_affected() != 1 {
                return Err(corrupt("fork projection did not apply to exactly one row"));
            }
        }
        tx.commit()
            .await
            .map_err(|_| commit_failure(Some(fork.target.thread_id.clone())))?;
        Ok(())
    }
}
