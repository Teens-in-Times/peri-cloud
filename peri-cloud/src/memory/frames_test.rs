use super::frames::MemoryFrames;
use super::tests::{fixture, value};
use super::*;
use crate::{personal::PersonalScope, state::FrozenSession};
use parking_lot::RwLock;
use peri_acp_types::{messages::BaseMessage, permission::PermissionMode, store::MessageFlags};
use peri_agent::session::MessageTranscript;
use std::sync::Arc;

async fn setup() -> (tempfile::TempDir, PersonalScope, MemoryFrames) {
    let (root, journal, a, _) = fixture().await;
    let journal = Arc::new(journal);
    let session_id = Uuid::new_v4();
    journal
        .bind(
            FrozenSession {
                session_id,
                principal_id: a,
                device_id: None,
                binding: None,
                system_prompt: "stable rules".into(),
                context_window: 128000,
            },
            PermissionMode::Default,
        )
        .await
        .unwrap();
    let scope = PersonalScope {
        session_id,
        turn_id: Uuid::new_v4(),
        device_id: None,
        memory_fence: Default::default(),
        model_revision: "fixture-model".into(),
        private_memory: true,
        scheduled: false,
        schedule_push: false,
        journal,
        principal: a,
        max_bytes: 65536,
    };
    let frames = MemoryFrames {
        scope: scope.clone(),
        transcript: Arc::new(RwLock::new(MessageTranscript::new())),
    };
    (root, scope, frames)
}
fn view(frames: &MemoryFrames) -> String {
    serde_json::to_string(&frames.transcript.read().visible_model_messages().unwrap()).unwrap()
}

#[tokio::test]
async fn frames_reuse_stable_ids_and_reissue_after_compact_without_changing_audit() {
    let (_root, scope, frames) = setup().await;
    let store = MemoryStore::new(&scope.journal, scope.principal, 65536);
    let mut fact = value("PINNED_MARKER");
    fact.pinned = true;
    store
        .add(Uuid::new_v4(), 0, fact, MemorySource::web())
        .await
        .unwrap();
    frames.refresh().await.unwrap();
    let first = frames.transcript.read().canonical_payloads();
    frames.refresh().await.unwrap();
    assert_eq!(
        frames.transcript.read().canonical_payloads().len(),
        first.len()
    );
    assert!(view(&frames).contains("PINNED_MARKER"));
    frames.transcript.write().set_flags_batch(
        first
            .iter()
            .map(|p| {
                (
                    p.id(),
                    MessageFlags {
                        excluded: true,
                        ..Default::default()
                    },
                )
            })
            .collect(),
    );
    frames
        .transcript
        .write()
        .append(BaseMessage::human("compact summary"));
    frames.refresh().await.unwrap();
    let second = frames.transcript.read().canonical_payloads();
    assert_eq!(second.len(), first.len() + 2);
    assert_ne!(first[0].id(), second.last().unwrap().id());
    assert!(view(&frames).contains("PINNED_MARKER"));
    assert!(view(&frames).contains("compact summary"));
    assert!(!first[0].displayable_to_client());
    assert!(second.last().unwrap().as_message().is_none());
}

#[tokio::test]
async fn forget_cuts_off_tool_recollections_summaries_and_survives_rewind() {
    let (_root, scope, frames) = setup().await;
    let store = MemoryStore::new(&scope.journal, scope.principal, 65536);
    let item = store
        .add(
            Uuid::new_v4(),
            0,
            value("FORGOTTEN_MARKER"),
            MemorySource::web(),
        )
        .await
        .unwrap();
    frames.refresh().await.unwrap();
    frames
        .transcript
        .write()
        .append(BaseMessage::human("user mentioned FORGOTTEN_MARKER"));
    frames.transcript.write().append(BaseMessage::human(
        "summary: tool result contained FORGOTTEN_MARKER",
    ));
    store.forget(item.id, 1, MemorySource::web()).await.unwrap();
    frames.refresh().await.unwrap();
    assert!(!view(&frames).contains("FORGOTTEN_MARKER"));
    let history = frames.transcript.read().canonical_payloads();
    assert!(history.iter().any(|p| p.as_message().is_some()));
    // A generic rewind cannot clear the independent privacy cutoff.
    frames.transcript.write().set_flags_batch(
        history
            .iter()
            .map(|p| (p.id(), MessageFlags::default()))
            .collect(),
    );
    frames.refresh().await.unwrap();
    assert!(!view(&frames).contains("FORGOTTEN_MARKER"));
    assert_eq!(store.privacy_epoch().await.unwrap(), 1);
}

#[tokio::test]
async fn device_catalog_omits_transport_secrets_and_revocation_invalidates_old_view() {
    let (_root, scope, frames) = setup().await;
    let d = super::scope_test::device(&scope.journal, scope.principal).await;
    frames.refresh().await.unwrap();
    let original = view(&frames);
    assert!(original.contains("unverified"));
    assert!(!original.contains(&d.default_workspace));
    assert!(!original.contains(&d.connection_id.unwrap().to_string()));
    frames
        .transcript
        .write()
        .append(BaseMessage::human("device facts quoted from TOOL_MARKER"));
    let mut d = d;
    d.revoked = true;
    sqlx::query("UPDATE identity_devices SET record_json=? WHERE id=?")
        .bind(serde_json::to_string(&d).unwrap())
        .bind(d.id.to_string())
        .execute(&scope.journal.identity_pool())
        .await
        .unwrap();
    frames.refresh().await.unwrap();
    let current = view(&frames);
    assert!(!current.contains("TOOL_MARKER"));
    assert!(!current.contains(&d.id.to_string()));
}

#[tokio::test]
async fn dispatch_guard_rejects_a_request_prepared_before_a_concurrent_forget() {
    use peri_model::{
        Model, ModelCapabilities, ModelError, ModelRequest, ModelResult, ModelStream,
        ProtocolErrorKind,
    };
    struct Probe(Arc<std::sync::atomic::AtomicUsize>);
    #[async_trait::async_trait]
    impl Model for Probe {
        fn capabilities(&self) -> ModelCapabilities {
            ModelCapabilities::default()
        }
        async fn stream(
            &self,
            _: ModelRequest,
            _: tokio_util::sync::CancellationToken,
        ) -> ModelResult<ModelStream> {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Err(ModelError::protocol(ProtocolErrorKind::Provider))
        }
    }
    let (_root, scope, frames) = setup().await;
    let store = MemoryStore::new(&scope.journal, scope.principal, 65536);
    let item = store
        .add(
            Uuid::new_v4(),
            0,
            value("GUARD_SECRET"),
            MemorySource::web(),
        )
        .await
        .unwrap();
    frames.refresh().await.unwrap();
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let model = super::frames::GuardedModel {
        inner: Arc::new(Probe(calls.clone())),
        scope,
    };
    let request = ModelRequest::new(vec![peri_model::ModelMessage::user_text(
        "old summary GUARD_SECRET",
    )]);
    store.forget(item.id, 1, MemorySource::web()).await.unwrap();
    assert!(model
        .stream(request, tokio_util::sync::CancellationToken::new())
        .await
        .is_err());
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 0);
}

#[tokio::test]
async fn restored_frame_is_in_both_budget_observation_and_actual_dispatch() {
    use peri_model::{
        Model, ModelError, ModelRequest, ModelResult, ModelStream, ProtocolErrorKind,
    };
    struct Probe {
        provider: peri_model::OpenAiModel,
        actual: Arc<parking_lot::Mutex<Option<serde_json::Value>>>,
    }
    #[async_trait::async_trait]
    impl Model for Probe {
        fn capabilities(&self) -> peri_model::ModelCapabilities {
            self.provider.capabilities()
        }
        fn prepare_request(
            &self,
            r: &ModelRequest,
        ) -> ModelResult<peri_model::PreparedModelRequest> {
            self.provider.prepare_request(r)
        }
        async fn stream(
            &self,
            r: ModelRequest,
            _: tokio_util::sync::CancellationToken,
        ) -> ModelResult<ModelStream> {
            *self.actual.lock() = Some(self.prepare_request(&r)?.body().as_value().clone());
            Err(ModelError::protocol(ProtocolErrorKind::Provider))
        }
    }
    let (_root, scope, frames) = setup().await;
    frames.refresh().await.unwrap();
    let actual = Arc::new(parking_lot::Mutex::new(None));
    let model = super::frames::GuardedModel {
        scope,
        inner: Arc::new(Probe {
            provider: peri_model::OpenAiModel::new(peri_model::OpenAiConfig::new(
                "http://127.0.0.1:1/v1".parse().unwrap(),
                "fixture-observation-key",
                "fixture-model",
            )),
            actual: actual.clone(),
        }),
    };
    let request = ModelRequest::new(vec![peri_model::ModelMessage::user_text(
        "summary without the original frame",
    )])
    .with_tools(vec![peri_model::ToolDefinition::new(
        "SearchMemory",
        peri_model::JsonObject::default(),
    )]);
    let observed = model
        .prepare_request(&request)
        .unwrap()
        .body()
        .as_value()
        .clone();
    assert!(observed.to_string().contains("context_frame"));
    assert!(model
        .stream(request, tokio_util::sync::CancellationToken::new())
        .await
        .is_err());
    assert_eq!(actual.lock().as_ref(), Some(&observed));
}

#[tokio::test]
async fn model_change_reissues_full_frame_and_expiry_discards_old_recollections() {
    let (_root, scope, mut frames) = setup().await;
    let store = MemoryStore::new(&scope.journal, scope.principal, 65536);
    let mut fact = value("EXPIRED_MARKER");
    fact.pinned = true;
    fact.valid_until = Some(now() + 1000);
    store
        .add(Uuid::new_v4(), 0, fact, MemorySource::web())
        .await
        .unwrap();
    frames.refresh().await.unwrap();
    let first = frames.transcript.read().canonical_payloads().len();
    frames.scope.model_revision = "another-model".into();
    frames.refresh().await.unwrap();
    assert_eq!(
        frames.transcript.read().canonical_payloads().len(),
        first + 1
    );
    frames
        .transcript
        .write()
        .append(BaseMessage::human("EXPIRED_MARKER in a compact summary"));
    // A deterministic clock boundary: advance the recorded frame's deadline
    // and authoritative item together, without waiting on wall clock.
    let mut payloads = frames.transcript.read().canonical_payloads();
    for p in &mut payloads {
        if let peri_acp_types::store::PersistedPayload::SystemReminder { reminder, .. } = p {
            let mut data = reminder.as_reminder().clone();
            data.metadata["expires_at"] = serde_json::json!(now() - 1);
            *reminder =
                peri_acp_types::system_reminder::TrustedSystemReminderFactory::for_recovery()
                    .construct(data)
                    .unwrap();
        }
    }
    *frames.transcript.write() = MessageTranscript::new().with_own_payloads(payloads);
    sqlx::query("UPDATE memory_items SET valid_until=? WHERE owner=?")
        .bind(now() - 1)
        .bind(scope.principal.to_string())
        .execute(&scope.journal.identity_pool())
        .await
        .unwrap();
    frames.refresh().await.unwrap();
    assert!(!view(&frames).contains("EXPIRED_MARKER"));
}
