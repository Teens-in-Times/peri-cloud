//! Write 失败草稿存储的单一事实源在 `peri-tool-runtime::filesystem::draft`；
//! 这里只做转发（P0-1b）。

#[allow(unused_imports)]
pub(crate) use peri_tool_runtime::filesystem::draft::{
    draft_hint_en, draft_hint_zh, DraftAccessError, DraftStore,
};
