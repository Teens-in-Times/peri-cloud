//! Shared human/assistant projection keyed by logical session, independent of adapter routes.
use super::{SessionState, StateResult};
use peri_acp_types::messages::{BaseMessage, MessageContent};
use sqlx::{Sqlite, Transaction};

pub(crate) async fn seed(
    tx: &mut Transaction<'_, Sqlite>,
    session: &SessionState,
) -> StateResult<()> {
    let id = session.frozen.session_id.to_string();
    let inserted =
        sqlx::query("INSERT OR IGNORE INTO conversation_projections(session_id) VALUES(?)")
            .bind(&id)
            .execute(&mut **tx)
            .await?
            .rows_affected();
    if inserted == 0 {
        return Ok(());
    }
    for payload in &session.history {
        if session
            .history_flags
            .get(&payload.id())
            .is_some_and(|f| f.excluded)
        {
            continue;
        }
        if let Some(message @ BaseMessage::Human { .. }) = payload.as_message() {
            let text = message.content();
            append(
                tx,
                session,
                &format!("conversation-history:{}", payload.id().as_uuid()),
                "history_input",
                &text,
                "accepted",
            )
            .await?;
        } else {
            for reply in crate::reply::replies(std::slice::from_ref(payload)) {
                assistant(tx, session, &reply).await?;
            }
        }
    }
    Ok(())
}
pub(crate) async fn user(
    tx: &mut Transaction<'_, Sqlite>,
    session: &SessionState,
    key: &str,
    prompt: &MessageContent,
) -> StateResult<()> {
    seed(tx, session).await?;
    if let MessageContent::Text(text) = prompt {
        append(
            tx,
            session,
            &format!("conversation-user:{key}"),
            "user",
            text,
            "accepted",
        )
        .await?;
    }
    Ok(())
}
pub(crate) async fn assistant(
    tx: &mut Transaction<'_, Sqlite>,
    session: &SessionState,
    reply: &crate::ChatReply,
) -> StateResult<()> {
    append(
        tx,
        session,
        &format!("conversation-ai:{}", reply.message_id),
        "assistant",
        &reply.text,
        "accepted",
    )
    .await
}
pub(crate) async fn notice(
    tx: &mut Transaction<'_, Sqlite>,
    session: &SessionState,
    key: &str,
    text: &str,
) -> StateResult<()> {
    seed(tx, session).await?;
    append(
        tx,
        session,
        &format!("conversation-notice:{key}"),
        "notice",
        text,
        "accepted",
    )
    .await
}
async fn append(
    tx: &mut Transaction<'_, Sqlite>,
    session: &SessionState,
    key: &str,
    role: &str,
    text: &str,
    state: &str,
) -> StateResult<()> {
    if text.is_empty() {
        return Ok(());
    }
    sqlx::query("INSERT OR IGNORE INTO web_messages(route_key,principal,event_key,role,text,state,created_at) VALUES(?,?,?,?,?,?,?)")
        .bind(format!("session:{}",session.frozen.session_id)).bind(session.frozen.principal_id.to_string()).bind(key).bind(role).bind(text).bind(state).bind(crate::tasks::now()).execute(&mut **tx).await?;
    Ok(())
}
