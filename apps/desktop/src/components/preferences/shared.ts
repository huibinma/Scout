// BETA-47：选项对话框共享类型与工具函数（原 PreferencesDialog.tsx 1579 行拆文件）。
// 各分类面板拆至同目录 *Pane.tsx；此处只放跨面板复用的类型 / 纯函数 / 分类表。

/**
 * 分类 key（2026-07-29 五 tab 改版：常规 / 索引 / 语义召回 / 术语与同义词 / 本机 MCP 服务）。
 * 内置原生索引 / Windows 系统集成收进「常规」内的子分区（不再是独立 tab，见 GeneralPane）；
 * 「隐私与记录」内容并入「索引」（索引概貌 + 数据/隐私管理同属"本机数据"这一件事，
 * 原「索引了什么」与隐私页的索引统计是同一份数据的重复展示，见 IndexingPane）。
 * `misc` 这个 key 因历史兼容保留，实际内容是「我的同义词」管理（见 SynonymsPane），
 * 显示名已改「术语与同义词」，更贴合实际内容。
 */
export type Category = "general" | "indexing" | "semantic" | "misc" | "mcp";

/** 当前是否 Windows（内置原生索引 / Windows 系统集成子分区仅 Windows 显示，见 GeneralPane）。 */
export const IS_WINDOWS =
  typeof navigator !== "undefined" && /Win/i.test(navigator.platform);

/** 分类表（渲染左侧分类树；Windows 专属内容已下沉进「常规」子分区，无需按平台过滤 tab 本身）。 */
export const CATEGORIES: { key: Category; label: string }[] = [
  { key: "general", label: "常规" },
  { key: "indexing", label: "索引" },
  { key: "semantic", label: "语义召回" },
  { key: "misc", label: "术语与同义词" },
  // BETA-53：本机 MCP 服务（跨平台）——让本机 LLM 客户端经 MCP 检索本机文件。
  { key: "mcp", label: "本机 MCP 服务" },
];

/** 字节数转人读单位（B / KB / MB），「数据存储位置」小节复用。 */
export function formatBytes(n: number): string {
  if (n < 1024) return `${n} B`;
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KB`;
  return `${(n / (1024 * 1024)).toFixed(1)} MB`;
}

export interface AuditEntry {
  timestamp: string;
  operation: string;
  source_paths: string[];
  destination: string | null;
  new_name: string | null;
  result: string;
  error: string | null;
}

/** cycle 7-a：索引阶段（后端 IndexPhase enum snake_case serialize）。 */
export type IndexPhase = "music_discovery" | "music_scan" | "doc" | "image";

/** 2026-07-28：单个 phase 的 walk/extract/write/recycle 耗时（毫秒），对应后端 `StageTimings`。 */
export interface StageTimings {
  walk_ms: number;
  extract_ms: number;
  write_ms: number;
  recycle_ms: number;
}

/** 2026-07-28：上一次完整 reindex 的分阶段耗时明细，对应后端 `RunStageTimings`。 */
export interface RunStageTimings {
  doc: StageTimings | null;
  image: StageTimings | null;
  music: StageTimings | null;
}

export interface IndexStatus {
  indexing: boolean;
  last_indexed: string | null;
  last_summary: string | null;
  /** cycle 6 v4：正在扫描的目录（bridge 更新为当前文件的父目录）；非索引中为 null。
   *  UI 文案叫「当前目录」（不是「索引根」，语义上是文件父目录、非配置 root）。 */
  current_root: string | null;
  /** cycle 6 v4：FTS 累计进度 [scanned, indexed]（跨全轮所有 phase 累计，不因 phase
   *  切换清零）；非索引中为 null。 */
  fts_progress: [number, number] | null;
  /** cycle 7-a：当前索引阶段（UI phase chip 用）；非索引中为 null。 */
  current_phase: IndexPhase | null;
  semantic_indexing: boolean;
  semantic_progress: [number, number] | null;
  semantic_summary: string | null;
  /** cycle 9：全库索引总数 [音频, 文档, 图片]（与「本地索引」行 last_summary 数字同源）。
   *  概貌是"当前生效目录内"口径、此为"全库"口径——两者可合法不一致（仅移除目录保留
   *  的记录 / 旧配置的记录仍在库），差值时概貌卡显式提示来源。 */
  db_totals: [number, number, number] | null;
  /** 2026-07-28：当前 phase 总文件数（walk/发现完成后才知道）；未知为 null。 */
  phase_total: number | null;
  /** 2026-07-28：当前 phase 内已扫描数（phase 切换即归零，与 fts_progress 的跨 phase
   *  累计不同）；配合 phase_total 算真百分比。 */
  phase_scanned: number | null;
  /** 2026-07-28：当前 phase 处理速率（个/分钟，累计平均）；未知为 null。 */
  phase_rate_per_min: number | null;
  /** 2026-07-28：上一次完整 reindex 的分阶段耗时明细；索引空闲时也保留。 */
  last_run_stage_ms: RunStageTimings;
}

/**
 * 2026-07-28：真百分比 + ETA 文案，配 `phase_total`/`phase_scanned`/`phase_rate_per_min`
 * 使用。任一数据缺失时返回 `null`（不编造百分比/ETA——`phase_total` 为 null 是已知的
 * fallback 场景，如发现层不可用时 walk 还没扫完；调用方此时应退回裸数字展示）。
 */
export function phaseProgressText(
  total: number | null,
  scanned: number | null,
  ratePerMin: number | null,
): { pct: number; label: string } | null {
  if (total === null || scanned === null || total <= 0) return null;
  const pct = Math.min(100, (scanned / total) * 100);
  const remaining = Math.max(0, total - scanned);
  if (ratePerMin === null || ratePerMin <= 0 || remaining === 0) {
    return { pct, label: `${scanned.toLocaleString()} / ${total.toLocaleString()}` };
  }
  const etaMin = remaining / ratePerMin;
  const etaLabel =
    etaMin < 1
      ? "不到 1 分钟"
      : etaMin < 60
        ? `约 ${Math.round(etaMin)} 分钟`
        : `约 ${(etaMin / 60).toFixed(1)} 小时`;
  return {
    pct,
    label: `${scanned.toLocaleString()} / ${total.toLocaleString()} · 约 ${Math.round(ratePerMin)} 个/分钟 · 预计还需 ${etaLabel}`,
  };
}

/** cycle 7-a：把 IndexPhase 映射到中文文案 + emoji chip。 */
export function phaseChipLabel(phase: IndexPhase): string {
  switch (phase) {
    case "music_discovery":
      return "🎵 扫描音频（内置原生索引快速发现，请稍候）";
    case "music_scan":
      return "🎵 扫描音频目录";
    case "doc":
      return "📄 扫描文档";
    case "image":
      return "🖼 扫描图片";
  }
}

/** `reindex` / `reindex_root` 命令的返回统计（cycle 7-c 单目录重扫与全量共用）。 */
export interface ReindexStats {
  music_added: number;
  music_updated: number;
  doc_added: number;
  doc_updated: number;
  image_added: number;
  image_updated: number;
}

export function reindexDoneMsg(s: ReindexStats): string {
  return `完成：音频 新增 ${s.music_added} / 更新 ${s.music_updated}，文档 新增 ${s.doc_added} / 更新 ${s.doc_updated}，图片 新增 ${s.image_added} / 更新 ${s.image_updated}`;
}

/** BETA-33 cycle 5：每个索引 root 的分类统计。后端 `get_index_overview` 返回。 */
export interface RootIndexOverview {
  path: string;
  is_default: boolean;
  doc_count: number;
  image_count: number;
  music_count: number;
  last_indexed_time: string | null;
}

/** BETA-40：一条「未能索引的文件」留痕。后端 `get_extraction_failures` 返回（按时间倒序）。 */
export interface ExtractionFailure {
  path: string;
  reason: string;
  failed_time: string | null;
}

/**
 * BETA-33 cycle 5：把 UTC rfc3339 时间转成本地口语（"5 分钟前" / "今天 15:32" / "2026-06-30"）。
 * 输入无效 → 空串。
 */
export function formatIndexTime(iso: string): string {
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return "";
  const now = new Date();
  const diffMs = now.getTime() - d.getTime();
  const diffMin = Math.round(diffMs / 60_000);
  if (diffMin < 1) return "刚刚";
  if (diffMin < 60) return `${diffMin} 分钟前`;
  const diffH = Math.round(diffMin / 60);
  if (diffH < 24 && d.getDate() === now.getDate()) {
    return `今天 ${String(d.getHours()).padStart(2, "0")}:${String(d.getMinutes()).padStart(2, "0")}`;
  }
  if (diffH < 48) return "昨天";
  const diffD = Math.round(diffH / 24);
  if (diffD < 7) return `${diffD} 天前`;
  return `${d.getFullYear()}-${String(d.getMonth() + 1).padStart(2, "0")}-${String(d.getDate()).padStart(2, "0")}`;
}
