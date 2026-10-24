use super::*;

#[tokio::test]
async fn test_schema3_identity_migration_reuses_binding_and_survives_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("threads.db");
    let status = std::process::Command::new("git")
        .args(["init", "-q", dir.path().to_str().unwrap()])
        .status()
        .unwrap();
    assert!(status.success(), "Git fixture initialization failed");
    version3_database(&path, dir.path())
        .await
        .close()
        .await
        .unwrap();

    let store = SqliteThreadStore::new(&path).await.unwrap();
    let workspace = store.resolve_workspace(dir.path()).await.unwrap();
    assert_eq!(
        workspace.project_id.to_string(),
        "11111111-1111-4111-8111-111111111111"
    );
    assert_eq!(
        workspace.workspace_id.to_string(),
        "22222222-2222-4222-8222-222222222222"
    );
    let execution: (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM sqlite_schema WHERE name = 'execution_runs'")
            .fetch_one(&store.database.pool)
            .await
            .unwrap();
    assert_eq!(execution, (0,));
    assert_eq!(
        store
            .load_messages(&"old-session".to_owned())
            .await
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        store
            .load_frozen_snapshot(&"old-session".to_owned())
            .await
            .unwrap()
            .as_deref(),
        Some("frozen-owner-state")
    );
    let binding = store
        .load_session_binding(&"old-session".to_owned())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        binding.project_id.to_string(),
        workspace.project_id.to_string()
    );
    assert_eq!(
        store
            .validate_session_binding(&"old-session".to_owned())
            .await
            .unwrap(),
        workspace
    );
    store.close().await;

    let reopened = SqliteThreadStore::new(&path).await.unwrap();
    let again = reopened.resolve_workspace(dir.path()).await.unwrap();
    assert_eq!(again.project_id, workspace.project_id);
    assert_eq!(again.workspace_id, workspace.workspace_id);
    assert_eq!(
        reopened
            .load_session_binding(&"old-session".to_owned())
            .await
            .unwrap()
            .unwrap()
            .workspace_id,
        workspace.workspace_id
    );
    reopened.close().await;
}

/// 记录 v2 升级不能改动的身份数据：项目、登记空间与绑定三段的原样值。
///
/// 登记空间的**载体与列名**随版本变化：v12 之前是 `workspaces(root, root_identity)`，
/// v12..v18 是改名为 `legacy_execution_registrations` 的原表，v19 起并回
/// `workspaces(path, identity)`。同一 id 下的五元组 (id, project_id, 根, 根身份, 发现)
/// 在搬运中逐字不变，所以投影按当前形状选择，跨版本的比较仍然落在同一份事实上。
async fn identity_bytes(connection: &mut SqliteConnection) -> Vec<String> {
    let registrations = column_names(connection, "legacy_execution_registrations")
        .await
        .unwrap();
    let workspaces = column_names(connection, "workspaces").await.unwrap();
    let registration_query = match (registrations.is_empty(), workspaces.contains("root")) {
        (false, _) => {
            "SELECT json_array(id, project_id, root, root_identity, discovery)
             FROM legacy_execution_registrations ORDER BY id"
        }
        (true, true) => {
            "SELECT json_array(id, project_id, root, root_identity, discovery)
             FROM workspaces ORDER BY id"
        }
        (true, false) => {
            "SELECT json_array(id, project_id, path, identity, discovery)
             FROM workspaces ORDER BY id"
        }
    };
    let mut values = Vec::new();
    for query in [
        "SELECT json_array(id, locator, object_identity) FROM projects ORDER BY id",
        registration_query,
        "SELECT json_array(thread_id, schema_version, project_id, workspace_id, relative_cwd) FROM session_bindings ORDER BY thread_id",
    ] {
        let rows: Vec<(String,)> = sqlx::query_as(AssertSqlSafe(query))
            .fetch_all(&mut *connection)
            .await
            .unwrap();
        values.extend(rows.into_iter().map(|(value,)| value));
    }
    values
}

#[tokio::test]
async fn test_version2_upgrade_removes_required_revision_and_preserves_identity() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("threads.db");
    let mut connection = version2_database(&path).await;
    let revision: (i64, Option<String>) = sqlx::query_as(
        "SELECT \"notnull\", dflt_value FROM pragma_table_info('session_bindings') WHERE name = 'revision'",
    ).fetch_one(&mut connection).await.unwrap();
    assert_eq!(revision, (1, None));
    let before_history = history_bytes(&mut connection).await;
    connection.close().await.unwrap();

    let store = SqliteThreadStore::new(&path).await.unwrap();
    let mut connection = store.database.pool.acquire().await.unwrap();
    assert_eq!(history_bytes(&mut connection).await, before_history);
    let migrated_identity = identity_bytes(&mut connection).await;
    assert!(migrated_identity
        .iter()
        .all(|value| !value.contains("birth_")));
    assert!(!column_names(&mut connection, "session_bindings")
        .await
        .unwrap()
        .contains("revision"));
    let (version,): (i64,) = sqlx::query_as("PRAGMA user_version")
        .fetch_one(&mut *connection)
        .await
        .unwrap();
    assert_eq!(version, CURRENT_SCHEMA_VERSION);
    drop(connection);

    let old = store
        .load_session_binding(&"old-session".to_owned())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(old.revision, 1);
    assert_eq!(
        old.project_id.to_string(),
        "11111111-1111-4111-8111-111111111111"
    );
    assert_eq!(
        old.workspace_id.to_string(),
        "22222222-2222-4222-8222-222222222222"
    );
    assert!(old.cwd_relative_to_workspace.as_os_str().is_empty());
    let workspace = store.resolve_workspace(dir.path()).await.unwrap();
    let id = store
        .create_bound_thread(
            ThreadMeta::new_at(dir.path().to_str().unwrap(), peri_time::now_wall()),
            &workspace,
        )
        .await
        .unwrap();
    assert_eq!(
        store.validate_session_binding(&id).await.unwrap(),
        workspace
    );
    assert_eq!(
        store
            .load_session_binding(&id)
            .await
            .unwrap()
            .unwrap()
            .revision,
        1
    );
    store.close().await;
}

#[tokio::test]
async fn test_version2_failed_column_drop_preserves_schema_version_and_data() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("threads.db");
    let mut connection = version2_database(&path).await;
    // 现有视图依赖 revision 时，SQLite 必须拒绝删除该列。
    sqlx::query("CREATE VIEW binding_revision_view AS SELECT revision FROM session_bindings")
        .execute(&mut connection)
        .await
        .unwrap();
    let before_schema: Vec<(String, Option<String>)> =
        sqlx::query_as("SELECT name, sql FROM sqlite_schema ORDER BY name")
            .fetch_all(&mut connection)
            .await
            .unwrap();
    let before_data = identity_bytes(&mut connection).await;
    connection.close().await.unwrap();
    let error = SqliteThreadStore::new(&path).await.err().unwrap();
    let message = error.to_string();
    assert!(
        message.contains("binding_revision_view") && message.contains("no such column: revision"),
        "应因视图依赖 revision 而拒绝 DROP COLUMN，实际错误：{message}"
    );
    let mut connection = SqliteConnection::connect_with(
        &SqliteConnectOptions::new().filename(&path).read_only(true),
    )
    .await
    .unwrap();
    let after_schema: Vec<(String, Option<String>)> =
        sqlx::query_as("SELECT name, sql FROM sqlite_schema ORDER BY name")
            .fetch_all(&mut connection)
            .await
            .unwrap();
    assert_eq!(after_schema, before_schema);
    assert_eq!(identity_bytes(&mut connection).await, before_data);
    let (version,): (i64,) = sqlx::query_as("PRAGMA user_version")
        .fetch_one(&mut connection)
        .await
        .unwrap();
    assert_eq!(version, 2);
    connection.close().await.unwrap();
}

#[tokio::test]
async fn test_identity_migration_collision_rolls_back_schema_and_rows() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("threads.db");
    let mut connection = version2_database(&path).await;
    sqlx::query("INSERT INTO projects VALUES (?, ?, ?)")
        .bind("33333333-3333-4333-8333-333333333333")
        .bind("/another/project")
        .bind(r#"{"device":1,"inode":2,"birth_seconds":9,"birth_nanos":10}"#)
        .execute(&mut connection)
        .await
        .unwrap();
    connection.close().await.unwrap();

    assert!(SqliteThreadStore::new(&path).await.is_err());
    let mut connection = SqliteConnection::connect_with(
        &SqliteConnectOptions::new().filename(&path).read_only(true),
    )
    .await
    .unwrap();
    let (version,): (i64,) = sqlx::query_as("PRAGMA user_version")
        .fetch_one(&mut connection)
        .await
        .unwrap();
    assert_eq!(version, 2, "冲突必须回滚版本号");
    let (identity,): (String,) = sqlx::query_as(
        "SELECT object_identity FROM projects WHERE id = '11111111-1111-4111-8111-111111111111'",
    )
    .fetch_one(&mut connection)
    .await
    .unwrap();
    assert!(identity.contains("birth_seconds"), "回滚不得改写旧身份");
}

#[tokio::test]
async fn test_identity_migration_corrupt_discovery_rolls_back_schema() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("threads.db");
    let mut connection = version2_database(&path).await;
    sqlx::query("UPDATE workspaces SET discovery = 'corrupt'")
        .execute(&mut connection)
        .await
        .unwrap();
    connection.close().await.unwrap();

    assert!(SqliteThreadStore::new(&path).await.is_err());
    let mut connection = SqliteConnection::connect_with(
        &SqliteConnectOptions::new().filename(&path).read_only(true),
    )
    .await
    .unwrap();
    let (version,): (i64,) = sqlx::query_as("PRAGMA user_version")
        .fetch_one(&mut connection)
        .await
        .unwrap();
    assert_eq!(version, 2, "损坏 discovery 必须回滚版本号");
}

#[path = "schema_registration_test.rs"]
mod registration_tests;
