# tests

集成测试与端到端测试（跨 package 协作）。

**状态**：未开始。

## 与各 package 内部测试的区别

- **单元测试**：写在各 package 内（Rust `#[cfg(test)]`、Node `__tests__/` 等），不放这里
- **集成 / E2E**：跨 package、跨平台、需要真实文件系统的测试放这里

## 计划内容

- macOS：用临时目录构造样本文件 → 跑 SpotlightBackend（注意 Spotlight 索引延迟，需 `mdimport` 强制索引或等待）
- Windows：临时目录 → SystemIndex / Everything（同样索引延迟问题）
- 跨平台一致性测试：同一份 SearchIntent 在不同后端上结果差异
- 安全测试：Prompt Injection、权限分级、Tool registry 边界
