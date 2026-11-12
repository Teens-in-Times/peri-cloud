//! 远端单条迁移（`v2` 契约 10|11 → 11|`v5`）的批次、守卫与拒绝面。
//!
//! 传输夹具是 SQLite 适配器扮演的「远端服务端」：外键关闭（远端服务端不强制外键），
//! 迁移作为**一个受管批次**逐条语句执行，失败整批回滚。夹具的输入库手写压缩前形状，
//! 不引用当前 `canonical` 的建表清单——升级实现改动时夹具不该跟着漂移。

use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Arc, Mutex,
};

use async_trait::async_trait;
use peri_acp_types::session_resources::{CloseSettlement, SessionResourceErrorKind};
use sqlx::{
    sqlite::{SqliteArguments, SqlitePoolOptions, SqliteRow},
    Connection, Row, Sqlite, SqlitePool, TypeInfo, ValueRef,
};
use turso_serverless::{Error as SdkError, Value};

use super::connection::RemoteTransport;
use super::generation::ConnectionGate;
use super::mutation::{RemoteStore, StoreAccess};
use super::schema::{self, StoreIdentityRead, StoreShapeRead, StoreSnapshot};
use super::schema_upgrade;
use super::session_data::{open_step, probe_shape, OpenStep};
use super::sql::StatementSpec;
use crate::sessions::{
    canonical::{V10_CREATE_INDEXES, V10_CREATE_TABLES},
    schema_cleanup::{LEGACY_EXECUTION_SQL, LEGACY_GOALS_SQL},
};

/// 压缩前形状（契约 `v2`，版本 10|11）的登记行数据；契约标签是升级来源。
pub(super) const PREVIOUS_CONTRACT: &str = "peri.session.store/v2";
pub(super) const DISCOVERY: &str = r#"{"root":"/tmp","common_dir":null,"private_dir":null}"#;

pub(super) struct SqliteTransport {
    pub(super) pool: SqlitePool,
    pub(super) writes: AtomicUsize,
    pub(super) read_batches: AtomicUsize,
    fail_column_drop: AtomicBool,
    fail_recovery_drop: AtomicBool,
    change_schema: AtomicBool,
    /// 批次下发前的并发写入（测试用 SQL 文本）：模拟两批读取与批次之间别的连接改了库。
    ///
    /// 文本由测试在运行时拼（`String` 而不是 `&'static str`）：并发写入要引用夹具里
    /// 运行期才知道的 id 与 rowid。
    pub(super) before_batch: Mutex<Option<String>>,
    drop_reply: AtomicBool,
    truncate_reply: AtomicBool,
}

impl SqliteTransport {
    /// 干净夹具：只给一个真 SQLite 池，故障开关全部关闭。
    ///
    /// 给测试自己建库的同类（见 `session_history_page_test.rs`）复用同一份传输面，
    /// 不必知道故障开关的字段形状。
    pub(super) fn for_test(pool: SqlitePool) -> Self {
        Self {
            pool,
            writes: AtomicUsize::new(0),
            read_batches: AtomicUsize::new(0),
            fail_column_drop: AtomicBool::new(false),
            fail_recovery_drop: AtomicBool::new(false),
            change_schema: AtomicBool::new(false),
            before_batch: Mutex::new(None),
            drop_reply: AtomicBool::new(false),
            truncate_reply: AtomicBool::new(false),
        }
    }
}

fn query(spec: &StatementSpec) -> sqlx::query::Query<'static, Sqlite, SqliteArguments> {
    let mut query = sqlx::query(spec.sql);
    for value in &spec.params {
        query = match value {
            Value::Null => query.bind(None::<String>),
            Value::Integer(number) => query.bind(*number),
            Value::Real(number) => query.bind(*number),
            Value::Text(text) => query.bind(text.clone()),
            Value::Blob(bytes) => query.bind(bytes.clone()),
        };
    }
    query
}

fn values(row: &SqliteRow) -> Vec<Value> {
    (0..row.len())
        .map(|index| {
            let raw = row.try_get_raw(index).unwrap();
            if raw.is_null() {
                return Value::Null;
            }
            match raw.type_info().name() {
                "INTEGER" => Value::Integer(row.get(index)),
                "REAL" => Value::Real(row.get(index)),
                "BLOB" => Value::Blob(row.get(index)),
                _ => Value::Text(row.get(index)),
            }
        })
        .collect()
}

fn error_of(error: sqlx::Error) -> SdkError {
    // 生产 SDK 把全部 `SQLITE_CONSTRAINT*` 折成 `Constraint`；夹具按同一口径映射，
    // 不能只认 UNIQUE/FK（批内守卫在单行表上以 NOT NULL 形式触发）。
    let constraint = error.as_database_error().is_some_and(|error| {
        matches!(
            error.kind(),
            sqlx::error::ErrorKind::UniqueViolation
                | sqlx::error::ErrorKind::ForeignKeyViolation
                | sqlx::error::ErrorKind::NotNullViolation
                | sqlx::error::ErrorKind::CheckViolation
                | sqlx::error::ErrorKind::ExclusionViolation
        )
    });
    if constraint {
        SdkError::Constraint("local fixture rejected statement".into())
    } else {
        SdkError::Error("local fixture rejected statement".into())
    }
}

#[async_trait]
impl RemoteTransport for SqliteTransport {
    async fn sql_values(&self, spec: &StatementSpec) -> turso_serverless::Result<Vec<Vec<Value>>> {
        Ok(query(spec)
            .fetch_all(&self.pool)
            .await
            .map_err(error_of)?
            .iter()
            .map(values)
            .collect())
    }

    async fn managed_batch(
        &self,
        statements: Vec<StatementSpec>,
    ) -> turso_serverless::Result<Vec<u64>> {
        self.writes.fetch_add(1, Ordering::SeqCst);
        if self.change_schema.swap(false, Ordering::SeqCst) {
            sqlx::query("CREATE TABLE late_extension (id TEXT)")
                .execute(&self.pool)
                .await
                .map_err(error_of)?;
        }
        // 守卫不跨 await（否则这个 future 不是 `Send`）。
        let concurrent = self.before_batch.lock().unwrap().take();
        if let Some(sql) = concurrent {
            // 语句文本来自本文件与同模块测试的静态夹具，不含外部输入。
            sqlx::raw_sql(sqlx::AssertSqlSafe(sql))
                .execute(&self.pool)
                .await
                .map_err(error_of)?;
        }
        let mut connection = self.pool.acquire().await.map_err(error_of)?;
        let mut transaction = connection
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(error_of)?;
        let mut counts = Vec::new();
        for (index, statement) in statements.iter().enumerate() {
            let fail = (statement.sql == "ALTER TABLE threads DROP COLUMN cached_context"
                && self.fail_column_drop.swap(false, Ordering::SeqCst))
                || (statement.sql == "DROP TABLE IF EXISTS session_control_state"
                    && self.fail_recovery_drop.swap(false, Ordering::SeqCst));
            let result = if fail {
                sqlx::query("ALTER TABLE threads DROP COLUMN absent_column")
                    .execute(&mut *transaction)
                    .await
            } else {
                query(statement).execute(&mut *transaction).await
            };
            match result {
                Ok(result) => counts.push(result.rows_affected()),
                Err(error) => {
                    transaction.rollback().await.map_err(error_of)?;
                    return Err(SdkError::BatchStatementFailed {
                        index,
                        error: Box::new(error_of(error)),
                        results: Vec::new(),
                    });
                }
            }
        }
        transaction.commit().await.map_err(error_of)?;
        if self.drop_reply.swap(false, Ordering::SeqCst) {
            return Err(SdkError::Http("simulated lost upgrade response".into()));
        }
        if self.truncate_reply.swap(false, Ordering::SeqCst) {
            counts.pop();
        }
        Ok(counts)
    }

    async fn consistent_read(
        &self,
        statements: Vec<StatementSpec>,
    ) -> turso_serverless::Result<Vec<Vec<Vec<Value>>>> {
        self.read_batches.fetch_add(1, Ordering::SeqCst);
        let mut connection = self.pool.acquire().await.map_err(error_of)?;
        let mut transaction = connection
            .begin_with("BEGIN DEFERRED")
            .await
            .map_err(error_of)?;
        let mut results = Vec::new();
        for statement in statements {
            if statement.sql.contains("FROM pragma_table_xinfo") {
                return Err(SdkError::Error(
                    "v1 Hrana does not support table-valued pragma_table_xinfo".into(),
                ));
            }
            let rows = query(&statement)
                .fetch_all(&mut *transaction)
                .await
                .map_err(error_of)?;
            results.push(rows.iter().map(values).collect());
        }
        transaction.commit().await.map_err(error_of)?;
        Ok(results)
    }

    fn is_autocommit(&self) -> turso_serverless::Result<bool> {
        Ok(true)
    }
    async fn close(&self) -> turso_serverless::Result<()> {
        Ok(())
    }
}

pub(super) struct Fixture {
    _directory: tempfile::TempDir,
    pub(super) transport: Arc<SqliteTransport>,
    pub(super) snapshot: StoreSnapshot,
}

impl Fixture {
    /// 压缩前契约的库：登记语义的 `workspaces`、执行环境表、machine 作用域凭证，
    /// 外加迁移要清掉的退役对象（`thread_goals`、`execution_runs`、旧缓存列）。
    pub(super) async fn new() -> Self {
        Self::new_with_foreign_keys(false).await
    }

    /// 连接级父行检查可打开的同款夹具：只给「`DROP TABLE` 的隐式删除真的会级联」这条失败
    /// 模式当观察点用。远端服务端的读数默认是 0，夹具默认也保持 0；这里打开它是在本地把
    /// 「归位失效」这一档变成可控实验，不是在声明远端会强制外键。
    pub(super) async fn new_with_foreign_keys(enforce: bool) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let options = sqlx::sqlite::SqliteConnectOptions::new()
            .filename(directory.path().join("remote.db"))
            .create_if_missing(true)
            .foreign_keys(enforce);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await
            .unwrap();
        for statement in V10_CREATE_TABLES {
            sqlx::query(*statement).execute(&pool).await.unwrap();
        }
        for statement in V10_CREATE_INDEXES {
            sqlx::raw_sql(sqlx::AssertSqlSafe((*statement).to_owned()))
                .execute(&pool)
                .await
                .unwrap();
        }
        sqlx::query(schema::CREATE_STORE_META_SQL)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(super::ledger::CREATE_OP_LEDGER_SQL)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(LEGACY_GOALS_SQL).execute(&pool).await.unwrap();
        sqlx::query(LEGACY_EXECUTION_SQL)
            .execute(&pool)
            .await
            .unwrap();
        // 语句文本来自本文件的静态夹具，不含外部输入。
        sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
            "ALTER TABLE threads ADD COLUMN cached_context TEXT;
            ALTER TABLE threads ADD COLUMN context_cache_epoch INTEGER NOT NULL DEFAULT 0;
            INSERT INTO peri_store_meta VALUES (0, 10, 'existing-store', '{PREVIOUS_CONTRACT}', 'original timestamp');
            INSERT INTO peri_op_ledger VALUES ('operation', 'append', 'digest', 'applied', 'receipt', 'original timestamp');
            INSERT INTO threads (id, cwd, created_at, updated_at, config, cached_context, context_cache_epoch) VALUES ('session', '/tmp', '2020-01-01T00:00:00Z', '2020-01-01T00:00:00Z', '{{\"model\":\"keep\"}}', 'old cache', 5);
            INSERT INTO thread_goals VALUES ('session', 'goal', 'old goal', 'active', NULL, 0, 0, 1, 1);
            INSERT INTO messages (rowid, message_id, thread_id, role, content, excluded, projection) VALUES (7, 'message', 'session', 'user', 'original content', 1, 'original projection');
            INSERT INTO execution_runs VALUES ('session', 7, 0);
            INSERT INTO projects VALUES ('project', '/tmp', 'identity');
            INSERT INTO workspaces VALUES ('11111111-1111-4111-8111-111111111111', 'project', '/tmp', 'root identity', '{DISCOVERY}');
            INSERT INTO session_bindings VALUES ('session', 1, 'project', '11111111-1111-4111-8111-111111111111', '');
            INSERT INTO session_environments VALUES ('session', 'original machine');
            INSERT INTO mcp_oauth_credentials VALUES ('principal', 'original machine', 'server', 'credential bytes', 'timestamp');
            UPDATE threads SET frozen_context = 'frozen bytes', inherited_context = 'inherited bytes', snapshot_at_message_id = 'message', agent_status = 'done', hidden = 1, cancel_policy = 'detach', message_count = 1"
        )))
        .execute(&pool)
        .await
        .unwrap();
        let transport = Arc::new(SqliteTransport {
            pool,
            writes: AtomicUsize::new(0),
            read_batches: AtomicUsize::new(0),
            fail_column_drop: AtomicBool::new(false),
            fail_recovery_drop: AtomicBool::new(false),
            change_schema: AtomicBool::new(false),
            before_batch: Mutex::new(None),
            drop_reply: AtomicBool::new(false),
            truncate_reply: AtomicBool::new(false),
        });
        let store = Self::store_for(&transport, StoreAccess::ReadWrite);
        let StoreIdentityRead::Present(snapshot) = store.read_identity().await.unwrap() else {
            panic!("missing fixture identity")
        };
        Self {
            _directory: directory,
            transport,
            snapshot,
        }
    }

    /// v14 形状的建表与索引（v15 之前的一代：`projects` 带 `object_identity`、
    /// `workspaces` 带 `identity`、`session_bindings` 带 `evidence_origin`，两张
    /// inherited 派生索引表在位）。夹具不能拿当前 DDL 生成，否则测不到升级。
    pub(super) const V14_TABLES: &[&str] = &[
        "CREATE TABLE IF NOT EXISTS machines (
            id TEXT PRIMARY KEY, name TEXT NOT NULL, identity_kind TEXT NOT NULL)",
        "CREATE TABLE IF NOT EXISTS projects (
            id TEXT PRIMARY KEY, locator TEXT NOT NULL, object_identity TEXT NOT NULL,
            UNIQUE(locator, object_identity))",
        "CREATE TABLE IF NOT EXISTS workspaces (
            id TEXT PRIMARY KEY, machine_id TEXT NOT NULL REFERENCES machines(id),
            path TEXT NOT NULL, path_source TEXT NOT NULL, project_id TEXT REFERENCES projects(id),
            identity TEXT, discovery TEXT, UNIQUE(machine_id, path))",
        "CREATE TABLE IF NOT EXISTS threads (
            id TEXT PRIMARY KEY, title TEXT, cwd TEXT NOT NULL DEFAULT '',
            created_at TEXT NOT NULL, updated_at TEXT NOT NULL,
            message_count INTEGER NOT NULL DEFAULT 0, parent_thread_id TEXT,
            snapshot_at_message_id TEXT, hidden BOOLEAN NOT NULL DEFAULT 0,
            cancel_policy TEXT NOT NULL DEFAULT 'cascade', config TEXT,
            frozen_context TEXT, inherited_context TEXT, agent_status TEXT NOT NULL DEFAULT 'active',
            workspace_id TEXT NOT NULL REFERENCES workspaces(id),
            archived BOOLEAN NOT NULL DEFAULT 0, context_usage_tokens INTEGER,
            work_state TEXT NOT NULL DEFAULT 'idle')",
        "CREATE TABLE IF NOT EXISTS messages (
            message_id TEXT PRIMARY KEY, thread_id TEXT NOT NULL REFERENCES threads(id),
            role TEXT NOT NULL, content TEXT NOT NULL, truncated BOOLEAN NOT NULL DEFAULT 0,
            excluded BOOLEAN NOT NULL DEFAULT 0, projection TEXT)",
        "CREATE TABLE IF NOT EXISTS session_bindings (
            thread_id TEXT PRIMARY KEY REFERENCES threads(id),
            schema_version INTEGER NOT NULL, project_id TEXT NOT NULL,
            workspace_id TEXT NOT NULL, relative_cwd TEXT NOT NULL,
            discovery_snapshot TEXT, evidence_origin TEXT NOT NULL)",
        "CREATE TABLE IF NOT EXISTS mcp_oauth_credentials (
            principal_id TEXT NOT NULL, workspace_id TEXT NOT NULL REFERENCES workspaces(id),
            server_key TEXT NOT NULL, credentials_blob TEXT NOT NULL, updated_at TEXT NOT NULL,
            PRIMARY KEY (principal_id, workspace_id, server_key))",
        "CREATE TABLE IF NOT EXISTS session_close_intents (
            thread_id TEXT PRIMARY KEY REFERENCES threads(id), requested_at TEXT NOT NULL)",
        "CREATE TABLE IF NOT EXISTS thread_mq (
            thread_id TEXT NOT NULL REFERENCES threads(id), id TEXT NOT NULL,
            enqueued_at TEXT NOT NULL, kind TEXT NOT NULL, source TEXT NOT NULL,
            policy_json TEXT NOT NULL, payload_json TEXT NOT NULL, original_draft TEXT,
            PRIMARY KEY(thread_id, id), UNIQUE(id))",
        "CREATE TABLE IF NOT EXISTS inherited_snapshot_index (
            snapshot_id TEXT PRIMARY KEY, thread_id TEXT NOT NULL, payload_bytes INTEGER NOT NULL,
            entry_count INTEGER NOT NULL, prepared_at TEXT NOT NULL)",
        "CREATE TABLE IF NOT EXISTS inherited_payload_index (
            snapshot_id TEXT NOT NULL, ordinal INTEGER NOT NULL, entry_id TEXT NOT NULL,
            raw_start INTEGER NOT NULL, raw_len INTEGER NOT NULL, logical_len INTEGER,
            PRIMARY KEY(snapshot_id, ordinal), UNIQUE(snapshot_id, entry_id))",
    ];

    /// 上一代当前形状（`v8` + 14）：v15 之前的当前形状，含要删除的三列与两张派生索引表。
    pub(super) async fn previous_generation() -> Self {
        let fixture = Self::built_from_v14_schema().await;
        sqlx::query(
            "UPDATE peri_store_meta SET schema_version = 14, contract = ?1 WHERE singleton = 0",
        )
        .bind(schema::PREVIOUS_CURRENT_STORE_CONTRACT)
        .execute(&fixture.transport.pool)
        .await
        .unwrap();
        Self {
            snapshot: Self::snapshot_of(&fixture.transport).await,
            ..fixture
        }
    }

    /// 上一代当前形状（`v7` + 13）：canonical 会话表齐全，缺 `threads` 的两列工作快照。
    pub(super) async fn previous_current() -> Self {
        let fixture = Self::built_from_v14_schema().await;
        sqlx::raw_sql(
            "ALTER TABLE threads DROP COLUMN context_usage_tokens;
             ALTER TABLE threads DROP COLUMN work_state;",
        )
        .execute(&fixture.transport.pool)
        .await
        .unwrap();
        sqlx::query(
            "UPDATE peri_store_meta SET schema_version = 13, contract = ?1 WHERE singleton = 0",
        )
        .bind(schema::LEGACY_CURRENT_STORE_CONTRACT)
        .execute(&fixture.transport.pool)
        .await
        .unwrap();
        Self {
            snapshot: Self::snapshot_of(&fixture.transport).await,
            ..fixture
        }
    }

    /// 再上一代当前形状（`v6` + 12）：还缺 `thread_mq`。
    pub(super) async fn legacy_current() -> Self {
        let fixture = Self::built_from_v14_schema().await;
        sqlx::raw_sql(
            "DROP TABLE thread_mq;
             ALTER TABLE threads DROP COLUMN context_usage_tokens;
             ALTER TABLE threads DROP COLUMN work_state;",
        )
        .execute(&fixture.transport.pool)
        .await
        .unwrap();
        sqlx::query(
            "UPDATE peri_store_meta SET schema_version = 12, contract = ?1 WHERE singleton = 0",
        )
        .bind(schema::OLDER_CURRENT_STORE_CONTRACT)
        .execute(&fixture.transport.pool)
        .await
        .unwrap();
        Self {
            snapshot: Self::snapshot_of(&fixture.transport).await,
            ..fixture
        }
    }

    /// 更早的当前形状（`v5` + 11）：还缺两张 inherited 派生索引表。
    pub(super) async fn older_current() -> Self {
        let fixture = Self::built_from_v14_schema().await;
        sqlx::raw_sql(
            "DROP TABLE inherited_payload_index;
             DROP TABLE inherited_snapshot_index;
             DROP TABLE thread_mq;
             ALTER TABLE threads DROP COLUMN context_usage_tokens;
             ALTER TABLE threads DROP COLUMN work_state;",
        )
        .execute(&fixture.transport.pool)
        .await
        .unwrap();
        sqlx::query(
            "UPDATE peri_store_meta SET schema_version = 11, contract = ?1 WHERE singleton = 0",
        )
        .bind(schema::OLDEST_CURRENT_STORE_CONTRACT)
        .execute(&fixture.transport.pool)
        .await
        .unwrap();
        Self {
            snapshot: Self::snapshot_of(&fixture.transport).await,
            ..fixture
        }
    }

    /// v14 形状的空库 + 一个项目行 + 元数据（版本 11 / `v5`，与旧夹具一致）。
    async fn built_from_v14_schema() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let options = sqlx::sqlite::SqliteConnectOptions::new()
            .filename(directory.path().join("remote.db"))
            .create_if_missing(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await
            .unwrap();
        for statement in Self::V14_TABLES {
            sqlx::query(sqlx::AssertSqlSafe((*statement).to_owned()))
                .execute(&pool)
                .await
                .unwrap();
        }
        sqlx::query(schema::CREATE_STORE_META_SQL)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO peri_store_meta VALUES (0, 11, 'existing-store', ?1, 'original timestamp')",
        )
        .bind(schema::OLDEST_CURRENT_STORE_CONTRACT)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query("INSERT INTO projects VALUES ('project', 'locator', 'identity');")
            .execute(&pool)
            .await
            .unwrap();
        let transport = Arc::new(SqliteTransport::for_test(pool));
        let snapshot = Self::snapshot_of(&transport).await;
        Self {
            _directory: directory,
            transport,
            snapshot,
        }
    }

    async fn snapshot_of(transport: &Arc<SqliteTransport>) -> StoreSnapshot {
        let store = Self::store_for(transport, StoreAccess::ReadWrite);
        let StoreIdentityRead::Present(snapshot) = store.read_identity().await.unwrap() else {
            panic!("missing fixture identity")
        };
        snapshot
    }

    fn store_for(transport: &Arc<SqliteTransport>, access: StoreAccess) -> RemoteStore {
        let gate = Arc::new(ConnectionGate::default());
        RemoteStore::new(transport.clone(), access, gate.mint(), gate)
    }

    pub(super) fn store(&self, access: StoreAccess) -> RemoteStore {
        Self::store_for(&self.transport, access)
    }

    /// 把夹具标成另一个代数与契约（构造拒绝面与 v11 输入）。
    pub(super) async fn declare(&self, version: i64, contract: &str) {
        sqlx::query("UPDATE peri_store_meta SET schema_version = ?1, contract = ?2")
            .bind(version)
            .bind(contract)
            .execute(&self.transport.pool)
            .await
            .unwrap();
    }

    pub(super) async fn version(&self) -> i64 {
        sqlx::query_scalar("SELECT schema_version FROM peri_store_meta")
            .fetch_one(&self.transport.pool)
            .await
            .unwrap()
    }

    pub(super) async fn contract(&self) -> String {
        sqlx::query_scalar("SELECT contract FROM peri_store_meta")
            .fetch_one(&self.transport.pool)
            .await
            .unwrap()
    }

    /// 计数断言（语句由调用点给出，不含外部输入）。
    pub(super) async fn count(&self, sql: String) -> i64 {
        sqlx::query_scalar(sqlx::AssertSqlSafe(sql))
            .fetch_one(&self.transport.pool)
            .await
            .unwrap()
    }

    /// 只读读回一份形状声明并判定：语句与判定都是生产打开路径用的那一份。
    pub(super) async fn shape_verdict(&self, probe: schema::ShapeProbe) -> StoreShapeRead {
        let rows: Vec<Vec<Value>> = sqlx::query(probe.sql())
            .fetch_all(&self.transport.pool)
            .await
            .unwrap()
            .iter()
            .map(values)
            .collect();
        let tables = schema::decode_column_map(&rows).unwrap();
        probe.check(&tables)
    }

    /// 升级前的旧事实：目标行、配置、旧缓存列都还在原位。
    pub(super) async fn assert_old_state(&self) {
        assert_eq!(self.version().await, 10);
        assert_eq!(self.contract().await, PREVIOUS_CONTRACT);
        let (goal,): (String,) = sqlx::query_as("SELECT objective FROM thread_goals")
            .fetch_one(&self.transport.pool)
            .await
            .unwrap();
        assert_eq!(goal, "old goal");
        let (config, cache, epoch): (String, String, i64) =
            sqlx::query_as("SELECT config, cached_context, context_cache_epoch FROM threads")
                .fetch_one(&self.transport.pool)
                .await
                .unwrap();
        assert_eq!(
            (config.as_str(), cache.as_str(), epoch),
            (r#"{"model":"keep"}"#, "old cache", 5)
        );
    }
}

/// 生产探测路径（`probe_shape`）的结论：真语句 + 真身份读取，与打开路径同一处接线。
pub(super) async fn probed_shape(store: &RemoteStore) -> Option<StoreShapeRead> {
    let read = store.read_identity().await.unwrap();
    probe_shape(store, &read).await.unwrap()
}

/// 退役的表与列在升级后消失，其余对象与行原样保留；版本与契约一次推进；终点形状 == canonical。
async fn assert_upgraded(fixture: &Fixture) {
    assert_eq!(fixture.version().await, schema::REMOTE_SCHEMA_VERSION);
    assert_eq!(fixture.contract().await, schema::STORE_CONTRACT);
    // 升级终点必须逐列等于 canonical（生产判定入口判定，不看建表文本）。
    assert_eq!(
        fixture.shape_verdict(schema::ShapeProbe::Current).await,
        StoreShapeRead::Consistent,
        "升级终点的会话表形状必须逐列等于 canonical"
    );
    // `DROP TABLE` 把被重建表上的索引一并删掉：终点要带齐整份 canonical 索引集。
    for index in [
        "idx_messages_thread_id",
        "idx_bindings_project",
        "idx_bindings_workspace",
        "idx_threads_updated",
        "idx_threads_workspace_archived",
        "idx_messages_active",
    ] {
        assert_eq!(
            fixture
                .count(format!(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type = 'index' AND name = '{index}'"
                ))
                .await,
            1,
            "{index} 必须在升级终点存在"
        );
    }
    assert_eq!(
        fixture
            .count("SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'session_environments'".to_owned())
            .await,
        0
    );
    assert_eq!(
        fixture
            .count("SELECT COUNT(*) FROM sqlite_master WHERE name IN ('execution_runs', 'thread_goals')".to_owned())
            .await,
        1,
        "退役的执行表清掉，业务扩展表 thread_goals 保留"
    );
    assert_eq!(
        fixture
            .count("SELECT COUNT(*) FROM pragma_table_xinfo('threads') WHERE name IN ('cached_context', 'context_cache_epoch')".to_owned())
            .await,
        0
    );
    // 结果守卫不误报：父行检查关闭时重建链不动历史，行数与搬到之前一致。
    assert_eq!(
        fixture
            .count("SELECT COUNT(*) FROM messages".to_owned())
            .await,
        1,
        "重建链没有清掉历史消息"
    );
}

include!("schema_upgrade_migration_test.rs");
