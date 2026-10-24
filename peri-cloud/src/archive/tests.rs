use super::*;
use peri_acp_types::{
    context_archive::*,
    messages::{BaseMessage, ToolCallRequest},
    permission::PermissionMode,
    store::{MessageFlags, PersistedPayload},
};
use peri_agent::{
    agent::compact_v2::{
        config::CompactConfig, micro::apply_micro_plan, planner::*, projection::*,
    },
    context_archive::ContextArchiveReader,
    context_archive_storage::{ArchiveStorage, DeploymentArchive},
    session::MessageTranscript,
    tools::ContextRetention,
};
use std::collections::HashMap;
use uuid::Uuid;

async fn fixture() -> (tempfile::TempDir, PersonalScope) {
    let root = tempfile::tempdir().unwrap();
    let journal = Arc::new(crate::state::CloudJournal::open(root.path()).await.unwrap());
    let principal = Uuid::new_v4();
    let session_id = Uuid::new_v4();
    sqlx::query("INSERT INTO principals(id,login,display_name,password_hash) VALUES(?,?,?,?)")
        .bind(principal.to_string())
        .bind(principal.to_string())
        .bind("test")
        .bind("unused")
        .execute(&journal.identity_pool())
        .await
        .unwrap();
    journal
        .bind(
            crate::state::FrozenSession {
                session_id,
                principal_id: principal,
                device_id: None,
                binding: None,
                system_prompt: "test".into(),
                context_window: 8000,
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
        model_revision: "test".into(),
        private_memory: false,
        scheduled: false,
        schedule_push: false,
        journal,
        principal,
        max_bytes: 1024 * 1024,
    };
    (root, scope)
}

#[tokio::test]
async fn micro_archive_commits_exact_originals_and_restores_with_bounded_recovery() {
    let (_root, scope) = fixture().await;
    let archive = CloudArchive::open(scope.clone()).await.unwrap();
    let id = scope.session_id.to_string();
    let mut transcript = MessageTranscript::new();
    transcript.bind_deployment_archive(id.clone(), archive.clone());
    transcript.append(BaseMessage::human("Keep the public interface unchanged."));
    for i in 0..6 {
        let call = format!("read-{i}");
        transcript.append(BaseMessage::ai_with_tool_calls(
            "",
            vec![ToolCallRequest::new(
                &call,
                "Read",
                serde_json::json!({"path":"history.rs"}),
            )],
        ));
        transcript.append(BaseMessage::tool_result(
            &call,
            format!("original-{i}:{}", "source中文".repeat(4000)),
        ));
    }
    transcript.checkpoint_archive().await.unwrap();
    let mut facts = MicroPlanningFacts {
        archive_scope: Some(ArchivePlanningScope {
            thread_id: id.clone(),
            committed_tail: transcript.entries().last().unwrap().id(),
        }),
        ..Default::default()
    };
    for entry in transcript.entries() {
        facts.exposed_message_ids.insert(entry.id());
        if let Some(message) = entry.as_message() {
            for call in message.tool_calls() {
                facts.closed_call_ids.insert(call.id.clone());
                facts
                    .tool_retention
                    .insert(call.name.to_lowercase(), ContextRetention::Recomputable);
            }
        }
    }
    let plan = plan_micro_with_facts(
        &transcript,
        &CompactConfig {
            micro_compact_stale_steps: 0,
            ..Default::default()
        },
        &ContextPressure {
            estimated_tokens: 6000,
            context_window: 8000,
            output_reserve: 1000,
            predicted_tool_growth: 0,
            safety_buffer: 0,
            cache_hit_rate: 0.0,
        },
        &facts,
        &ProviderCapabilities::openai(),
    )
    .unwrap();
    assert!(!plan.catalogs.is_empty());
    apply_micro_plan(&mut transcript, &plan).await.unwrap();
    let payloads = transcript.canonical_payloads();
    let flags = payloads
        .iter()
        .map(|p| (p.id(), transcript.flags(p.id())))
        .collect::<HashMap<_, _>>();
    assert_eq!(payloads.len(), 13);
    let hot = render_persisted_llm_view(&transcript, &ProviderCapabilities::openai()).unwrap();
    let fresh = CloudArchive::open(scope.clone()).await.unwrap();
    let mut restored = MessageTranscript::new().with_own_payloads(payloads.clone());
    restored.set_flags_batch(flags);
    restored.bind_deployment_archive(id.clone(), fresh.clone());
    restored.checkpoint_archive().await.unwrap();
    let cold = render_persisted_llm_view(&restored, &ProviderCapabilities::openai()).unwrap();
    assert_eq!(
        serde_json::to_value(hot).unwrap(),
        serde_json::to_value(cold).unwrap()
    );
    let reader = ContextArchiveReader::from_storage(fresh.clone(), id.clone());
    let catalog = &plan.catalogs[0];
    let reference = ArchiveReference::Catalog {
        source_thread_id: id.clone(),
        anchor_message_id: catalog.anchor_message_id,
        digest: catalog.digest().unwrap(),
        member: Some(0),
    }
    .encode()
    .unwrap();
    let mut cursor = None;
    let mut original = Vec::new();
    let mut complete = false;
    for _ in 0..128 {
        let response = reader
            .read(ArchiveReadRequest {
                archive_ref: reference.clone(),
                cursor,
                budget_tokens: Some(1024),
            })
            .await
            .unwrap();
        assert!(serde_json::to_vec(&response).unwrap().len() <= 4096);
        let entry = &response.entries[0];
        assert_eq!(entry.offset, original.len() as u64);
        match entry.encoding.as_str() {
            "utf8" => original.extend_from_slice(entry.data.as_bytes()),
            "hex" => {
                for index in (0..entry.data.len()).step_by(2) {
                    original.push(u8::from_str_radix(&entry.data[index..index + 2], 16).unwrap());
                }
            }
            other => panic!("unexpected archive encoding: {other}"),
        }
        if response.complete {
            complete = true;
            break;
        }
        cursor = response.next_cursor;
        assert!(cursor.is_some());
    }
    let expected = payloads
        .iter()
        .find(|p| p.id() == catalog.members[0].message_id)
        .unwrap();
    assert!(complete, "archive recovery must reach its terminal cursor");
    assert_eq!(
        content_digest(&original),
        content_digest(
            peri_acp_types::store::serialize_persisted_payload(expected)
                .unwrap()
                .as_bytes()
        )
    );
    assert!(fresh
        .read_archive_metadata(
            &Uuid::new_v4().to_string(),
            &ArchiveMetadataQuery {
                limit: 1,
                ..Default::default()
            }
        )
        .await
        .is_err());
    let mut stranger = scope.clone();
    stranger.principal = Uuid::new_v4();
    assert!(CloudArchive::open(stranger).await.is_err());
    // Full may exclude members, but its active root keeps originals recoverable.
    let summary = BaseMessage::human("summary");
    let mut full_flags = HashMap::new();
    for payload in &payloads {
        let mut flags = restored.flags(payload.id());
        flags.excluded = true;
        full_flags.insert(payload.id(), flags);
    }
    full_flags.insert(
        summary.id(),
        MessageFlags {
            projection: Some(MessageProjectionDirective {
                policy_version: PROJECTION_POLICY_VERSION,
                archive_roots: plan.archive_roots.clone(),
                ..Default::default()
            }),
            ..Default::default()
        },
    );
    let mut full_payloads = payloads;
    full_payloads.push(PersistedPayload::Message(summary));
    fresh
        .checkpoint(&id, &full_payloads, &full_flags)
        .await
        .unwrap();
    assert!(reader
        .read(ArchiveReadRequest {
            archive_ref: reference,
            cursor: None,
            budget_tokens: Some(1024)
        })
        .await
        .is_ok());
}

#[tokio::test]
async fn archive_rejects_changed_identity_clear_forget_and_hidden_reasoning() {
    let (_root, scope) = fixture().await;
    let archive = CloudArchive::open(scope.clone()).await.unwrap();
    let id = scope.session_id.to_string();
    let payload = PersistedPayload::Message(BaseMessage::human("original"));
    archive
        .checkpoint(&id, std::slice::from_ref(&payload), &HashMap::new())
        .await
        .unwrap();
    let mut changed = payload.clone();
    if let PersistedPayload::Message(BaseMessage::Human { content, .. }) = &mut changed {
        *content = "changed".into();
    }
    assert!(archive
        .checkpoint(&id, &[changed], &HashMap::new())
        .await
        .is_err());
    let reasoning = PersistedPayload::Message(BaseMessage::ai(
        peri_acp_types::messages::MessageContent::Blocks(vec![ContentBlock::Reasoning {
            text: "private reasoning".into(),
            signature: None,
        }]),
    ));
    assert!(!archive.permits_recovery(&reasoning));
    archive
        .checkpoint(&id, &[payload.clone(), reasoning.clone()], &HashMap::new())
        .await
        .unwrap();
    let chunk = ArchiveChunkQuery {
        message_id: reasoning.id(),
        committed_tail: payload.id(),
        offset: 0,
        max_bytes: 4096,
    };
    assert!(archive.read_archive_chunk(&id, &chunk).await.is_err());
    let pool = scope.journal.identity_pool();
    sqlx::query("INSERT INTO memory_accounts(owner,generation,privacy_epoch) VALUES(?,0,1)")
        .bind(scope.principal.to_string())
        .execute(&pool)
        .await
        .unwrap();
    assert!(archive
        .read_archive_metadata(
            &id,
            &ArchiveMetadataQuery {
                limit: 1,
                ..Default::default()
            }
        )
        .await
        .is_err());
    let after_forget = CloudArchive::open(scope.clone()).await.unwrap();
    assert!(after_forget
        .read_archive_metadata(
            &id,
            &ArchiveMetadataQuery {
                limit: 1,
                ..Default::default()
            }
        )
        .await
        .is_err());
    sqlx::query("UPDATE cloud_sessions SET state_json=json_set(state_json,'$.context_generation',1) WHERE id=?").bind(&id).execute(&pool).await.unwrap();
    assert!(after_forget
        .checkpoint(&id, &[payload], &HashMap::new())
        .await
        .is_err());
}
