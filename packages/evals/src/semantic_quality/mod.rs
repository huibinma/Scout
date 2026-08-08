//! BETA-15B-6：语义召回质量评测（合成语料 + 缓存向量 + 三臂排名指标）。
//! 全部读 checked-in 合成数据（无 PII）；hybrid 跑生产融合。

pub mod arms;
pub mod data;
pub mod metrics;
pub mod report;

/// 评测相似度下限：复刻生产语义臂融合前过滤（`DEFAULT_SIMILARITY_FLOOR=0.30`）。
pub const EVAL_SIMILARITY_FLOOR: f32 = 0.30;
/// 排名截断 k（指标 @k）。
pub const TOP_K: usize = 10;
