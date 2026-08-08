//! Parser 模块组：5 个 SearchIntent variant 各自的规则解析器。
//!
//! 共享 helper 在 [`common`]。每个 parser 模块自带 `#[cfg(test)] mod tests`。

pub(crate) mod artist;
pub mod clarify;
pub mod common;
pub mod file_action;
pub mod file_search;
pub mod media_search;
pub mod refine;
