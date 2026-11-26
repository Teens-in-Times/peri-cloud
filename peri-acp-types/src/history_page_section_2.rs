/// 一段 canonical 完整回放（未协商 `peri.historyPagination` 的标准客户端路径）。
///
/// 段内载荷按 canonical 顺序给出：继承区在前（**只在首段**出现），其后是自有行按 `rowid`
/// 升序。**不过滤任何条目**：已排除的归档原文、System 条目、model-only reminder 都在其中——
/// 这是完整历史，可展示性与投影是调用方（ACP 出口）的事。
#[derive(Clone, Debug)]
pub struct HistorySegment {
    pub payloads: Vec<PersistedPayload>,
    /// `None` = 本次回放**没有更多自有行**了（已到固定上界，或本来就没有行）。
    ///
    /// 它不表示「本次读取失败」或「后端不支持」：那两种情形都必须显式失败，不能用一个空段
    /// 冒充回放结束。
    pub next_cursor: Option<HistoryForwardCursor>,
}

/// 一段的读取计划（阶段一的产物）：**先按长度定段，再只取这些行的正文**。
///
/// 这是两个存储 adapter 共用的唯一一份段规则（与 [`HistoryPagePlan`] 同构，方向相反）：
/// 候选行按存储序**升序**给入，本类型决定本段推进哪些行、游标停在谁之后、更晚方向还有没有
/// 内容。
///
/// 规则：
///
/// 1. 一次最多推进 `limit` 行；调用方多给一条候选行（最多 `limit + 1` 行），用来判断更晚
///    方向是否还有内容。判据是**未过滤的**行数，因此段里不会出现「读到的比推进的多」。
/// 2. 字节预算按**被推进的每一行**计（完整回放不做可展示性过滤，所以每一行都要读正文）。
///    预算之外只多留一条：整行原文就超预算的那条留给下一段，本段已推进的行不受影响；
///    第一条就超预算时独占一段（完整回放不允许丢内容），仍然推进并给出游标——否则大条目
///    之后的历史永远读不到。
/// 3. `limit` 必须为正：`limit = 0` 的段既不推进任何行也铸不出游标（`after` 不存在），而
///    调用方会把「没有游标」读成「回放结束」——静默截断，因此 [`Self::materialize`] 失败
///    关闭（`InvalidInput`），不返回一个看起来正常的空段。
#[derive(Clone, Debug)]
pub struct HistorySegmentPlan {
    /// 本段推进的行（升序存储序），也是阶段二**唯一**允许取正文的集合。
    entries: Vec<HistoryPageCandidate>,
    /// 更晚方向是否还有内容（`false` ⇒ `next_cursor = None`）。
    has_more: bool,
}

impl HistorySegmentPlan {
    /// 按长度决定本段推进哪些行（阶段一，不解码任何正文）。
    ///
    /// `candidates` 必须按存储序**升序**给入，最多 `limit + 1` 行（多出的那一条只用于
    /// 「更晚方向还有没有内容」）。
    pub fn plan(candidates: Vec<HistoryPageCandidate>, limit: usize, byte_budget: usize) -> Self {
        let mut entries: Vec<HistoryPageCandidate> = Vec::new();
        let mut remaining = byte_budget;
        for candidate in &candidates {
            if entries.len() >= limit {
                break;
            }
            // 已经有条目在手时，整行原文就超预算的不再收下：本段到此为止，游标停在上一条
            // 已推进的行上，这条留给下一段。
            if !entries.is_empty() && candidate.encoded_bytes > remaining {
                break;
            }
            remaining = remaining.saturating_sub(candidate.encoded_bytes);
            entries.push(*candidate);
        }
        // 只有「候选行全部推进完」且「没有多出来的那一条」才表示更晚方向确实没有内容。
        let exhausted = entries.len() == candidates.len() && candidates.len() <= limit;
        Self {
            entries,
            has_more: !exhausted,
        }
    }

    /// 阶段二**唯一**允许取正文的条目集合：就是本段那些行的 id。
    ///
    /// 多读的那一条候选行不在其中；它的正文永远不该被读出来。
    pub fn body_ids(&self) -> Vec<MessageId> {
        self.entries.iter().map(|entry| entry.message_id).collect()
    }

    /// 阶段二：把本段的正文与首段的继承区交给段（两个存储 adapter 共用的唯一装配实现）。
    ///
    /// - `inherited`：继承区 payload，**只在首段非空**（来自单行 `inherited_context` 快照）；
    ///   继承区在本切片不分页，因此它整份跟在首段最前面。
    /// - `tail`：本次回放的固定上界文本。首段由读取给出、后续段透传游标里的值；只有确实
    ///   还有更晚内容时才会用到它，那时它必须存在——缺失即读取不完整，不当成「没有更多」。
    /// - `rows`：阶段二按 [`Self::body_ids`] 取回的本段正文行，集合与顺序都必须一致。长度
    ///   或身份不符即损坏：把少读/多读的行静默装配成一段，等于把一次不完整读取当成正常回放
    ///   （完整回放尤其不能丢内容）。长度与位置取阶段一的事实（段预算当初就是按它判定的）。
    pub fn materialize(
        self,
        inherited: Vec<PersistedPayload>,
        tail: Option<&str>,
        rows: Vec<HistoryPageRow>,
    ) -> SessionResourceResult<HistorySegment> {
        if self.entries.is_empty() && self.has_more {
            return Err(invalid_input("history segment limit must be positive"));
        }
        if rows.len() != self.entries.len() {
            return Err(corrupt(
                "history segment bodies do not match the planned segment size",
            ));
        }
        let mut payloads = inherited;
        let paired_planned = rows.iter().zip(self.entries.iter());
        for (row, planned) in paired_planned {
            if row.message_id != planned.message_id {
                return Err(corrupt(
                    "history segment bodies are not the planned segment entries",
                ));
            }
            payloads.push(decode_row(row)?);
        }
        let next_cursor = match (self.has_more, self.entries.last()) {
            (true, Some(last)) => Some(HistoryForwardCursor::new(
                last.position.to_string(),
                tail.ok_or_else(|| corrupt("history segment tail is missing"))?,
            )),
            _ => None,
        };
        Ok(HistorySegment {
            payloads,
            next_cursor,
        })
    }
}

/// 持久化行里的条目 id 文本 → [`MessageId`]：不是 uuid 即损坏。
///
/// id 的解析规则只有这一份（候选行、正文行、存储序键读取共用）：两边各解释一次就可能把
/// 同一条目的两个身份混起来。
pub fn message_id_of(message_id: &str) -> SessionResourceResult<MessageId> {
    Ok(uuid::Uuid::parse_str(message_id)
        .map_err(|_| corrupt("persisted payload message id is not a uuid"))?
        .into())
}

/// 行内 id 与载荷 id 必须一致：不一致是数据故障，不把两套身份混起来（与整段历史读取同规）。
fn decode_row(row: &HistoryPageRow) -> SessionResourceResult<PersistedPayload> {
    let payload = store::deserialize_persisted_payload(&row.content)
        .map_err(|_| corrupt("history entry is not decodable"))?;
    if payload.id() != row.message_id {
        return Err(corrupt("persisted payload message id mismatch"));
    }
    Ok(payload)
}

fn corrupt(detail: &str) -> SessionResourceError {
    SessionResourceError::new(SessionResourceErrorKind::Corrupt {
        detail: detail.to_owned(),
    })
}

/// 调用方给错了参数（段长为零）：不是数据故障，也不能降级成一次正常回答。
fn invalid_input(detail: &str) -> SessionResourceError {
    SessionResourceError::new(SessionResourceErrorKind::InvalidInput {
        detail: detail.to_owned(),
    })
}

#[cfg(test)]
#[path = "history_page_test.rs"]
mod tests;

// V1 窗口协议按职责拆在 `history_window`；这里保持既有公开模块路径的可用性，
// 让消费方既能从 `history_page` 找到分页/窗口的关联类型，也不改变旧类型语义。
pub use crate::history_window::{
    HistoryBodyChunk, HistoryBodyDescriptor, HistoryCheckpoint, HistoryCommitMetadata,
    HistoryCommittedTail, HistoryEntryId, HistoryInheritedSnapshot, HistoryInheritedWindow,
    HistoryReadTicket, HistoryReadTicketBinding, HistoryRewindInvalidation, HistoryRewindStatus,
    HistorySource, HistoryWindowAnchor, HistoryWindowCursor, HistoryWindowDirection,
    HistoryWindowError, HistoryWindowMetadata, HistoryWindowOpenPlan, HistoryWindowOpenRequest,
    HistoryWindowPage, HistoryWindowPageRequest, HistoryWindowRequest, HistoryWindowSeekAnchor,
    HistoryWindowSeekRequest,
};
