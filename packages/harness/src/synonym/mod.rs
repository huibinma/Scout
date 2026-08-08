//! 同义词关键词扩展（BETA-11）。
//!
//! 在 `IntentRouter` 之后、`SearchBackend` 之前把单 keyword 扩成等价词组。

pub mod expander;
pub mod yaml;

pub use expander::{NoopExpander, SynonymExpander};
pub use yaml::{ExpanderError, YamlSynonymExpander};

pub mod user;
pub use user::{LayeredSynonymExpander, UserDictError, UserGroup, UserIndex};
