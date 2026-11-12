#[tokio::test]
async fn upgrade_moves_a_released_store_to_the_current_generation_in_one_batch() {
    let fixture = Fixture::new().await;
    let retained_sql = "SELECT 'threads', json_array(id, title, cwd, created_at, updated_at, message_count, parent_thread_id, snapshot_at_message_id, hidden, cancel_policy, config, frozen_context, inherited_context, agent_status) FROM threads
        UNION ALL SELECT 'projects', json_array(id, locator) FROM projects
        UNION ALL SELECT 'bindings', json_array(thread_id, schema_version, project_id, relative_cwd) FROM session_bindings
        ORDER BY 1, 2";
    let before: Vec<(String, String)> = sqlx::query_as(retained_sql)
        .fetch_all(&fixture.transport.pool)
        .await
        .unwrap();
    let store = fixture.store(StoreAccess::ReadWrite);
    assert!(matches!(
        open_step(
            StoreIdentityRead::Present(fixture.snapshot.clone()),
            Some(fixture.shape_verdict(schema::ShapeProbe::Input).await),
            StoreAccess::ReadWrite
        )
        .unwrap(),
        OpenStep::Upgrade(_)
    ));

    schema_upgrade::upgrade(&store, &fixture.snapshot)
        .await
        .unwrap();

    assert_upgraded(&fixture).await;
    let after: Vec<(String, String)> = sqlx::query_as(retained_sql)
        .fetch_all(&fixture.transport.pool)
        .await
        .unwrap();
    assert_eq!(after, before);
    let workspace: (String, String, String, String) =
        sqlx::query_as("SELECT machine_id, path, path_source, project_id FROM workspaces")
            .fetch_one(&fixture.transport.pool)
            .await
            .unwrap();
    assert_eq!(workspace.0, "original machine");
    assert_eq!(workspace.1, "/tmp");
    assert_eq!(workspace.2, "unverified");
    assert_eq!(workspace.3, "project");
    let binding: (String, String) =
        sqlx::query_as("SELECT workspace_id, discovery_snapshot FROM session_bindings")
            .fetch_one(&fixture.transport.pool)
            .await
            .unwrap();
    assert_eq!(binding.0, "11111111-1111-4111-8111-111111111111");
    // 绑定行的创建证据收缩为最小执行根（v15）：native 只有 `root`。
    assert_eq!(binding.1, r#"{"root":"/tmp"}"#);
    let thread: (String, i64) =
        sqlx::query_as("SELECT workspace_id, archived FROM threads WHERE id = 'session'")
            .fetch_one(&fixture.transport.pool)
            .await
            .unwrap();
    assert_eq!(thread.0, binding.0);
    assert_eq!(thread.1, 0);
    let machines: Vec<(String, String)> =
        sqlx::query_as("SELECT id, identity_kind FROM machines ORDER BY id")
            .fetch_all(&fixture.transport.pool)
            .await
            .unwrap();
    assert_eq!(machines.len(), 2);
    assert!(machines
        .iter()
        .any(|(id, kind)| id == "original machine" && kind == "legacy_unknown"));
    assert!(
        machines.iter().any(|(_, kind)| kind == "known"),
        "当前机器也要可查询"
    );
    assert_eq!(
        fixture
            .count("SELECT COUNT(*) FROM mcp_oauth_credentials".to_owned())
            .await,
        0,
        "machine 作用域凭证无法证明 Workspace 归属"
    );
    let (identity, timestamp, config): (String, String, String) = sqlx::query_as(
        "SELECT store_id, peri_store_meta.created_at, config FROM peri_store_meta, threads",
    )
    .fetch_one(&fixture.transport.pool)
    .await
    .unwrap();
    assert_eq!(
        (identity.as_str(), timestamp.as_str(), config.as_str()),
        (
            "existing-store",
            "original timestamp",
            r#"{"model":"keep"}"#
        )
    );
    let (receipt,): (String,) = sqlx::query_as("SELECT receipt FROM peri_op_ledger")
        .fetch_one(&fixture.transport.pool)
        .await
        .unwrap();
    assert_eq!(receipt, "receipt");
    let history: (i64, String, bool, String) =
        sqlx::query_as("SELECT rowid, content, excluded, projection FROM messages")
            .fetch_one(&fixture.transport.pool)
            .await
            .unwrap();
    assert_eq!(
        history,
        (
            7,
            "original content".into(),
            true,
            "original projection".into()
        )
    );
    assert_eq!(
        fixture
            .count(
                "SELECT COUNT(*) FROM sqlite_master WHERE name = 'session_close_intents'"
                    .to_owned()
            )
            .await,
        1
    );
    assert!(matches!(
        open_step(
            store.read_identity().await.unwrap(),
            probed_shape(&store).await,
            StoreAccess::ReadWrite
        )
        .unwrap(),
        OpenStep::Existing(_)
    ));
    assert_eq!(
        fixture.transport.writes.load(Ordering::SeqCst),
        1,
        "整段迁移是一个受管批次"
    );
}

/// 真实项目行是 v15 `projects` 的身份来源：locator 不被归属行路径覆盖，重复 locator
/// 保留 rowid 最早一行并改绑引用，没有被会话引用的项目也保留。
#[tokio::test]
async fn upgrade_preserves_project_rows_and_merges_duplicate_locators() {
    let fixture = Fixture::new().await;
    sqlx::raw_sql(
        "INSERT INTO projects VALUES ('project-late', '/tmp', 'late identity');
         INSERT INTO projects VALUES ('project-orphan', '/orphan', 'orphan identity');
         UPDATE session_bindings SET project_id = 'project-late' WHERE thread_id = 'session'",
    )
    .execute(&fixture.transport.pool)
    .await
    .unwrap();

    schema_upgrade::upgrade(&fixture.store(StoreAccess::ReadWrite), &fixture.snapshot)
        .await
        .unwrap();

    let projects: Vec<(String, String)> =
        sqlx::query_as("SELECT id, locator FROM projects ORDER BY id")
            .fetch_all(&fixture.transport.pool)
            .await
            .unwrap();
    assert_eq!(
        projects,
        vec![
            ("project".to_owned(), "/tmp".to_owned()),
            ("project-orphan".to_owned(), "/orphan".to_owned()),
        ],
        "重复 locator 保留 rowid 最早一行，未被引用的项目不丢"
    );
    let (project_id,): (String,) =
        sqlx::query_as("SELECT project_id FROM session_bindings WHERE thread_id = 'session'")
            .fetch_one(&fixture.transport.pool)
            .await
            .unwrap();
    assert_eq!(project_id, "project", "被合并行的引用改绑到保留行");
    assert_eq!(
        fixture.shape_verdict(schema::ShapeProbe::Current).await,
        StoreShapeRead::Consistent
    );
}

/// 版本 11 的 `v2` 库（v4 内部用过、未正式发布）：形状就是搬运输入，同一条批次接纳。
#[tokio::test]
async fn upgrade_accepts_the_internal_version_11_shape() {
    let fixture = Fixture::new().await;
    fixture.declare(11, PREVIOUS_CONTRACT).await;
    let StoreIdentityRead::Present(snapshot) = fixture
        .store(StoreAccess::ReadWrite)
        .read_identity()
        .await
        .unwrap()
    else {
        panic!("missing identity")
    };
    assert!(matches!(
        open_step(
            StoreIdentityRead::Present(snapshot.clone()),
            Some(fixture.shape_verdict(schema::ShapeProbe::Input).await),
            StoreAccess::ReadWrite
        )
        .unwrap(),
        OpenStep::Upgrade(_)
    ));

    schema_upgrade::upgrade(&fixture.store(StoreAccess::ReadWrite), &snapshot)
        .await
        .unwrap();

    assert_eq!(fixture.version().await, schema::REMOTE_SCHEMA_VERSION);
    assert_eq!(fixture.contract().await, schema::STORE_CONTRACT);
    assert_eq!(
        fixture.shape_verdict(schema::ShapeProbe::Current).await,
        StoreShapeRead::Consistent
    );
    assert_eq!(
        fixture
            .count(
                "SELECT COUNT(*) FROM sqlite_master WHERE name = 'session_environments'".to_owned()
            )
            .await,
        0
    );
}

/// 只有身份与账本的空库（v2 契约但一张 canonical 表都没有）：直接建当前形状，身份不换。
#[tokio::test]
async fn upgrade_builds_the_current_shape_for_an_identity_only_store() {
    let fixture = Fixture::new().await;
    sqlx::raw_sql(
        "DROP TABLE thread_goals; DROP TABLE execution_runs; DROP TABLE messages; DROP TABLE threads;
         DROP TABLE session_bindings; DROP TABLE session_environments; DROP TABLE workspaces; DROP TABLE projects;
         DROP TABLE mcp_oauth_credentials",
    )
    .execute(&fixture.transport.pool)
    .await
    .unwrap();

    schema_upgrade::upgrade(&fixture.store(StoreAccess::ReadWrite), &fixture.snapshot)
        .await
        .unwrap();

    assert_eq!(fixture.version().await, schema::REMOTE_SCHEMA_VERSION);
    assert_eq!(fixture.contract().await, schema::STORE_CONTRACT);
    assert_eq!(
        fixture.shape_verdict(schema::ShapeProbe::Current).await,
        StoreShapeRead::Consistent,
        "空库分支建出的形状也要逐列等于 canonical"
    );
    let (identity,): (String,) = sqlx::query_as("SELECT store_id FROM peri_store_meta")
        .fetch_one(&fixture.transport.pool)
        .await
        .unwrap();
    assert_eq!(identity, "existing-store");
    let (columns,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM pragma_table_xinfo('threads')")
        .fetch_one(&fixture.transport.pool)
        .await
        .unwrap();
    assert_eq!(
        columns,
        16 + crate::sessions::canonical::THREADS_V14_COLUMNS.len() as i64,
        "v13 的 16 列 + 两列工作快照"
    );
    for table in [
        "machines",
        "session_bindings",
        "mcp_oauth_credentials",
        "session_close_intents",
    ] {
        assert_eq!(
            fixture
                .count(format!(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = '{table}'"
                ))
                .await,
            1,
            "{table}"
        );
    }
}

/// 只读访问不发写批次，旧状态原样。
#[tokio::test]
async fn upgrade_requires_a_writable_store() {
    let fixture = Fixture::new().await;
    let reader = fixture.store(StoreAccess::ReadOnly);
    let error = schema_upgrade::upgrade(&reader, &fixture.snapshot)
        .await
        .unwrap_err();
    assert!(matches!(
        error.kind(),
        SessionResourceErrorKind::ReadOnlyStore
    ));
    assert_eq!(fixture.transport.writes.load(Ordering::SeqCst), 0);
    fixture.assert_old_state().await;
}

/// 历史表缺列：形状不是本构建认识的输入，拒绝且不发批次。
#[tokio::test]
async fn upgrade_rejects_missing_history_columns_without_writing() {
    let fixture = Fixture::new().await;
    sqlx::query("ALTER TABLE messages DROP COLUMN projection")
        .execute(&fixture.transport.pool)
        .await
        .unwrap();
    let error = schema_upgrade::upgrade(&fixture.store(StoreAccess::ReadWrite), &fixture.snapshot)
        .await
        .unwrap_err();
    assert!(matches!(
        error.kind(),
        SessionResourceErrorKind::Unsupported
    ));
    assert_eq!(fixture.transport.writes.load(Ordering::SeqCst), 0);
    fixture.assert_old_state().await;
}

/// 拒绝面：不匹配的版本（含 v2 契约的 12）、其他契约标签、更早的版本，一律不认识。
#[tokio::test]
async fn upgrade_refuses_generations_and_contracts_outside_the_compatible_one() {
    for (version, contract) in [
        (9, PREVIOUS_CONTRACT),
        (12, PREVIOUS_CONTRACT),
        (13, PREVIOUS_CONTRACT),
        (14, PREVIOUS_CONTRACT),
        (15, PREVIOUS_CONTRACT),
        (16, PREVIOUS_CONTRACT),
        (17, PREVIOUS_CONTRACT),
        (18, PREVIOUS_CONTRACT),
        (19, PREVIOUS_CONTRACT),
        (10, "peri.session.store/v3"),
        (11, "peri.session.store/v4"),
        (10, "unknown-contract"),
    ] {
        let fixture = Fixture::new().await;
        fixture.declare(version, contract).await;
        let StoreIdentityRead::Present(snapshot) = fixture
            .store(StoreAccess::ReadWrite)
            .read_identity()
            .await
            .unwrap()
        else {
            panic!("missing identity")
        };
        // 版本判定：13..19 比本构建新；12 不是 v2 契约的合法版本，走输入形状拒绝。
        if version > schema::REMOTE_SCHEMA_VERSION {
            assert_eq!(
                schema::acceptance(version),
                schema::SchemaAcceptance::TooNew,
                "({version}, {contract}) 必须判成 TooNew"
            );
            assert!(!snapshot.readable());
        }
        // 身份读取之后 `open_step` 就拒绝：连形状探测都不做（`None`）。
        for access in [StoreAccess::ReadOnly, StoreAccess::ReadWrite] {
            let refused =
                open_step(StoreIdentityRead::Present(snapshot.clone()), None, access).unwrap_err();
            assert!(
                matches!(refused.kind(), SessionResourceErrorKind::Unsupported),
                "({version}, {contract}, {access:?}) 必须在打开路径被拒绝"
            );
        }
        let error = schema_upgrade::upgrade(&fixture.store(StoreAccess::ReadWrite), &snapshot)
            .await
            .unwrap_err();
        assert!(
            matches!(error.kind(), SessionResourceErrorKind::Unsupported),
            "({version}, {contract}) 必须被拒绝"
        );
        assert_eq!(fixture.transport.writes.load(Ordering::SeqCst), 0);
        assert_eq!(fixture.version().await, version);
        assert_eq!(fixture.contract().await, contract);
    }
}

/// 快照读完之后元数据被改写：批次开头的守卫让整批失败，库保持原样。
#[tokio::test]
async fn upgrade_guards_the_meta_snapshot_before_mutating() {
    for mutation in [
        "UPDATE peri_store_meta SET store_id = 'replacement-store'",
        "UPDATE peri_store_meta SET contract = 'unknown-contract'",
    ] {
        let fixture = Fixture::new().await;
        sqlx::query(mutation)
            .execute(&fixture.transport.pool)
            .await
            .unwrap();
        let before: (i64, String, String) =
            sqlx::query_as("SELECT schema_version, store_id, contract FROM peri_store_meta")
                .fetch_one(&fixture.transport.pool)
                .await
                .unwrap();
        assert!(
            schema_upgrade::upgrade(&fixture.store(StoreAccess::ReadWrite), &fixture.snapshot)
                .await
                .is_err()
        );
        let after: (i64, String, String) =
            sqlx::query_as("SELECT schema_version, store_id, contract FROM peri_store_meta")
                .fetch_one(&fixture.transport.pool)
                .await
                .unwrap();
        assert_eq!(before, after);
        assert_eq!(
            fixture
                .count("SELECT COUNT(*) FROM sqlite_master WHERE name = 'legacy_execution_registrations'".to_owned())
                .await,
            0
        );
    }
}

/// 快照读完之后多出一个对象：对象数守卫让整批失败，扩展对象留在库里。
#[tokio::test]
async fn upgrade_detects_objects_created_after_its_snapshot() {
    let fixture = Fixture::new().await;
    fixture
        .transport
        .change_schema
        .store(true, Ordering::SeqCst);
    assert!(
        schema_upgrade::upgrade(&fixture.store(StoreAccess::ReadWrite), &fixture.snapshot)
            .await
            .is_err()
    );
    fixture.assert_old_state().await;
    assert_eq!(
        fixture
            .count("SELECT COUNT(*) FROM sqlite_master WHERE name = 'late_extension'".to_owned())
            .await,
        1
    );
}

/// 退役对象形状不认识（多一列）：拒绝，不猜内容。
#[tokio::test]
async fn upgrade_refuses_unknown_retired_shapes_without_writing() {
    let fixture = Fixture::new().await;
    sqlx::query("ALTER TABLE execution_runs ADD COLUMN extension_data TEXT")
        .execute(&fixture.transport.pool)
        .await
        .unwrap();
    let error = schema_upgrade::upgrade(&fixture.store(StoreAccess::ReadWrite), &fixture.snapshot)
        .await
        .unwrap_err();
    assert!(matches!(
        error.kind(),
        SessionResourceErrorKind::Unsupported
    ));
    assert_eq!(fixture.transport.writes.load(Ordering::SeqCst), 0);
    fixture.assert_old_state().await;
}

/// 批次中途失败：整批回滚，旧状态与版本保持，重试能成功。
#[tokio::test]
async fn upgrade_rolls_back_a_failed_batch_and_retries() {
    let fixture = Fixture::new().await;
    fixture
        .transport
        .fail_column_drop
        .store(true, Ordering::SeqCst);
    assert!(
        schema_upgrade::upgrade(&fixture.store(StoreAccess::ReadWrite), &fixture.snapshot)
            .await
            .is_err()
    );
    fixture.assert_old_state().await;
    assert_eq!(
        fixture
            .count("SELECT COUNT(*) FROM execution_runs".to_owned())
            .await,
        1,
        "退役表在回滚后仍在"
    );

    schema_upgrade::upgrade(&fixture.store(StoreAccess::ReadWrite), &fixture.snapshot)
        .await
        .unwrap();

    assert_upgraded(&fixture).await;
}

/// 回复丢失或行数不完整：结论是「不确定」，但库已经提交；重新读身份看到的就是真实代数。
#[tokio::test]
async fn upgrade_reports_an_uncertain_reply_but_reopen_reads_the_committed_generation() {
    for lost in [true, false] {
        let fixture = Fixture::new().await;
        if lost {
            fixture.transport.drop_reply.store(true, Ordering::SeqCst);
        } else {
            fixture
                .transport
                .truncate_reply
                .store(true, Ordering::SeqCst);
        }
        let error =
            schema_upgrade::upgrade(&fixture.store(StoreAccess::ReadWrite), &fixture.snapshot)
                .await
                .unwrap_err();
        assert!(matches!(
            error.kind(),
            SessionResourceErrorKind::PersistenceUncertain { .. }
        ));
        // 单条批次已经提交：库停在当前代数，重开的结论是「本构建直接可用」。
        assert_eq!(fixture.version().await, schema::REMOTE_SCHEMA_VERSION);
        assert_eq!(fixture.contract().await, schema::STORE_CONTRACT);
        let reopened = fixture.store(StoreAccess::ReadWrite);
        assert!(matches!(
            open_step(
                reopened.read_identity().await.unwrap(),
                probed_shape(&reopened).await,
                StoreAccess::ReadWrite
            )
            .unwrap(),
            OpenStep::Existing(_)
        ));
        assert_eq!(fixture.transport.writes.load(Ordering::SeqCst), 1);
    }
}

// Full remote application behavior is tested with the same SQL transport fixture.
mod full_remote_tests {
    include!("full_remote_test.rs");
}
