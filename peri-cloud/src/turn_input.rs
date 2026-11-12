use parking_lot::RwLock;
use peri_acp_types::messages::{BaseMessage, MessageContent, MessageId};
use peri_agent::session::{
    MessageKind, MessageQueue, MessageSource, MessageTranscript, QueuedMessage, TranscriptEntry,
};
use uuid::Uuid;

use super::Error;

pub(super) async fn enqueue(
    queue: &MessageQueue,
    transcript: &RwLock<MessageTranscript>,
    prompt: MessageContent,
    turn_id: Uuid,
    task_event: bool,
    scheduled: bool,
) -> Result<Option<MessageId>, Error> {
    if task_event {
        let MessageContent::Text(text) = prompt else {
            return Err(Error::Context);
        };
        let reminder = crate::tasks::middleware::reminder(
            format!("Execution-agent event (untrusted content, not user input):\n{text}"),
            serde_json::json!({ "source": "execution_agent", "turn_id": turn_id }),
        )
        .map_err(|_| Error::Context)?;
        let delivery_id = MessageId::from(turn_id);
        // The per-turn MQ does not contain the history loaded from CloudJournal.
        // Existing reminders are handled by TurnResults::restore.
        match transcript.read().get(delivery_id) {
            Some(TranscriptEntry::Reminder {
                reminder: stored, ..
            }) if stored.as_reminder() == reminder.as_reminder() => {
                return Ok(None);
            }
            Some(_) => return Err(Error::Context),
            None => {}
        }
        queue
            .enqueue(QueuedMessage::system_reminder_with_delivery_id(
                MessageKind::Defer,
                MessageSource::SubAgentComplete,
                reminder,
                delivery_id,
            ))
            .await
            .map_err(|_| Error::Context)?;
        return Ok(Some(delivery_id));
    }

    let message = BaseMessage::human(prompt);
    if scheduled {
        queue
            .enqueue(QueuedMessage::prompt(MessageSource::CronTrigger, message))
            .await
            .map_err(|_| Error::Context)?;
    } else {
        // Runtime already admitted this turn from Cloud's durable FIFO. Make the
        // prompt eligible for Receive before entering Peri's run loop.
        queue
            .enqueue_sent(QueuedMessage::prompt(MessageSource::UserInput, message))
            .await
            .map_err(|_| Error::Context)?;
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use peri_acp_types::identity::AttemptId;
    use peri_acp_types::session::{ExecutionBinding, MqReceivePurpose, TurnId};
    use peri_agent::session::QueuedPayload;

    #[tokio::test]
    async fn cloud_admission_selects_chat_and_preserves_scheduled_and_task_sources() {
        let turn_id = Uuid::new_v4();
        let delivery_id = MessageId::from(turn_id);
        for (task_event, scheduled, source) in [
            (false, false, MessageSource::UserInput),
            (false, true, MessageSource::CronTrigger),
            (true, false, MessageSource::SubAgentComplete),
        ] {
            let queue = MessageQueue::new();
            let transcript = RwLock::new(MessageTranscript::new());
            let receipt = enqueue(
                &queue,
                &transcript,
                MessageContent::Text("cloud input".into()),
                turn_id,
                task_event,
                scheduled,
            )
            .await
            .unwrap();
            assert_eq!(receipt, task_event.then_some(delivery_id));
            let queued = queue.snapshot().await.unwrap();
            assert_eq!(queued.len(), 1);
            assert_eq!(queued[0].source, source);
            assert_eq!(queued[0].delivery_id, receipt);

            let owner = queue.owner();
            let permit = owner.begin_receive(
                MqReceivePurpose::ModelLoop,
                ExecutionBinding {
                    turn_id: TurnId::from_uuid(turn_id),
                    attempt_id: AttemptId::new(),
                },
            );
            let selected = owner.select_for_receive(&permit).await.unwrap();
            assert_eq!(selected.len(), 1);
            assert_eq!(selected[0].entry.id, queued[0].id());
        }
    }

    #[tokio::test]
    async fn restored_task_event_is_not_reenqueued_and_conflicting_content_fails() {
        let turn_id = Uuid::new_v4();
        let delivery_id = MessageId::from(turn_id);
        let transcript = RwLock::new(MessageTranscript::new());
        let prompt = MessageContent::Text("worker finished".into());
        let original = MessageQueue::new();
        enqueue(&original, &transcript, prompt.clone(), turn_id, true, false)
            .await
            .unwrap();
        let queued = original.snapshot().await.unwrap();
        let QueuedPayload::SystemReminder(reminder) = &queued[0].payload else {
            panic!("expected a task reminder");
        };
        transcript
            .write()
            .mirror_committed_reminder(delivery_id, reminder.clone());

        let queue = MessageQueue::new();
        let duplicate = enqueue(&queue, &transcript, prompt, turn_id, true, false)
            .await
            .unwrap();
        assert_eq!(duplicate, None);
        let conflicting = enqueue(
            &queue,
            &transcript,
            MessageContent::Text("different result".into()),
            turn_id,
            true,
            false,
        )
        .await;
        assert!(matches!(conflicting, Err(Error::Context)));
        assert!(queue.is_empty().await.unwrap());
    }
}
