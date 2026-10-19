//! Conversation rounds, not model iterations, drive periodic task refresh.
use super::*;

pub(super) async fn periodic_due(
    pool: &sqlx::SqlitePool,
    scope: &TaskScope,
    turn: Uuid,
    previous: Option<i64>,
    interval: u32,
) -> Result<(bool, i64)> {
    let current:Option<i64>=sqlx::query_scalar("SELECT t.rowid FROM cloud_turns t JOIN cloud_sessions s ON s.id=t.session_id WHERE t.id=? AND t.session_id=? AND s.principal=?")
        .bind(turn.to_string()).bind(scope.session.to_string()).bind(scope.principal.to_string()).fetch_optional(pool).await?;
    let current = current.ok_or(TaskError::Forbidden)?;
    let Some(previous) = previous else {
        return Ok((true, current));
    };
    // Global rowids are just stable cursors. Count this user's conversation only.
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM cloud_turns WHERE session_id=? AND rowid>? AND rowid<=?",
    )
    .bind(scope.session.to_string())
    .bind(previous)
    .bind(current)
    .fetch_one(pool)
    .await?;
    Ok((count >= i64::from(interval.clamp(1, 128)), current))
}
