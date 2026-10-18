use super::*;
use sqlx::Row;
use uuid::Uuid;

/// Trusted cloud admission metadata; never accepted as model tool arguments.
#[derive(Clone)]
pub(crate) struct ScheduledAdmission {
    pub occurrence_id: Uuid,
    pub request_hash: String,
    pub route_json: String,
    pub admitted_at: i64,
    pub expires_at: i64,
    pub daily_limit: u32,
}
impl CloudJournal {
    /// Query the original session before resolving the route's current session.
    pub(crate) async fn scheduled_receipt(
        &self,
        owner: Uuid,
        id: Uuid,
        hash: &str,
    ) -> StateResult<Option<TurnRecord>> {
        let row=sqlx::query("SELECT a.request_hash,t.record_json FROM scheduled_admissions a JOIN cloud_turns t ON t.id=a.turn_id JOIN cloud_sessions s ON s.id=t.session_id AND s.principal=a.owner WHERE a.owner=? AND a.occurrence_id=?")
            .bind(owner.to_string()).bind(id.to_string()).fetch_optional(&self.identity_pool()).await?;
        row.map(|row| {
            if row.get::<String, _>("request_hash") != hash {
                return Err(StateError::InvocationConflict);
            }
            Ok(serde_json::from_str(row.get("record_json"))?)
        })
        .transpose()
    }
}
