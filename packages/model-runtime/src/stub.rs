use crate::{GenerateParams, LlamaModelRuntime, ModelError, ModelLoadParams, ModelLoader};
use std::path::Path;

/// 一个简单的回声加载器，用于测试和未集成 llama.cpp 时的占位
#[derive(Debug)]
pub struct StubLoader;

impl ModelLoader for StubLoader {
    fn load(
        &self,
        _path: &Path,
        _params: &ModelLoadParams,
    ) -> Result<Box<dyn LlamaModelRuntime>, ModelError> {
        Ok(Box::new(StubModel))
    }
}

#[derive(Debug)]
pub struct StubModel;

impl LlamaModelRuntime for StubModel {
    fn generate(&self, prompt: &str, _params: &GenerateParams) -> Result<String, ModelError> {
        Ok(format!("Echo: {prompt}"))
    }
}
