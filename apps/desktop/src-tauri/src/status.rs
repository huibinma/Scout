use scout_harness::{BackendSummary, CapabilityDiscovery};
use tauri::State;

/**
 * 获取所有后端工具的状态摘要。
 *
 * 供前端状态栏显示各搜索后端的可用性、实现状态(Real/Stub)等。
 * registry 经 SearchDeps 取用(单一 managed 状态)。
 */
#[tauri::command]
pub fn get_backend_status(deps: State<'_, crate::search::SearchDeps>) -> Vec<BackendSummary> {
    let discovery = CapabilityDiscovery::new(deps.registry());
    discovery.backend_summary()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use scout_harness::context::ContextMemory;
    use scout_harness::file_action_tool::{FileActionTool, LocalFileActionExecutor};
    use scout_harness::{NoopExpander, PolicyEngine, SynonymExpander, ToolRegistry, Tracer};
    use std::sync::{Arc, Mutex};

    #[test]
    fn get_backend_status_reads_registry_via_searchdeps() {
        let deps = crate::search::SearchDeps::new(
            Arc::new(ToolRegistry::new()),
            Arc::new(PolicyEngine::new()),
            Arc::new(Tracer::with_hooks(vec![])),
            Arc::new(Mutex::new(ContextMemory::new())),
            Arc::new(FileActionTool::new(
                Arc::new(LocalFileActionExecutor),
                PolicyEngine::new(),
            )),
            Arc::new(Mutex::new(None)),
            Arc::new(NoopExpander) as Arc<dyn SynonymExpander>,
        );
        let summaries = CapabilityDiscovery::new(deps.registry()).backend_summary();
        assert!(
            summaries.is_empty(),
            "空 registry 摘要应为空, 实得 {summaries:?}"
        );
    }
}
