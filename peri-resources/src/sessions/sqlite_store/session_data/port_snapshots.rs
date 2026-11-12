impl SqliteSessionData {
    async fn load_snapshot_impl(&self, id: &ThreadId) -> SessionResourceResult<SessionSnapshot> {
        // 一次读取视图：同一连接上的延迟事务让 meta/binding/frozen/历史/继承区来自
        // 同一个数据库状态，不让调用方拼多次跨时刻查询。meta 走轻量投影（不含
        // 正文）：历史事实由 payloads 给出。
        let mut connection = self
            .database
            .pool
            .acquire()
            .await
            .map_err(|error| map_sqlx(&error))?;
        let mut tx = sqlx::Connection::begin(&mut *connection)
            .await
            .map_err(|error| map_sqlx(&error))?;
        let meta = context::load_meta_on(&mut tx, id, THREAD_META_COLUMNS)
            .await
            .map_err(read_failure)?;
        let parent = meta.parent_thread_id.clone();
        let binding = match binding_row_state_on(&mut tx, id).await? {
            BindingRowState::Bound(binding) => BindingState::Bound(binding),
            BindingRowState::Absent if parent.is_some() => BindingState::ExternalOrUnregistered,
            BindingRowState::Absent => BindingState::Missing,
        };
        let frozen = match frozen_bytes_on(&mut tx, id).await.map_err(read_failure)? {
            Some(bytes) => FrozenState::Present(FrozenSnapshotBytes::new(bytes)),
            None => FrozenState::LegacyAbsent,
        };
        let payloads = context::load_payloads_on(&mut tx, id)
            .await
            .map_err(read_failure)?;
        let flags = compaction::load_flags_on(&mut tx, id)
            .await
            .map_err(read_failure)?;
        let inherited = context::load_inherited_context_on(&mut tx, id, THREAD_META_COLUMNS)
            .await
            .map_err(read_failure)?;
        // 只读事务：没有写入效果可以证明，提交失败仍按原因分类，不判 `Unknown`。
        tx.commit().await.map_err(|error| map_sqlx(&error))?;
        Ok(SessionSnapshot {
            meta,
            binding,
            frozen,
            payloads,
            flags,
            inherited,
        })
    }

    async fn load_active_snapshot_impl(
        &self,
        id: &ThreadId,
    ) -> SessionResourceResult<ActiveSessionSnapshot> {
        // 与 `load_snapshot` 同一次读取视图、同一套分类规则，差别只有范围：
        // payload 与 flags 的读取条件都带 `excluded = 0`，已排除正文与已排除条目的
        // 标记都不进入内存（不先全量再过滤）。
        let mut connection = self
            .database
            .pool
            .acquire()
            .await
            .map_err(|error| map_sqlx(&error))?;
        let mut tx = sqlx::Connection::begin(&mut *connection)
            .await
            .map_err(|error| map_sqlx(&error))?;
        let meta = context::load_meta_on(&mut tx, id, THREAD_META_COLUMNS)
            .await
            .map_err(read_failure)?;
        let parent = meta.parent_thread_id.clone();
        let binding = match binding_row_state_on(&mut tx, id).await? {
            BindingRowState::Bound(binding) => BindingState::Bound(binding),
            BindingRowState::Absent if parent.is_some() => BindingState::ExternalOrUnregistered,
            BindingRowState::Absent => BindingState::Missing,
        };
        let frozen = match frozen_bytes_on(&mut tx, id).await.map_err(read_failure)? {
            Some(bytes) => FrozenState::Present(FrozenSnapshotBytes::new(bytes)),
            None => FrozenState::LegacyAbsent,
        };
        let payloads = context::load_active_payloads_on(&mut tx, id)
            .await
            .map_err(read_failure)?;
        let flags = compaction::load_active_flags_on(&mut tx, id)
            .await
            .map_err(read_failure)?;
        let inherited = context::load_inherited_context_on(&mut tx, id, THREAD_META_COLUMNS)
            .await
            .map_err(read_failure)?;
        tx.commit().await.map_err(|error| map_sqlx(&error))?;
        Ok(ActiveSessionSnapshot {
            meta,
            binding,
            frozen,
            inherited,
            payloads,
            flags,
        })
    }

    async fn save_child_impl(&self, child: &ChildSnapshot) -> SessionResourceResult<()> {
        self.writable()?;
        // 防御校验复用门面同一条规则：落库写的是 `target.meta` 里的父关系，声明与它不一致
        // 时必须在这里就停下，不能靠「父链核对」兜底——那条链查的是声明里的 parent。
        ensure_child_relation(child)?;
        let snapshot_at = child
            .target
            .meta
            .snapshot_at_message_id
            .map(|id| id.as_uuid().to_string());
        let inherited = child
            .inherited
            .to_json_with_spans()
            .map_err(|_| corrupt("inherited context is not serializable"))?;
        // 发布前校验引用边界，损坏的继承区不能落库。
        InheritedContext::from_json(&inherited.json)
            .map_err(|_| corrupt("inherited context has invalid message references"))?;
        let mut tx = self
            .database
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(|error| map_sqlx(&error))?;
        self.require_session(&mut tx, &child.parent_id).await?;
        if !thread_exists_on(&mut tx, &child.parent_id)
            .await
            .map_err(read_failure)?
        {
            return Err(not_found());
        }
        // 父子关系与根归属必须与库内事实一致：child 的 root 必须真是 parent 链的根。
        let parent_root = thread_root_on(&mut tx, &child.parent_id)
            .await
            .map_err(read_failure)?;
        if parent_root != child.root_id {
            return Err(invalid_input("child root does not match its parent chain"));
        }
        // frozen 必须逐字节来自 root 的已保存快照：不重新扫描目录，也不重新冻结。
        let root_frozen = frozen_bytes_on(&mut tx, &child.root_id)
            .await
            .map_err(read_failure)?;
        if root_frozen.as_deref() != Some(child.target.frozen.as_str()) {
            return Err(invalid_input(
                "child frozen snapshot must be the root's saved snapshot",
            ));
        }
        // 子会话继承父会话的执行绑定身份，不另立 workspace 归属。
        match binding_row_state_on(&mut tx, &child.parent_id).await? {
            BindingRowState::Bound(parent_binding) if parent_binding != child.target.binding => {
                return Err(SessionResourceError::new(
                    SessionResourceErrorKind::Workspace(WorkspaceError::ExecutionBindingMismatch),
                ));
            }
            _ => {}
        }
        insert_thread_row(
            &mut tx,
            &new_session_row(
                &child.target,
                snapshot_at.as_deref(),
                Some(child.target.frozen.as_str()),
                0,
            ),
        )
        .await
        .map_err(write_failure)?;
        insert_binding_row(&mut tx, &child.target.thread_id, &child.target.binding)
            .await
            .map_err(write_failure)?;
        sqlx::query("UPDATE threads SET inherited_context = ?1 WHERE id = ?2")
            .bind(&inherited.json)
            .bind(child.target.thread_id.as_str())
            .execute(&mut *tx)
            .await
            .map_err(|error| map_sqlx(&error))?;
        // `inherited_context` 字节是唯一事实源：v15 起不再派生索引（可从本列重建）。
        tx.commit()
            .await
            .map_err(|_| commit_failure(Some(child.target.thread_id.clone())))?;
        Ok(())
    }
}
