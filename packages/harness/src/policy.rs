//! Policy Engine：统一判断工具动作是否允许执行。

use crate::SupportedIntent;
use scout_search_backend::{FileActionKind, SearchIntent};

/// Harness 权限等级。
///
/// 分级与计划书 §8.1 对齐：L0/L1 默认允许，L2 预留给正文/OCR 读取，
/// L3 打开文件默认允许，L4 写操作需要确认，L5 删除在 MVP 阶段禁用。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum PermissionLevel {
    /// 系统只读操作。
    L0,
    /// 文件 metadata 读取与普通搜索。
    L1,
    /// 文件正文 / OCR 读取。
    L2,
    /// 打开文件或应用。
    L3,
    /// 复制 / 移动 / 重命名等文件写操作。
    L4,
    /// 删除或批量破坏性修改。
    L5,
}

/// 需要 Policy Engine 判断的动作。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PolicyAction {
    /// 显式系统只读操作。
    SystemRead,
    /// 显式 metadata 读取操作。
    MetadataRead,
    /// 显式正文 / OCR 读取操作。
    ContentRead,
    /// 某类 `SearchIntent` 对应的搜索或澄清动作。
    SearchIntent(SupportedIntent),
    /// 文件操作 intent 中的具体动作。
    FileAction(FileActionKind),
}

impl PolicyAction {
    /// 返回动作对应的权限等级。
    #[must_use]
    pub const fn permission_level(self) -> PermissionLevel {
        match self {
            Self::SystemRead | Self::SearchIntent(SupportedIntent::Clarify) => PermissionLevel::L0,
            Self::MetadataRead
            | Self::SearchIntent(
                SupportedIntent::FileSearch
                | SupportedIntent::MediaSearch
                | SupportedIntent::Refine,
            )
            | Self::FileAction(FileActionKind::Locate) => PermissionLevel::L1,
            Self::ContentRead => PermissionLevel::L2,
            Self::FileAction(FileActionKind::Open) => PermissionLevel::L3,
            Self::SearchIntent(SupportedIntent::FileAction)
            | Self::FileAction(
                FileActionKind::Copy | FileActionKind::Move | FileActionKind::Rename,
            ) => PermissionLevel::L4,
            Self::FileAction(FileActionKind::Delete) => PermissionLevel::L5,
        }
    }
}

impl From<FileActionKind> for PolicyAction {
    fn from(action: FileActionKind) -> Self {
        Self::FileAction(action)
    }
}

impl From<&SearchIntent> for PolicyAction {
    fn from(intent: &SearchIntent) -> Self {
        match intent {
            SearchIntent::FileAction(file_action) => Self::FileAction(file_action.action),
            _ => Self::SearchIntent(SupportedIntent::from_intent(intent)),
        }
    }
}

/// Policy Engine 的决策结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PolicyDecision {
    /// 允许直接执行。
    Allow,
    /// 需要用户明确确认后才能执行。
    RequireConfirmation,
    /// 拒绝执行，并携带面向开发者 / UI 的原因。
    Deny {
        /// 拒绝原因。
        reason: String,
    },
}

/// 默认权限策略引擎。
///
/// MVP 默认策略：
/// - L0/L1/L3：允许；
/// - L2/L4：需要确认；
/// - L5：拒绝，删除在 MVP 阶段硬禁用。
#[derive(Debug, Default, Clone, Copy)]
pub struct PolicyEngine;

impl PolicyEngine {
    /// 创建默认 Policy Engine。
    #[must_use]
    pub const fn new() -> Self {
        Self
    }

    /// 评估动作是否允许、需要确认或拒绝。
    #[must_use]
    pub fn evaluate(&self, action: &PolicyAction) -> PolicyDecision {
        match action.permission_level() {
            PermissionLevel::L0 | PermissionLevel::L1 | PermissionLevel::L3 => {
                PolicyDecision::Allow
            }
            PermissionLevel::L2 | PermissionLevel::L4 => PolicyDecision::RequireConfirmation,
            PermissionLevel::L5 => PolicyDecision::Deny {
                reason: "delete is disabled in MVP".to_owned(),
            },
        }
    }

    /// 判断动作是否需要用户确认。
    ///
    /// 被拒绝的动作返回 `false`；调用方应先使用 [`Self::evaluate`] 获取拒绝原因。
    #[must_use]
    pub fn require_confirmation(&self, action: &PolicyAction) -> bool {
        matches!(self.evaluate(action), PolicyDecision::RequireConfirmation)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use scout_search_backend::{
        FileAction, FileSearch, MediaSearch, MediaType, SchemaVersion, TargetRef,
    };

    fn engine() -> PolicyEngine {
        PolicyEngine::new()
    }

    #[test]
    fn delete_is_denied() {
        let decision = engine().evaluate(&PolicyAction::from(FileActionKind::Delete));
        assert_eq!(
            decision,
            PolicyDecision::Deny {
                reason: "delete is disabled in MVP".to_owned()
            }
        );
    }

    #[test]
    fn l4_file_writes_require_confirmation() {
        for action in [
            FileActionKind::Copy,
            FileActionKind::Move,
            FileActionKind::Rename,
        ] {
            let policy_action = PolicyAction::from(action);
            assert_eq!(
                engine().evaluate(&policy_action),
                PolicyDecision::RequireConfirmation
            );
            assert!(engine().require_confirmation(&policy_action));
        }
    }

    #[test]
    fn l3_open_is_allowed() {
        let action = PolicyAction::from(FileActionKind::Open);
        assert_eq!(engine().evaluate(&action), PolicyDecision::Allow);
        assert!(!engine().require_confirmation(&action));
    }

    #[test]
    fn l2_content_read_requires_confirmation() {
        let action = PolicyAction::ContentRead;
        assert_eq!(
            engine().evaluate(&action),
            PolicyDecision::RequireConfirmation
        );
        assert!(engine().require_confirmation(&action));
    }

    #[test]
    fn search_intents_default_to_l1_allow() {
        let file_search = SearchIntent::FileSearch(FileSearch {
            schema_version: SchemaVersion::V1,
            language: None,
            keywords: Some(vec!["budget".to_owned()]),
            extensions: None,
            file_type: None,
            location: None,
            modified_time: None,
            created_time: None,
            accessed_time: None,
            size: None,
            exclude_extensions: None,
            exclude_file_type: None,
            sort: None,
            limit: None,
        });
        let media_search = SearchIntent::MediaSearch(MediaSearch {
            schema_version: SchemaVersion::V1,
            language: None,
            media_type: MediaType::Audio,
            artist: None,
            title: None,
            album: None,
            genre: None,
            quality: None,
            duration: None,
            keywords: Some(vec!["song".to_owned()]),
            extensions: None,
            file_type: None,
            location: None,
            modified_time: None,
            created_time: None,
            accessed_time: None,
            size: None,
            exclude_extensions: None,
            exclude_file_type: None,
            sort: None,
            limit: None,
        });

        assert_eq!(
            engine().evaluate(&PolicyAction::from(&file_search)),
            PolicyDecision::Allow
        );
        assert_eq!(
            engine().evaluate(&PolicyAction::from(&media_search)),
            PolicyDecision::Allow
        );
    }

    #[test]
    fn file_action_intent_uses_inner_action() {
        let intent = SearchIntent::FileAction(FileAction {
            schema_version: SchemaVersion::V1,
            language: None,
            action: FileActionKind::Move,
            target_ref: TargetRef::Path {
                value: "/tmp/a.txt".to_owned(),
            },
            destination: Some("/tmp/b.txt".to_owned()),
            new_name: None,
            requires_confirmation: true,
        });

        assert_eq!(
            engine().evaluate(&PolicyAction::from(&intent)),
            PolicyDecision::RequireConfirmation
        );
    }
}
