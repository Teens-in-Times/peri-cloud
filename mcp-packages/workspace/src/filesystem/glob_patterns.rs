//! glob 过滤器编译的单一事实源在 `peri-tool-runtime::filesystem::glob_patterns`；
//! 这里只做转发（executor review 2026-10-26 P0-1b：消除双拷贝漂移面）。

#[allow(unused_imports)]
pub(crate) use glob::Pattern;
#[allow(unused_imports)]
pub(crate) use peri_tool_runtime::filesystem::glob_patterns::*;

#[cfg(test)]
#[path = "glob_patterns_test.rs"]
mod tests;
