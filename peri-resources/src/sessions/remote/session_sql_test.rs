use super::*;

/// 远端两条撤销入口的判据点：write-once（fork 补偿）不叠加 frozen 判据，
/// 两阶段草稿的三条删除全部带 `frozen_context IS NULL`。
#[test]
fn revocation_statements_split_by_entry_point() {
    // 两阶段草稿：判据在每一条删除上（同一批内不会出现「历史删了、会话行还在」）。
    let draft = revoke_draft_statements("s");
    assert_eq!(draft.len(), 3);
    for statement in &draft {
        assert!(
            statement.sql.contains("frozen_context IS NULL"),
            "草稿撤销的每条删除都必须带未提交判据: {}",
            statement.sql
        );
        assert!(
            statement.sql.starts_with("DELETE"),
            "只允许删除语句: {}",
            statement.sql
        );
    }
    // write-once：撤销未发布创建（fork 等）不叠加 frozen 判据——目标创建即带 frozen，
    // 补偿就是把它整条删掉。
    let write_once = revoke_session_statements("s");
    assert_eq!(write_once.len(), 3);
    for statement in &write_once {
        assert!(
            !statement.sql.contains("frozen_context"),
            "write-once 撤销不得叠加 frozen 判据: {}",
            statement.sql
        );
    }
}

/// inherited 派生索引删除必须排在会话行删除之前；缺表时跳过，不改变原有撤销语句。
#[test]
fn inherited_index_deletes_precede_thread_row_deletion() {
    let effects =
        with_inherited_index_deletes(revoke_session_statements("s"), "s", true, true, false);
    let sqls: Vec<&str> = effects.iter().map(|statement| statement.sql).collect();
    assert_eq!(effects.len(), 5);
    assert_eq!(
        sqls[2],
        crate::sessions::canonical::DELETE_INHERITED_PAYLOADS_BY_THREAD_SQL
    );
    assert_eq!(
        sqls[3],
        crate::sessions::canonical::DELETE_INHERITED_SNAPSHOT_BY_THREAD_SQL
    );
    assert_eq!(sqls[4], DELETE_SESSION_SQL);

    let draft = with_inherited_index_deletes(revoke_draft_statements("s"), "s", true, true, true);
    for statement in &draft[2..4] {
        assert!(
            statement.sql.contains("frozen_context IS NULL"),
            "草稿撤销的派生索引删除必须与线程删除共用未提交判据: {}",
            statement.sql
        );
    }
    assert!(
        draft.last().unwrap().sql.starts_with("DELETE FROM threads"),
        "线程删除必须留在批次末尾（撤销流程按 counts.last() 判定）"
    );

    assert_eq!(
        with_inherited_index_deletes(revoke_session_statements("s"), "s", false, true, false).len(),
        3,
        "缺 snapshot 表时不能插入任何派生删除"
    );
    let snapshot_only =
        with_inherited_index_deletes(revoke_session_statements("s"), "s", true, false, false);
    assert_eq!(snapshot_only.len(), 4);
    assert_eq!(
        snapshot_only[2].sql,
        crate::sessions::canonical::DELETE_INHERITED_SNAPSHOT_BY_THREAD_SQL
    );
    assert_eq!(snapshot_only[3].sql, DELETE_SESSION_SQL);
}

/// 回退候选语句形状：阶段一 user 过滤 + keyset + LIMIT，阶段二按主键单行；
/// 参数全部走绑定（静态 SQL 不插值）。
#[test]
fn rewind_candidate_statements_are_user_filtered_bounded_and_parameterized() {
    let first = rewind_candidate_rows_statement("s", None, 128);
    assert!(first.is_read_only(), "阶段一必须是只读语句");
    assert!(first.sql.contains("m.role = 'user'"));
    assert!(first.sql.contains("ORDER BY m.rowid DESC LIMIT ?3"));
    assert!(!first.sql.contains("excluded"), "归档 user 仍是合法目标");
    assert_eq!(
        first.params,
        vec![
            Value::Text("s".to_owned()),
            Value::Null,
            Value::Integer(128)
        ]
    );

    let next = rewind_candidate_rows_statement("s", Some(42), 128);
    assert_eq!(
        next.params,
        vec![
            Value::Text("s".to_owned()),
            Value::Integer(42),
            Value::Integer(128)
        ],
        "keyset 游标只走绑定参数"
    );

    let body = rewind_candidate_body_statement("s", "m-1");
    assert!(body.is_read_only());
    assert!(body.sql.contains("m.message_id = ?2"));
    assert_eq!(
        body.params,
        vec![Value::Text("s".to_owned()), Value::Text("m-1".to_owned())]
    );
}
