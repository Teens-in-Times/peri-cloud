/// v14 → v15 的清理批次：三表重建（删列、收窄唯一键、投影绑定证据）+ 两张退役表 +
/// 索引重放 + 版本推进。
///
/// 与 additive / 搬运路径共用 [`v15_cleanup_statements`]：v14 的库不缺列，只差删除，因此
/// 这里不做 `ALTER`、不搬会话事实；v13/v12/v11 的增量批次在补完列与表之后接同一段语句。
///
/// 守卫链在破坏性语句之前：元数据行、绑定行计数、逐行绑定证据与登记行，任何一条不成立都让
/// 批次以唯一键冲突失败。
pub(super) fn v15_cleanup_plan(
    snapshot: &StoreSnapshot,
    read: &MigrationRead,
    current_machine: &str,
) -> SessionResourceResult<Vec<StatementSpec>> {
    if !v15_cleanup_source(snapshot) {
        return Err(unsupported());
    }
    let mut statements = vec![StatementSpec::new(
        GUARD_META_SQL,
        vec![
            Value::Integer(snapshot.schema_version),
            Value::Text(snapshot.store_id.as_str().to_owned()),
            Value::Text(snapshot.contract.clone()),
        ],
    )];
    // 绑定行的计数与逐行证据守卫由 `v15_cleanup_statements` 统一下发：additive 与
    // v15 清理共用同一段重建，守卫必须跟着重建走。
    // 登记行只存在于压缩前的输入里；当前形状（v14 → v15）没有它们，不能下发计数守卫。
    if read.has_registration_shape() {
        statements.push(StatementSpec::new(
            GUARD_REGISTRATION_COUNT_SQL,
            vec![Value::Integer(read.registrations.len() as i64)],
        ));
        for row in &read.registrations {
            statements.push(StatementSpec::new(GUARD_REGISTRATION_ROW_SQL, row.clone()));
        }
    }
    statements.extend(v15_cleanup_statements(
        current_machine,
        &read.binding_snapshots,
        &read.projects,
    )?);
    statements.push(advance_version(snapshot));
    Ok(statements)
}

// NOTE: additive 批次在补完 v14 的列与表后接 `v15_cleanup_statements`，一次推进到 v15；
// 绑定守卫也由该语句段统一下发，覆盖 additive 与 v14 清理两条路径。

/// v15 清理的语句段（不含元数据/登记守卫与版本推进）：先下发绑定守卫，再重建三表、
/// 删两表、重放索引。
///
/// `binding_snapshots` 是批外读到的 `(thread_id, discovery_snapshot)` 原文；这里逐行投影成
/// 最小执行根再插入暂存表——SQL 里不做这件事（远端 json1 未实测），两端因此在 Rust 侧共用
/// 同一份投影（`binding_evidence::project`）。解析失败即整批失败：静默降级成 NULL 会把
/// 可执行会话变成只读历史。
fn v15_cleanup_statements(
    current_machine: &str,
    binding_snapshots: &[(String, Option<String>)],
    projects: &[(String, String)],
) -> SessionResourceResult<Vec<StatementSpec>> {
    let mut projected = Vec::with_capacity(binding_snapshots.len());
    for (thread_id, snapshot) in binding_snapshots {
        let snapshot = match snapshot.as_deref() {
            None => Value::Null,
            Some(raw) => Value::Text(
                crate::sessions::binding_evidence::project(raw).map_err(|_| unsupported())?,
            ),
        };
        projected.push((thread_id.clone(), snapshot));
    }
    // `projects`：保留每个 locator 的 rowid 最早一行（`projects` 已按 rowid 升序读回），
    // 其余行的引用先改绑、后删除。这一步必须在重建之前：重建后的唯一键是
    // `UNIQUE(locator)`，重复行会让搬入直接撞键。
    let mut keep: HashMap<&str, &str> = HashMap::new();
    let mut merges: Vec<(&str, &str)> = Vec::new();
    for (id, locator) in projects {
        match keep.get(locator.as_str()) {
            Some(keep_id) => merges.push((id.as_str(), keep_id)),
            None => {
                keep.insert(locator.as_str(), id.as_str());
            }
        }
    }
    // 守卫链必须在任何破坏性语句之前：重建只搬批外读到的绑定行，批间新增或改写的行
    // 要以唯一键冲突失败，不能被静默丢掉或写入未投影的旧证据。
    let mut statements = vec![StatementSpec::new(
        GUARD_BINDING_COUNT_SQL,
        vec![Value::Integer(binding_snapshots.len() as i64)],
    )];
    for (thread_id, snapshot) in binding_snapshots {
        statements.push(StatementSpec::new(
            GUARD_BINDING_SNAPSHOT_SQL,
            vec![
                Value::Text(thread_id.clone()),
                snapshot.clone().map(Value::Text).unwrap_or(Value::Null),
            ],
        ));
    }
    for (drop_id, keep_id) in &merges {
        statements.push(StatementSpec::new(
            "UPDATE workspaces SET project_id = ?1 WHERE project_id = ?2",
            vec![
                Value::Text((*keep_id).to_owned()),
                Value::Text((*drop_id).to_owned()),
            ],
        ));
        statements.push(StatementSpec::new(
            "UPDATE session_bindings SET project_id = ?1 WHERE project_id = ?2",
            vec![
                Value::Text((*keep_id).to_owned()),
                Value::Text((*drop_id).to_owned()),
            ],
        ));
        statements.push(StatementSpec::new(
            "DELETE FROM projects WHERE id = ?1",
            vec![Value::Text((*drop_id).to_owned())],
        ));
    }
    // 1. projects 重建为 `UNIQUE(locator)`：此时 locator 已唯一，整表搬运即可。
    statements.push(StatementSpec::bare(
        canonical::CREATE_REBUILD_PROJECTS_TABLE_SQL,
    ));
    statements.push(StatementSpec::bare(
        canonical::INSERT_PROJECTS_INTO_REBUILD_SQL,
    ));
    // 2. workspaces：删 `identity`。归属与证据列原样搬运。
    statements.push(StatementSpec::bare(
        canonical::CREATE_REBUILD_WORKSPACES_TABLE_SQL,
    ));
    statements.push(StatementSpec::bare(
        canonical::INSERT_WORKSPACES_INTO_REBUILD_SQL,
    ));
    // 3. session_bindings：删 `evidence_origin`，证据逐行投影。
    statements.push(StatementSpec::bare(
        canonical::CREATE_REBUILD_BINDINGS_TABLE_SQL,
    ));
    for (thread_id, snapshot) in &projected {
        statements.push(StatementSpec::new(
            "INSERT INTO session_bindings_v15 (thread_id, schema_version, project_id, workspace_id, relative_cwd, discovery_snapshot)
             SELECT thread_id, schema_version, project_id, workspace_id, relative_cwd, ?2
             FROM session_bindings WHERE thread_id = ?1",
            vec![Value::Text(thread_id.clone()), snapshot.clone()],
        ));
    }
    // 4. 换表：先删引用者，再删父表，最后按 v15 DDL 建新表并搬回。
    statements.push(StatementSpec::bare("DROP TABLE session_bindings"));
    statements.push(StatementSpec::bare("DROP TABLE workspaces"));
    statements.push(StatementSpec::bare("DROP TABLE projects"));
    statements.push(StatementSpec::bare(canonical::CREATE_PROJECTS_TABLE_SQL));
    statements.push(StatementSpec::bare(canonical::CREATE_WORKSPACES_TABLE_SQL));
    statements.push(StatementSpec::bare(canonical::CREATE_BINDINGS_TABLE_SQL));
    statements.push(current_machine_statement(current_machine));
    statements.push(StatementSpec::bare(
        canonical::INSERT_REBUILD_INTO_PROJECTS_SQL,
    ));
    statements.push(StatementSpec::bare(
        canonical::INSERT_REBUILD_INTO_WORKSPACES_SQL,
    ));
    statements.push(StatementSpec::bare(
        canonical::INSERT_REBUILD_INTO_BINDINGS_SQL,
    ));
    statements.extend(
        [
            "DROP TABLE projects_v15",
            "DROP TABLE workspaces_v15",
            "DROP TABLE session_bindings_v15",
        ]
        .into_iter()
        .map(StatementSpec::bare),
    );
    // 5. 退役的两张派生索引表（payload 先于 snapshot）。缺表时 `IF EXISTS` 幂等。
    statements.extend(
        canonical::DROP_INHERITED_INDEX_TABLES
            .iter()
            .map(|sql| StatementSpec::bare(sql)),
    );
    // 6. 索引重放：重建的三张表随 `DROP TABLE` 丢掉了自己的索引。
    statements.extend(
        canonical::CREATE_INDEXES
            .iter()
            .map(|sql| StatementSpec::bare(sql)),
    );
    Ok(statements)
}
fn current_machine_statement(current_machine: &str) -> StatementSpec {
    StatementSpec::new(
        canonical::INSERT_CURRENT_MACHINE_SQL,
        vec![Value::Text(current_machine.to_owned())],
    )
}

/// v14 的 `threads` 增量列：增量批次与搬运重建都要在收尾前补齐，DDL 文本只在这里生成。
fn threads_v14_column_statements() -> impl Iterator<Item = StatementSpec> {
    canonical::THREADS_V14_COLUMNS
        .iter()
        .map(|(name, definition)| {
            StatementSpec::bare(Box::leak(
                format!("ALTER TABLE threads ADD COLUMN {name} {definition}").into_boxed_str(),
            ))
        })
}

/// 版本与契约一起推进：形状变了，代数跟着变。
fn advance_version(snapshot: &StoreSnapshot) -> StatementSpec {
    StatementSpec::new(
        ADVANCE_VERSION_SQL,
        vec![
            Value::Integer(schema::REMOTE_SCHEMA_VERSION),
            Value::Text(schema::STORE_CONTRACT.to_owned()),
            Value::Integer(snapshot.schema_version),
            Value::Text(snapshot.store_id.as_str().to_owned()),
            Value::Text(snapshot.contract.clone()),
        ],
    )
}

fn object_values(object: &SchemaObject) -> Vec<Value> {
    vec![
        Value::Text(object.kind.clone()),
        Value::Text(object.name.clone()),
        Value::Text(object.table.clone()),
        object.sql.clone().map(Value::Text).unwrap_or(Value::Null),
    ]
}

fn optional_value(value: &Option<String>) -> Value {
    match value {
        Some(value) => Value::Text(value.clone()),
        None => Value::Null,
    }
}

/// 会话行与绑定行 → 规划输入。
///
/// 会话的机器归属缺行时用 `legacy:<store id>` 兜底（本机以外的旧机器，与未知机器同类）；
/// 绑定记录的根取不到时按保存的 cwd 规划，规划只用于分组，绝不当成执行证据。
fn decode_legacy(
    session_rows: &[Vec<Value>],
    binding_rows: &[Vec<Value>],
    registration_rows: &[Vec<Value>],
    snapshot: &StoreSnapshot,
) -> SessionResourceResult<(Vec<LegacySession>, Vec<LegacyRegistration>)> {
    let registrations = decode_registrations(registration_rows)?;
    let mut bindings: std::collections::HashMap<String, (String, Option<String>)> =
        std::collections::HashMap::new();
    for row in binding_rows {
        let thread_id = text_at(row, 0).ok_or_else(unsupported)?.to_owned();
        // 绑定表的主键是 `thread_id`：读回重复行说明这不是本构建认识的形状（例如同名的
        // VIEW）。后行覆盖前行会让输出比输入少一行、版本却照常推进——显式拒绝。
        let replaced = bindings.insert(
            thread_id,
            (
                text_at(row, 3).ok_or_else(unsupported)?.to_owned(),
                optional_text(row, 4)?,
            ),
        );
        if replaced.is_some() {
            return Err(unsupported());
        }
    }
    let mut sessions = Vec::with_capacity(session_rows.len());
    for row in session_rows {
        let id = text_at(row, 0).ok_or_else(unsupported)?.to_owned();
        let parent_id = optional_text(row, 1)?;
        let cwd = text_at(row, 2).ok_or_else(unsupported)?.to_owned();
        let machine_id = optional_text(row, 3)?
            .unwrap_or_else(|| format!("legacy:{}", snapshot.store_id.as_str()));
        let binding = bindings.get(&id);
        let registration = binding
            .map(|(workspace_id, _)| {
                workspace_id
                    .parse::<WorkspaceId>()
                    .map_err(|_| unsupported())
            })
            .transpose()?;
        // 相对路径与保存的 cwd 推不出同一个根时，绑定记录的根不再可信：
        // 按保存的 cwd 分组（规划用途），绑定本身仍原样搬运并原样校验。
        let derived_root = binding
            .and_then(|(_, relative)| relative.as_deref())
            .and_then(|relative| derive_remote_root(&cwd, relative).ok());
        let plan_registration = if registration.is_some() && derived_root.is_none() {
            None
        } else {
            registration
        };
        sessions.push(LegacySession {
            id,
            parent_id,
            machine_id,
            cwd: cwd.into(),
            execution_workspace_id: plan_registration,
            derived_root,
        });
    }
    // 推不出根、又带着登记 id 的会话必须在别处有同路径的归属：否则规划会静默改绑，
    // 这里显式拒绝（与 `plan_local_workspaces` 的检查互补）。
    for session in &sessions {
        if let (Some(registration_id), Some(derived_root)) = (
            session.execution_workspace_id,
            session.derived_root.as_ref(),
        ) {
            if registrations
                .iter()
                .find(|registration| registration.id == registration_id)
                .is_some_and(|registration| &registration.root != derived_root)
            {
                return Err(unsupported());
            }
        }
    }
    Ok((sessions, registrations))
}

/// 登记行 → 规划输入：观测证据必须描述同一个根，否则这不是本构建认识的登记行。
fn decode_registrations(rows: &[Vec<Value>]) -> SessionResourceResult<Vec<LegacyRegistration>> {
    let mut registrations = Vec::with_capacity(rows.len());
    for row in rows {
        let id: WorkspaceId = text_at(row, 0)
            .ok_or_else(unsupported)?
            .parse()
            .map_err(|_| unsupported())?;
        let project_id = text_at(row, 1).ok_or_else(unsupported)?.to_owned();
        let root = text_at(row, 2).ok_or_else(unsupported)?.to_owned();
        let discovery = text_at(row, 3).ok_or_else(unsupported)?.to_owned();
        let observed: serde_json::Value =
            serde_json::from_str(&discovery).map_err(|_| unsupported())?;
        if observed.get("root").and_then(serde_json::Value::as_str) != Some(root.as_str()) {
            return Err(unsupported());
        }
        let path_source = if observed
            .get("common_dir")
            .is_some_and(|value| !value.is_null())
            || observed
                .get("private_dir")
                .is_some_and(|value| !value.is_null())
        {
            WorkspacePathSource::Discovered
        } else {
            WorkspacePathSource::Unverified
        };
        registrations.push(LegacyRegistration {
            id,
            root: root.into(),
            path_source,
            project_id,
            discovery,
        });
    }
    Ok(registrations)
}

/// 绑定行 → 搬运输入。
fn decode_bindings(rows: &[Vec<Value>]) -> SessionResourceResult<Vec<LegacyBindingRow>> {
    rows.iter()
        .map(|row| {
            Ok(LegacyBindingRow {
                thread_id: text_at(row, 0).ok_or_else(unsupported)?.to_owned(),
                schema_version: int_at(row, 1).ok_or_else(unsupported)?,
                project_id: text_at(row, 2).ok_or_else(unsupported)?.to_owned(),
                workspace_id: text_at(row, 3)
                    .ok_or_else(unsupported)?
                    .parse()
                    .map_err(|_| unsupported())?,
                relative_cwd: text_at(row, 4).ok_or_else(unsupported)?.to_owned(),
            })
        })
        .collect()
}
