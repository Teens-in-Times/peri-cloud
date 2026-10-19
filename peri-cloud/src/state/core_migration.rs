use super::{StateError, StateResult};
use sqlx::SqlitePool;

pub(super) async fn migrate(pool: &SqlitePool) -> StateResult<()> {
    let version: i64 = sqlx::query_scalar("PRAGMA user_version")
        .fetch_one(pool)
        .await?;
    if version >= 16 {
        return Ok(());
    }
    if version != 15 {
        return Err(StateError::Version);
    }
    let mut tx = pool.begin().await?;
    for sql in [
        "CREATE TABLE gateway_queue (seq INTEGER PRIMARY KEY AUTOINCREMENT,event_key TEXT NOT NULL UNIQUE REFERENCES gateway_inbox(event_key),principal TEXT NOT NULL REFERENCES principals(id),session_id TEXT NOT NULL REFERENCES cloud_sessions(id),message_json TEXT NOT NULL,source TEXT NOT NULL,reply_reference TEXT NOT NULL,reply_kind TEXT NOT NULL,state TEXT NOT NULL)",
        "CREATE INDEX gateway_queue_fifo ON gateway_queue(session_id,state,seq)",
        "CREATE TABLE agent_tasks (id TEXT PRIMARY KEY,principal TEXT NOT NULL REFERENCES principals(id),parent_session TEXT NOT NULL REFERENCES cloud_sessions(id),worker_session TEXT NOT NULL UNIQUE REFERENCES cloud_sessions(id),request_key TEXT NOT NULL,record_json TEXT NOT NULL,UNIQUE(parent_session,request_key))",
        "CREATE INDEX agent_tasks_owner ON agent_tasks(principal,parent_session)",
        "CREATE TABLE agent_task_messages (seq INTEGER PRIMARY KEY AUTOINCREMENT,task_id TEXT NOT NULL REFERENCES agent_tasks(id),request_key TEXT NOT NULL,direction TEXT NOT NULL,body TEXT NOT NULL,state TEXT NOT NULL,UNIQUE(task_id,request_key))",
        "CREATE TABLE agent_task_trace (seq INTEGER PRIMARY KEY AUTOINCREMENT,task_id TEXT NOT NULL REFERENCES agent_tasks(id),event_json TEXT NOT NULL)",
        "CREATE TABLE agent_device_leases (device_id TEXT PRIMARY KEY REFERENCES identity_devices(id),principal TEXT NOT NULL REFERENCES principals(id),owner_session TEXT NOT NULL REFERENCES cloud_sessions(id),phase TEXT NOT NULL)",
        "CREATE TABLE conversation_projections (session_id TEXT PRIMARY KEY REFERENCES cloud_sessions(id))",
        "PRAGMA user_version=16",
    ] { sqlx::query(sql).execute(&mut *tx).await?; }
    tx.commit().await?;
    Ok(())
}
