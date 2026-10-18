//! Persist audience independently of runtime caches, including resumed history.
use crate::state::{CloudJournal, StateResult};
use uuid::Uuid;

pub(crate) async fn can_attach(
    journal: &CloudJournal,
    principal: Uuid,
    session: Uuid,
    private: bool,
) -> StateResult<bool> {
    if private {
        return Ok(true);
    }
    let pool = journal.identity_pool();
    let (exists,): (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM cloud_sessions WHERE id=? AND principal=?")
            .bind(session.to_string())
            .bind(principal.to_string())
            .fetch_one(&pool)
            .await?;
    if exists == 0 {
        return Ok(true);
    }
    let visibility: Option<bool> =
        sqlx::query_scalar("SELECT private_memory FROM memory_session_audience WHERE session_id=?")
            .bind(session.to_string())
            .fetch_optional(&pool)
            .await?;
    // Legacy history may already contain personal facts. Unknown is private.
    Ok(visibility == Some(false))
}
pub(crate) async fn record(
    journal: &CloudJournal,
    session: Uuid,
    private: bool,
) -> StateResult<()> {
    sqlx::query("INSERT INTO memory_session_audience(session_id,private_memory) VALUES(?,?) ON CONFLICT(session_id) DO UPDATE SET private_memory=MAX(private_memory,excluded.private_memory)")
        .bind(session.to_string()).bind(private).execute(&journal.identity_pool()).await?;
    Ok(())
}
