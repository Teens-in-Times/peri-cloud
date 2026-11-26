#[test]
fn effect_ready_requires_runtime_generation() {
    let missing = HistoryEffect {
        execution: ExecutionEffect::Ready,
        history: HistoryEffectState::NotPublished,
        runtime_generation: None,
    };
    assert!(missing.validate().is_err());

    let ready = HistoryEffect {
        execution: ExecutionEffect::Ready,
        history: HistoryEffectState::NotPublished,
        runtime_generation: Some("runtime-9".to_owned()),
    };
    ready.validate().unwrap();

    let data = HistoryErrorData::new(
        HistoryErrorReason::BackendUnavailable,
        HistoryRecovery::Retry,
        ready,
    );
    data.validate().unwrap();
    let value = serde_json::to_value(&data).unwrap();
    assert_eq!(value["effect"]["execution"], "ready");
    assert_eq!(value["effect"]["runtimeGeneration"], "runtime-9");
}

#[test]
fn all_error_reasons_serialize() {
    let reasons = [
        HistoryErrorReason::ProtocolNotNegotiated,
        HistoryErrorReason::UnsupportedVersion,
        HistoryErrorReason::InvalidCursor,
        HistoryErrorReason::CrossWindowCursor,
        HistoryErrorReason::SnapshotInvalidated,
        HistoryErrorReason::WindowExpired,
        HistoryErrorReason::WindowClosed,
        HistoryErrorReason::TicketInvalid,
        HistoryErrorReason::AnchorUnavailable,
        HistoryErrorReason::PermissionDenied,
        HistoryErrorReason::NotFound,
        HistoryErrorReason::IndexNotReady,
        HistoryErrorReason::SourceUnavailable,
        HistoryErrorReason::CapacityExceeded,
        HistoryErrorReason::BudgetExceeded,
        HistoryErrorReason::BackendUnavailable,
        HistoryErrorReason::Timeout,
        HistoryErrorReason::RequestConflict,
        HistoryErrorReason::OperationExpired,
        HistoryErrorReason::OperationInProgress,
        HistoryErrorReason::DeliveryExpired,
        HistoryErrorReason::DeliveryGap,
        HistoryErrorReason::Busy,
        HistoryErrorReason::InvalidRequest,
    ];
    for reason in reasons {
        let value = serde_json::to_value(reason).unwrap();
        assert!(
            value.as_str().is_some(),
            "{reason:?} must serialize as string"
        );
        let back: HistoryErrorReason = serde_json::from_value(value).unwrap();
        assert_eq!(back, reason);
    }
    assert_eq!(
        serde_json::to_value(HistoryErrorReason::ProtocolNotNegotiated).unwrap(),
        "protocol_not_negotiated"
    );
    assert_eq!(
        serde_json::to_value(HistoryErrorReason::OperationInProgress).unwrap(),
        "operation_in_progress"
    );
}

#[test]
fn identity_lengths_are_bounded() {
    let mut request = decode_open(&open_wire("1", latest_anchor(), 50, 0, 524288));
    request.session_id = "x".repeat(512);
    request.validate(&limits()).unwrap();
    request.session_id = "x".repeat(513);
    assert!(matches!(
        request.validate(&limits()),
        Err(HistoryContractError::IdentityTooLong { field: "sessionId" })
    ));

    let mut entry_request = decode_open(&open_wire("1", entry_anchor(), 25, 24, 524288));
    if let HistoryAnchor::Entry(anchor) = &mut entry_request.anchor {
        anchor.entry_id = "x".repeat(513);
    }
    assert!(matches!(
        entry_request.validate(&limits()),
        Err(HistoryContractError::IdentityTooLong { field: "entryId" })
    ));

    let mut page = decode_page(&page_wire(50));
    page.cursor = Some("x".repeat(HISTORY_TOKEN_MAX_BYTES + 1));
    assert!(matches!(
        page.validate(&limits()),
        Err(HistoryContractError::IdentityTooLong { field: "cursor" })
    ));
}

#[test]
fn page_cursor_is_optional_and_bounded_when_present() {
    let mut value = page_wire(50);
    value.as_object_mut().unwrap().remove("cursor");
    let page = decode_page(&value);
    assert!(page.cursor.is_none());
    page.validate(&limits()).unwrap();
}

#[test]
fn page_rejects_unknown_fields_and_wrong_kind() {
    let value = serde_json::to_value(window_page()).unwrap();
    assert!(serde_json::from_value::<HistoryWindowPage>(with_extra(value.clone())).is_err());

    let mut wrong_kind = value;
    wrong_kind["kind"] = json!("initial");
    assert!(serde_json::from_value::<HistoryWindowPage>(wrong_kind).is_err());
}

#[test]
fn nested_enum_variants_reject_unknown_fields() {
    let anchor_extra = json!({
        "kind": "entry",
        "entryId": "e1",
        "source": { "kind": "own" },
        "extra": true
    });
    assert!(serde_json::from_value::<HistoryAnchor>(anchor_extra).is_err());

    let source_extra = json!({
        "kind": "inherited",
        "snapshotId": "snapshot-1",
        "ancestorSessionId": "ancestor-1",
        "extra": true
    });
    assert!(serde_json::from_value::<HistorySource>(source_extra).is_err());

    let sub_item_extra = json!({
        "kind": "text",
        "subItemId": "text-0",
        "role": "assistant",
        "text": "hello",
        "extra": true
    });
    assert!(serde_json::from_value::<HistorySubItem>(sub_item_extra).is_err());

    let coverage_extra = json!({
        "kind": "interval",
        "from": { "kind": "head" },
        "to": { "kind": "tail" },
        "extra": true
    });
    assert!(serde_json::from_value::<HistoryCoverage>(coverage_extra).is_err());
}

#[test]
fn initial_result_round_trip() {
    let result = HistoryInitialResult {
        version: 2,
        kind: HistoryInitialKind::Initial,
        request_key: DecimalU64::new(1),
        execution_ready: true,
        runtime_generation: "runtime-9".to_owned(),
        delivery_id: "delivery-51".to_owned(),
        live_boundary: HistoryLiveBoundary {
            stream_id: "live-7".to_owned(),
            after_seq: DecimalU64::new(120),
        },
        page: {
            let mut page = window_page();
            page.committed_tail = None;
            page.items.clear();
            page.coverage = HistoryCoverage::EmptySnapshot;
            page.progress = HistoryProgress {
                reason: HistoryProgressReason::Complete,
                scanned_entries: 0,
                consumed_entries: 0,
            };
            page.expires_at = None;
            page
        },
    };
    result.validate().unwrap();
    let value = serde_json::to_value(&result).unwrap();
    assert_eq!(value["kind"], "initial");
    assert_eq!(value["liveBoundary"]["afterSeq"], "120");
    assert!(value["page"]["committedTail"].is_null());
    let back: HistoryInitialResult = serde_json::from_value(value).unwrap();
    assert_eq!(back, result);
}
