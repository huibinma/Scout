import { useState, useCallback, useRef, useMemo, useEffect } from "react";
import { invoke, Channel } from "@tauri-apps/api/core";
import {
  ArrowBendDownRight,
  ClockCounterClockwise,
  Clock,
  File,
  FileArchive,
  FileAudio,
  FileDoc,
  FileImage,
  FilePdf,
  FilePpt,
  FileText,
  FileVideo,
  FileXls,
  GearSix,
  MagnifyingGlass,
  PushPin,
  Sparkle,
  SlidersHorizontal,
  Star,
  Translate,
} from "@phosphor-icons/react";
import { onMenuAction } from "./lib/menu-events";

// 与 src-tauri/src/search.rs::SearchResultJson 对应
interface SearchResultJson {
  id: string;
  path: string;
  name: string;
  source: string;
  // BETA-04 多源融合：命中此结果的全部来源（单源时与 source 一致）。
  sources: string[];
  match_type: string;
  score: number | null;
  /// BETA-33 cycle 3 v3（v0.9.3）：语义原始 cosine（0-1）。仅语义命中有值。
  /// 与 `score` 区分：`score` 是 RRF 融合分（排序用、~0.16 拥挤），
  /// `semantic_cosine` 是真相似度（0.30-0.90、可评估 floor + 按相似度排序）。
  semantic_cosine?: number | null;
  modified_time: string | null;
  size_bytes: number | null;
}

// BETA-15B-5：与 src-tauri 的 explain_semantic_hit 返回对应；start/end 为正文字符偏移。
interface ExplainPayload {
  passages: { start: number; end: number; score: number }[];
}

// BETA-29：Search Intent 完整 JSON（schema wire 格式）。只类型化草稿 UI 会读写的
// 关键字段；其余字段经 index signature 原样保留、重跑时不丢。
interface IntentJson {
  intent: string;
  keywords?: string[] | null;
  extensions?: string[] | null;
  // wire 格式：单值回写标量、多值数组（BETA-18 scalar-or-vec）。
  file_type?: string | string[] | null;
  modified_time?: { type: string; value?: string; from?: string; to?: string } | null;
  sort?: string | null;
  [key: string]: unknown;
}

// 与 src-tauri/src/search.rs::SearchEvent 对应（Tauri Channel 流式协议）
type SearchEvent =
  | {
      event: "started";
      intent_summary: string;
      fallback_used: boolean;
      signals: string[];
      tool_id: string;
      // BETA-29：本轮生效 intent 的完整 JSON（后端序列化失败时为 null，草稿入口隐藏）。
      intent_json: IntentJson | null;
    }
  | { event: "result"; item: SearchResultJson }
  | { event: "complete"; total: number; elapsed_ms: number }
  | { event: "action_done"; action_kind: string; paths: string[] }
  | {
      event: "confirm_action";
      action_kind: string;
      paths: string[];
      destination: string | null;
      new_name: string | null;
    }
  | { event: "error"; message: string }
  | { event: "backend_switched"; from: string; to: string; reason: string }
  | { event: "model_thinking" };

interface IntentSummary {
  intent_summary: string;
  fallback_used: boolean;
  signals: string[];
  tool_id: string;
  // BETA-29：草稿 UI 的数据源（started 事件带回）。
  intent_json: IntentJson | null;
}

// BETA-20 结果预览（与 src-tauri/src/search/preview.rs::PreviewPayload 对应）。
// 命中片段 snippet 中  /  为高亮哨兵（命中起止），前端转 <mark>。
type PreviewPayload =
  | {
      kind: "music";
      artist: string | null;
      title: string | null;
      album: string | null;
      duration_secs: number | null;
      format: string | null;
      bitrate: number | null;
    }
  | {
      kind: "document";
      doc_type: string;
      title: string | null;
      author: string | null;
      page_count: number | null;
      body: string;
      body_truncated: boolean;
      snippet: string | null;
      // BETA-35 cycle 5：扫描版 PDF 附带段级 OCR / 失败页；非扫描 PDF 均为空数组
      // （Rust 侧 #[serde(default)]，老索引 / 老 payload 也安全）。
      scanned_pages: ScannedPageInfo[];
      failed_pages: FailedPageInfo[];
    }
  | { kind: "unindexed" };

// BETA-35 cycle 5：扫描版 PDF 段级 OCR 结果（前端命中回页展示"第 N 页 · OCR"）。
interface ScannedPageInfo {
  page_no: number;
  seq: number;
  text: string;
  text_truncated: boolean;
}
interface FailedPageInfo {
  page_no: number;
  reason: string;
}

// BETA-22 搜索历史 + 保存的搜索（与 src-tauri/src/history.rs 对应）。
interface HistoryEntry {
  query: string;
  last_run: string;
  run_count: number;
}
interface SavedSearch {
  id: string;
  name: string;
  query: string;
  created: string;
  // BETA-29 v2：可选意图草稿——存在时重跑走 search_with_intent（保留草稿修正）。
  intent?: IntentJson | null;
}
interface SearchHistoryStore {
  recent: HistoryEntry[];
  saved: SavedSearch[];
}

const PREVIEW_PREF_KEY = "scout.preview.v1";
// BETA-11D：零命中同义词教学开关（localStorage 持久化，默认开）。
const SUGGEST_SYNONYM_PREF_KEY = "suggestSynonymOnEmpty";
const HL_START = "";
const HL_END = "";
// 图片扩展名（doc_type 落在此集合时按「图片 OCR 文本」呈现），与 indexer IMAGE_EXTS 口径一致。
const IMAGE_DOC_TYPES = new Set([
  "png",
  "jpg",
  "jpeg",
  "bmp",
  "gif",
  "tif",
  "tiff",
  "webp",
]);

type Status =
  | { kind: "idle" }
  | {
      kind: "streaming";
      intent: IntentSummary | null;
      results: SearchResultJson[];
    }
  | {
      kind: "ready";
      intent: IntentSummary | null;
      results: SearchResultJson[];
      total: number;
      elapsed_ms: number;
    }
  | { kind: "action_done"; action_kind: string; paths: string[] }
  | {
      kind: "confirm_pending";
      action_kind: string;
      paths: string[];
      destination: string | null;
      new_name: string | null;
    }
  | { kind: "error"; message: string };

// 列定义。null sort.key = 保持 backend 原始顺序（按相关性 / 流式到达序）。
type ColKey = "name" | "path" | "size" | "ext" | "mtime" | "source" | "match" | "similarity";
type SortDir = "asc" | "desc";
interface SortState {
  key: ColKey | null;
  dir: SortDir;
}

interface ColumnDef {
  key: ColKey;
  label: string;
  defaultWidth: number;
  /** 名称列必选，不可隐藏（对齐 Everything） */
  alwaysOn?: boolean;
  /** 默认是否显示 */
  defaultVisible: boolean;
  /** 单元格 class（对齐等样式） */
  cellClass: string;
  /** 单元格内容渲染 */
  render: (r: SearchResultJson) => React.ReactNode;
  /** 排序键值：number 走数值比较，string 走中文 locale 比较 */
  sortValue: (r: SearchResultJson) => string | number;
}

// match_type 原始字符串 → 中文展示标签（与后端 MatchType 序列化口径一致，小写）。
function matchTypeLabel(mt: string): string {
  switch (mt) {
    case "filename":
      return "文件名";
    case "content":
      return "内容";
    case "metadata":
      return "元数据";
    case "ocr":
      return "OCR";
    case "semantic":
      return "按意思找到";
    default:
      return mt;
  }
}

/// 语义召回来源标注：纯语义命中 / 关键词+语义双中 / 非语义（null）。
function semanticSourceLabel(r: SearchResultJson): string | null {
  const srcs = r.sources ?? [];
  const hasSem = srcs.includes("semanticindex") || r.match_type === "semantic";
  if (!hasSem) return null;
  const hasKeyword = srcs.some((s) => s !== "semanticindex");
  return hasKeyword ? "关键词+语义双中" : "纯语义命中";
}

/// 真 cosine → 置信档位。
function confidenceBand(score: number): string {
  if (score >= 0.5) return "强相关";
  if (score >= 0.3) return "中相关";
  return "弱相关";
}

// 所有可选列（顺序即默认显示顺序）。均可由现有结果数据直接算出，无需后端改动。
const ALL_COLUMNS: ColumnDef[] = [
  {
    key: "name",
    label: "名称",
    defaultWidth: 280,
    alwaysOn: true,
    defaultVisible: true,
    cellClass: "col-name",
    render: (r) => (
      <>
        <span className="file-icon" aria-hidden>
          {fileGlyph(r.name)}
        </span>
        <span className="file-name">{r.name}</span>
      </>
    ),
    sortValue: (r) => r.name.toLowerCase(),
  },
  {
    key: "path",
    label: "路径",
    defaultWidth: 420,
    defaultVisible: true,
    cellClass: "col-path",
    render: (r) => <span title={r.path}>{dirOf(r.path)}</span>,
    sortValue: (r) => r.path.toLowerCase(),
  },
  {
    key: "size",
    label: "大小",
    defaultWidth: 90,
    defaultVisible: true,
    cellClass: "col-size",
    render: (r) => (r.size_bytes !== null ? formatSize(r.size_bytes) : ""),
    sortValue: (r) => r.size_bytes ?? -1,
  },
  {
    key: "ext",
    label: "扩展名",
    defaultWidth: 80,
    defaultVisible: false,
    cellClass: "col-ext",
    render: (r) => extOf(r.name),
    sortValue: (r) => extOf(r.name),
  },
  {
    key: "mtime",
    label: "修改时间",
    defaultWidth: 150,
    defaultVisible: true,
    cellClass: "col-mtime",
    render: (r) => (r.modified_time ? formatDate(r.modified_time) : ""),
    sortValue: (r) => (r.modified_time ? Date.parse(r.modified_time) : 0),
  },
  {
    key: "source",
    label: "来源",
    defaultWidth: 120,
    defaultVisible: false,
    cellClass: "col-source",
    // 多源命中（BETA-04 fan-out 合并）显示「a + b」，单源显示 source。
    render: (r) => (r.sources && r.sources.length > 1 ? r.sources.join(" + ") : r.source),
    sortValue: (r) => r.source,
  },
  {
    key: "match",
    label: "匹配方式",
    defaultWidth: 110,
    // BETA-15B-1：语义召回是旗舰信号，匹配方式列默认可见以展示「按意思找到」徽标。
    // cycle 3 v3：badge 上不再显示分数（避免混淆 RRF vs cosine），改由独立「相似度」列承载。
    defaultVisible: true,
    cellClass: "col-match",
    render: (r) => {
      const semLabel = semanticSourceLabel(r);
      return semLabel ? (
        <span className="badge-semantic" title={`${semLabel}（按语义/跨语言召回）`}>
          {semLabel}
        </span>
      ) : (
        matchTypeLabel(r.match_type)
      );
    },
    sortValue: (r) => r.match_type,
  },
  {
    key: "similarity",
    label: "相似度",
    defaultWidth: 80,
    // BETA-33 cycle 3 v3（v0.9.3）：语义原始 cosine（0-1）独立列，可点头排序。
    // 只对语义命中有数值；非语义命中显示空。
    defaultVisible: true,
    cellClass: "col-similarity",
    render: (r) =>
      typeof r.semantic_cosine === "number" ? (
        <span
          title={`语义相似度 ${r.semantic_cosine.toFixed(3)}（${confidenceBand(r.semantic_cosine)}；低于设置的相似度下限已被过滤）`}
        >
          {r.semantic_cosine.toFixed(2)}
        </span>
      ) : (
        ""
      ),
    // 语义命中按 cosine 排（大 → 小）；非语义命中 sort 到最后（用 -1 占位）。
    sortValue: (r) =>
      typeof r.semantic_cosine === "number" ? r.semantic_cosine : -1,
  },
];

const COLS_BY_KEY: Record<ColKey, ColumnDef> = Object.fromEntries(
  ALL_COLUMNS.map((c) => [c.key, c]),
) as Record<ColKey, ColumnDef>;

// # 列固定宽度（px）。计入表格总宽，保证表宽 = 各列宽之和。
const INDEX_WIDTH = 48;

const COLS_STORAGE_KEY = "scout.columns.v1";
// BETA-15B-3：列偏好 schema 版本。v2 = 引入「匹配方式」语义列；驱动一次性列迁移。
// BETA-33 cycle 3 v3（v0.9.3）：v3 = 引入「相似度」列（语义原始 cosine）。
const COLUMN_PREFS_VERSION = 3;

interface ColumnPrefs {
  /** 可见列 key（保持 ALL_COLUMNS 顺序渲染） */
  visible: ColKey[];
  /** 每列宽度覆盖（缺省用 defaultWidth） */
  widths: Partial<Record<ColKey, number>>;
  /** BETA-15B-3：prefs schema 版本，缺省（旧数据）视为 1。 */
  version: number;
}

function defaultColumnPrefs(): ColumnPrefs {
  return {
    visible: ALL_COLUMNS.filter((c) => c.defaultVisible).map((c) => c.key),
    widths: {},
    version: COLUMN_PREFS_VERSION,
  };
}

/**
 * 纯函数：把解析出的（可能旧版）prefs 迁移到当前 schema。
 * v2 前的 prefs 早于「匹配方式」列 → 注入一次 match（老用户从未见过该列、不可能主动隐藏过），
 * 标 version=2；此后尊重用户选择（手动隐藏 match 不再被强加）。
 * 返回 { prefs, migrated }；migrated=true 时调用方应回写持久化，使迁移只发生一次。
 */
function migrateColumnPrefs(parsed: Partial<ColumnPrefs>): {
  prefs: ColumnPrefs;
  migrated: boolean;
} {
  const validKeys = new Set<ColKey>(ALL_COLUMNS.map((c) => c.key));
  let visible = Array.isArray(parsed.visible)
    ? parsed.visible.filter((k): k is ColKey => validKeys.has(k as ColKey))
    : defaultColumnPrefs().visible;
  if (!visible.includes("name")) visible = ["name", ...visible]; // 名称列始终可见
  const version = typeof parsed.version === "number" ? parsed.version : 1;
  let injected = false;
  if (version < 2 && !visible.includes("match")) {
    // 一次性补显旗舰语义列（渲染按 ALL_COLUMNS 顺序，visible 内位置无关）。
    visible = [...visible, "match"];
    injected = true;
  }
  if (version < 3 && !visible.includes("similarity")) {
    // v3 一次性补显「相似度」列（语义原始 cosine，可评估 floor 与按分数排序）。
    visible = [...visible, "similarity"];
    injected = true;
  }
  const prefs: ColumnPrefs = {
    visible,
    widths: parsed.widths ?? {},
    version: COLUMN_PREFS_VERSION,
  };
  // injected 或仅版本落后（含已有 match 的 v1）都回写一次，把 version 升到当前。
  return { prefs, migrated: injected || version < COLUMN_PREFS_VERSION };
}

function loadColumnPrefs(): ColumnPrefs {
  try {
    const raw = localStorage.getItem(COLS_STORAGE_KEY);
    if (!raw) return defaultColumnPrefs();
    const parsed = JSON.parse(raw) as Partial<ColumnPrefs>;
    const { prefs, migrated } = migrateColumnPrefs(parsed);
    if (migrated) saveColumnPrefs(prefs); // 迁移结果回写，迁移只发生一次
    return prefs;
  } catch {
    return defaultColumnPrefs();
  }
}

function saveColumnPrefs(prefs: ColumnPrefs): void {
  try {
    localStorage.setItem(COLS_STORAGE_KEY, JSON.stringify(prefs));
  } catch {
    // localStorage 不可用时忽略，不影响功能
  }
}

// BETA-11D：零命中同义词教学流程状态。
// "prompt" = 显示「为 {head} 添加同义词?」输入框；
// "confirm" = adhoc 重查已返回，显示「记住?」确认。
type SynonymFlowState =
  | { stage: "prompt"; head: string; aliasesRaw: string }
  | { stage: "confirm"; head: string; aliases: string[] }
  | null;

export default function SearchView() {
  const [query, setQuery] = useState("");
  const [status, setStatus] = useState<Status>({ kind: "idle" });
  const [sort, setSort] = useState<SortState>({ key: null, dir: "asc" });
  const [selected, setSelected] = useState<string | null>(null);
  // BETA-33 cycle 2：菜单栏 → 输入框聚焦 / 全选 通道（Ctrl+F / Ctrl+N / Esc）。
  const inputRef = useRef<HTMLInputElement>(null);
  // 结果内即时筛选（客户端，按名称/路径子串过滤当前结果）
  const [filter, setFilter] = useState("");
  // 右键上下文菜单
  const [menu, setMenu] = useState<{
    x: number;
    y: number;
    result: SearchResultJson;
  } | null>(null);
  // 文件操作（打开/定位）的瞬时反馈，显示在状态栏
  const [actionMsg, setActionMsg] = useState<string | null>(null);
  // 后端回退提示（fallback chain：主后端无结果/失败时切到下一后端）。每轮搜索重置。
  const [switchNotes, setSwitchNotes] = useState<string[]>([]);
  // BETA-23：模型 fallback 推理中（约 1s），显示「正在理解查询」轻量提示。
  const [modelThinking, setModelThinking] = useState(false);
  // BETA-20 结果预览面板：本轮已执行的 query（用于命中高亮，避免随输入框实时变化）、
  // 当前预览数据、加载态、面板显隐（localStorage 持久化，默认开）。
  const [executedQuery, setExecutedQuery] = useState("");
  const [preview, setPreview] = useState<PreviewPayload | null>(null);
  const [previewLoading, setPreviewLoading] = useState(false);
  // BETA-15B-5：语义命中段落高亮区间（仅语义结果按需拉取）。
  const [explain, setExplain] = useState<ExplainPayload | null>(null);
  const [showPreview, setShowPreview] = useState<boolean>(() => {
    try {
      return localStorage.getItem(PREVIEW_PREF_KEY) !== "0";
    } catch {
      return true;
    }
  });
  const togglePreview = useCallback(() => {
    setShowPreview((v) => {
      const next = !v;
      try {
        localStorage.setItem(PREVIEW_PREF_KEY, next ? "1" : "0");
      } catch {
        // localStorage 不可用时忽略
      }
      return next;
    });
  }, []);
  // BETA-11D：零命中同义词教学开关（localStorage 持久化，默认开）。
  const [suggestSynonymOnEmpty, setSuggestSynonymOnEmpty] = useState<boolean>(() => {
    try {
      return localStorage.getItem(SUGGEST_SYNONYM_PREF_KEY) !== "0";
    } catch {
      return true;
    }
  });
  const toggleSuggestSynonym = useCallback(() => {
    setSuggestSynonymOnEmpty((v) => {
      const next = !v;
      try {
        localStorage.setItem(SUGGEST_SYNONYM_PREF_KEY, next ? "1" : "0");
      } catch {
        // localStorage 不可用时忽略
      }
      return next;
    });
  }, []);
  // BETA-11D：零命中教学流程状态（null = 不显示）。
  const [synonymFlow, setSynonymFlow] = useState<SynonymFlowState>(null);
  // BETA-15B-5：相似度下限，用于「弱相关已隐藏」说明。
  const [semanticFloor, setSemanticFloor] = useState(0.3);
  // 标记当前搜索是否是 adhoc 重查（用于抑制无限循环的零命中提示）。
  // 使用 ref 而非 state，避免闭包 stale（channel 回调里读取）。
  const isAdhocRef = useRef(false);
  // BETA-29：意图草稿面板显隐（默认折叠，不打断主搜索流）。跨轮保持——
  // 修正重跑后面板留在打开态，方便连续调整。
  const [draftOpen, setDraftOpen] = useState(false);
  // BETA-22：搜索历史（自动记录）+ 保存的搜索；历史下拉显隐；保存命名输入态。
  const [history, setHistory] = useState<HistoryEntry[]>([]);
  const [saved, setSaved] = useState<SavedSearch[]>([]);
  const [showHistory, setShowHistory] = useState(false);
  const [savingName, setSavingName] = useState<string | null>(null);
  // BETA-29 v2：保存草稿时暂存的意图 JSON（命名确认时随 save_search 一并提交；
  // 普通「☆ 保存此搜索」入口不带意图，保持 null）。
  const [savingIntent, setSavingIntent] = useState<IntentJson | null>(null);
  // BETA-29 v2：搜索前意图预览（只解析不执行；「按此条件搜索」或普通搜索发起时清除）。
  const [preSearchDraft, setPreSearchDraft] = useState<{
    query: string;
    summary: string;
    json: IntentJson;
  } | null>(null);
  // 用 ref 保证 channel 回调里读到的是最新累积结果，不被 React 闭包 stale
  const streamRef = useRef<{
    intent: IntentSummary | null;
    results: SearchResultJson[];
  }>({
    intent: null,
    results: [],
  });

  // BETA-22：拉取历史 + 保存的搜索（启动 / 变更后刷新）。失败静默（功能降级不阻断搜索）。
  const refreshHistory = useCallback(() => {
    invoke<SearchHistoryStore>("get_search_history")
      .then((s) => {
        setHistory(s.recent ?? []);
        setSaved(s.saved ?? []);
      })
      .catch(() => {});
  }, []);

  useEffect(() => {
    refreshHistory();
  }, [refreshHistory]);

  // BETA-15B-5：读相似度下限用于「弱相关已隐藏」说明（get_settings 已返回该字段）。
  // 依赖 showPreview：每次打开预览面板时重读，避免用户在设置页修改后回来显示过期阈值。
  // clamp[0,1] 防止后端返回越界值导致荒谬数字。
  useEffect(() => {
    invoke<{ semantic_similarity_floor: number | null }>("get_settings")
      .then((s) =>
        setSemanticFloor(Math.max(0, Math.min(1, s.semantic_similarity_floor ?? 0.3))),
      )
      .catch(() => {});
  }, [showPreview]);

  // 执行一次搜索（接受显式 query，供输入框 / 历史 / 保存的搜索复用，不依赖 state 闭包）。
  const runSearch = useCallback(
    async (q: string) => {
      if (q.trim().length === 0) {
        return;
      }
      setQuery(q); // 同步输入框（从历史/保存的搜索触发时）
      setShowHistory(false);
      setPreSearchDraft(null); // BETA-29 v2：普通搜索发起即离开预览态
      // BETA-11D：新的普通搜索，重置教学流程 + adhoc 标记。
      isAdhocRef.current = false;
      setSynonymFlow(null);
      // 重置 ref + state 进入 streaming 态
      streamRef.current = { intent: null, results: [] };
      setExecutedQuery(q); // 记录本轮 query 供预览高亮（与输入框后续编辑解耦）
      setSelected(null);
      setPreview(null);
      setFilter("");
      setMenu(null);
      setActionMsg(null);
      setSwitchNotes([]);
      setModelThinking(false);
      setStatus({ kind: "streaming", intent: null, results: [] });
      // BETA-22：记录到搜索历史（fire-and-forget，不阻塞搜索；成功后刷新下拉）。
      invoke("record_search", { query: q }).then(refreshHistory).catch(() => {});

      const onEvent = new Channel<SearchEvent>();
      onEvent.onmessage = (msg) => {
        switch (msg.event) {
          case "started": {
            setModelThinking(false);
            const intent: IntentSummary = {
              intent_summary: msg.intent_summary,
              fallback_used: msg.fallback_used,
              signals: msg.signals,
              tool_id: msg.tool_id,
              intent_json: msg.intent_json,
            };
            streamRef.current.intent = intent;
            setStatus({
              kind: "streaming",
              intent,
              results: streamRef.current.results,
            });
            break;
          }
          case "result": {
            streamRef.current.results = [...streamRef.current.results, msg.item];
            setStatus({
              kind: "streaming",
              intent: streamRef.current.intent,
              results: streamRef.current.results,
            });
            break;
          }
          case "complete": {
            setModelThinking(false);
            setStatus({
              kind: "ready",
              intent: streamRef.current.intent,
              results: streamRef.current.results,
              total: msg.total,
              elapsed_ms: msg.elapsed_ms,
            });
            // BETA-11D：零命中教学——仅普通搜索（非 adhoc）且开关开启时触发。
            if (
              msg.total === 0 &&
              !isAdhocRef.current &&
              suggestSynonymOnEmpty &&
              q.trim().length > 0
            ) {
              setSynonymFlow({ stage: "prompt", head: q.trim(), aliasesRaw: "" });
            }
            break;
          }
          case "action_done": {
            setStatus({
              kind: "action_done",
              action_kind: msg.action_kind,
              paths: msg.paths,
            });
            break;
          }
          case "confirm_action": {
            setStatus({
              kind: "confirm_pending",
              action_kind: msg.action_kind,
              paths: msg.paths,
              destination: msg.destination,
              new_name: msg.new_name,
            });
            break;
          }
          case "error": {
            setModelThinking(false);
            setStatus({ kind: "error", message: msg.message });
            break;
          }
          case "model_thinking": {
            setModelThinking(true);
            break;
          }
          case "backend_switched": {
            // fallback chain：主后端无结果/失败，已切到下一候选。展示一行轻量提示，
            // 让用户理解结果来自备用后端（如 Windows Search 未索引 → Everything 兜底）。
            console.debug(
              `backend switched ${msg.from} → ${msg.to} (${msg.reason})`,
            );
            setSwitchNotes((prev) => [
              ...prev,
              `${friendlyBackend(msg.from)}${friendlyReason(msg.reason)}，已改用 ${friendlyBackend(msg.to)}`,
            ]);
            break;
          }
        }
      };

      try {
        await invoke("search", { query: q, onEvent });
      } catch (err) {
        setModelThinking(false);
        setStatus({ kind: "error", message: String(err) });
      }
    },
    [refreshHistory, suggestSynonymOnEmpty],
  );

  // 输入框 / 搜索按钮触发：用当前输入框内容执行。
  const handleSearch = useCallback(() => runSearch(query), [runSearch, query]);

  // BETA-29 v2：搜索前预览——只解析不执行（parser 视角），草稿面板确认/修正后再搜。
  // 动作/澄清类不支持草稿编辑 → 提示并直接走普通搜索（不挡用户）。
  const previewBeforeSearch = useCallback(async () => {
    const q = query.trim();
    if (!q) return;
    setShowHistory(false);
    try {
      const p = await invoke<{
        supported: boolean;
        intent_summary: string;
        intent_json: IntentJson | null;
      }>("preview_intent", { query: q });
      if (!p.supported || !p.intent_json) {
        setActionMsg("该查询属于动作/澄清类，不支持草稿编辑，已直接执行搜索");
        void runSearch(q);
        return;
      }
      setPreSearchDraft({
        query: q,
        summary: p.intent_summary,
        json: p.intent_json,
      });
    } catch (err) {
      setActionMsg(`预览失败：${String(err)}`);
    }
  }, [query, runSearch]);

  // BETA-11D：adhoc 同义词重查（不落盘）。
  // head / aliases 由调用方（零命中提示 UI）传入；原始 query 来自 executedQuery。
  const runAdhocSearch = useCallback(
    async (head: string, aliases: string[]) => {
      const q = executedQuery.trim();
      if (!q || aliases.length === 0) return;
      // 注意：此处故意不调用 record_search。adhoc 重查是临时的扩展预览，
      // 同义词组尚未落盘，不应污染搜索历史（与 runSearch 的行为不同）。
      // 标记为 adhoc，避免零命中后再次弹出提示。
      isAdhocRef.current = true;
      setSynonymFlow(null);
      // 复用同一套 streaming 状态（结果替换空集）。
      streamRef.current = { intent: null, results: [] };
      setSelected(null);
      setPreview(null);
      setFilter("");
      setMenu(null);
      setActionMsg(null);
      setSwitchNotes([]);
      setModelThinking(false);
      setStatus({ kind: "streaming", intent: null, results: [] });

      const onEvent = new Channel<SearchEvent>();
      onEvent.onmessage = (msg) => {
        switch (msg.event) {
          case "started": {
            setModelThinking(false);
            const intent: IntentSummary = {
              intent_summary: msg.intent_summary,
              fallback_used: msg.fallback_used,
              signals: msg.signals,
              tool_id: msg.tool_id,
              intent_json: msg.intent_json,
            };
            streamRef.current.intent = intent;
            setStatus({
              kind: "streaming",
              intent,
              results: streamRef.current.results,
            });
            break;
          }
          case "result": {
            streamRef.current.results = [...streamRef.current.results, msg.item];
            setStatus({
              kind: "streaming",
              intent: streamRef.current.intent,
              results: streamRef.current.results,
            });
            break;
          }
          case "complete": {
            setModelThinking(false);
            setStatus({
              kind: "ready",
              intent: streamRef.current.intent,
              results: streamRef.current.results,
              total: msg.total,
              elapsed_ms: msg.elapsed_ms,
            });
            // adhoc 重查结束后（无论有无结果）弹出「记住?」确认。
            setSynonymFlow({ stage: "confirm", head, aliases });
            break;
          }
          case "action_done": {
            setStatus({
              kind: "action_done",
              action_kind: msg.action_kind,
              paths: msg.paths,
            });
            break;
          }
          case "confirm_action": {
            setStatus({
              kind: "confirm_pending",
              action_kind: msg.action_kind,
              paths: msg.paths,
              destination: msg.destination,
              new_name: msg.new_name,
            });
            break;
          }
          case "error": {
            setModelThinking(false);
            setStatus({ kind: "error", message: msg.message });
            break;
          }
          case "model_thinking": {
            setModelThinking(true);
            break;
          }
          case "backend_switched": {
            console.debug(
              `backend switched ${msg.from} → ${msg.to} (${msg.reason})`,
            );
            setSwitchNotes((prev) => [
              ...prev,
              `${friendlyBackend(msg.from)}${friendlyReason(msg.reason)}，已改用 ${friendlyBackend(msg.to)}`,
            ]);
            break;
          }
        }
      };

      try {
        await invoke("search_with_adhoc_synonyms", {
          query: q,
          head,
          aliases,
          onEvent,
        });
      } catch (err) {
        setModelThinking(false);
        setStatus({ kind: "error", message: String(err) });
      }
    },
    [executedQuery],
  );

  // BETA-29：意图草稿重跑——用户在草稿面板修正类型/时间/排序/关键词后，
  // 把整份 intent JSON 原样送回后端跳过 parser 直接执行。
  // 与 adhoc 重查同款约定：不 record_search（query 文本没变，避免历史重复计数）、
  // 标记 isAdhocRef 抑制零命中同义词教学（修正流程中弹教学是打断）。
  const runDraftSearch = useCallback(
    async (intentDraft: IntentJson, queryOverride?: string) => {
      // BETA-29 v2：queryOverride 供「搜索前预览」与「带草稿的保存的搜索」传入——
      // 这两个入口没有（或不该沿用）executedQuery，需同步输入框与本轮 query 记录。
      const q = (queryOverride ?? executedQuery).trim();
      if (queryOverride !== undefined) {
        setQuery(queryOverride);
        setExecutedQuery(queryOverride);
      }
      setPreSearchDraft(null);
      isAdhocRef.current = true;
      setSynonymFlow(null);
      streamRef.current = { intent: null, results: [] };
      setSelected(null);
      setPreview(null);
      setFilter("");
      setMenu(null);
      setActionMsg(null);
      setSwitchNotes([]);
      setModelThinking(false);
      setStatus({ kind: "streaming", intent: null, results: [] });

      const onEvent = new Channel<SearchEvent>();
      onEvent.onmessage = (msg) => {
        switch (msg.event) {
          case "started": {
            setModelThinking(false);
            const intent: IntentSummary = {
              intent_summary: msg.intent_summary,
              fallback_used: msg.fallback_used,
              signals: msg.signals,
              tool_id: msg.tool_id,
              intent_json: msg.intent_json,
            };
            streamRef.current.intent = intent;
            setStatus({
              kind: "streaming",
              intent,
              results: streamRef.current.results,
            });
            break;
          }
          case "result": {
            streamRef.current.results = [...streamRef.current.results, msg.item];
            setStatus({
              kind: "streaming",
              intent: streamRef.current.intent,
              results: streamRef.current.results,
            });
            break;
          }
          case "complete": {
            setModelThinking(false);
            setStatus({
              kind: "ready",
              intent: streamRef.current.intent,
              results: streamRef.current.results,
              total: msg.total,
              elapsed_ms: msg.elapsed_ms,
            });
            break;
          }
          case "action_done": {
            setStatus({
              kind: "action_done",
              action_kind: msg.action_kind,
              paths: msg.paths,
            });
            break;
          }
          case "confirm_action": {
            setStatus({
              kind: "confirm_pending",
              action_kind: msg.action_kind,
              paths: msg.paths,
              destination: msg.destination,
              new_name: msg.new_name,
            });
            break;
          }
          case "error": {
            setModelThinking(false);
            setStatus({ kind: "error", message: msg.message });
            break;
          }
          case "model_thinking": {
            setModelThinking(true);
            break;
          }
          case "backend_switched": {
            console.debug(
              `backend switched ${msg.from} → ${msg.to} (${msg.reason})`,
            );
            setSwitchNotes((prev) => [
              ...prev,
              `${friendlyBackend(msg.from)}${friendlyReason(msg.reason)}，已改用 ${friendlyBackend(msg.to)}`,
            ]);
            break;
          }
        }
      };

      try {
        await invoke("search_with_intent", {
          intent: intentDraft,
          query: q,
          onEvent,
        });
      } catch (err) {
        setModelThinking(false);
        setStatus({ kind: "error", message: String(err) });
      }
    },
    [executedQuery],
  );

  // BETA-11D：「记住」此映射 → 持久化到用户词典。
  const handleRemember = useCallback(async (head: string, aliases: string[]) => {
    setSynonymFlow(null);
    try {
      await invoke("add_user_synonym", { head, aliases });
    } catch (err) {
      setActionMsg(`保存同义词失败：${String(err)}`);
    }
  }, []);

  // BETA-11D：「不记住」→ 仅关闭确认条。
  const handleForget = useCallback(() => {
    setSynonymFlow(null);
  }, []);

  // BETA-22：保存当前搜索（命名后置顶，可一键重跑）。用内联输入而非 window.prompt
  // （Tauri WKWebView 不支持 prompt）。
  const beginSave = useCallback(() => {
    const q = (executedQuery || query).trim();
    if (!q) return;
    setSavingIntent(null); // 普通保存不带意图草稿
    setSavingName(q);
  }, [executedQuery, query]);

  // BETA-29 v2：把草稿面板当前修正保存为「保存的搜索」（复用命名输入，带意图提交）。
  const beginSaveDraft = useCallback(
    (intent: IntentJson) => {
      const q = (executedQuery || query).trim();
      if (!q) return;
      setSavingIntent(intent);
      setSavingName(q);
    },
    [executedQuery, query],
  );

  const commitSave = useCallback(async () => {
    const name = (savingName ?? "").trim();
    const q = (executedQuery || query).trim();
    if (!name || !q) {
      setSavingName(null);
      setSavingIntent(null);
      return;
    }
    try {
      await invoke("save_search", { name, query: q, intent: savingIntent });
      setSavingName(null);
      setSavingIntent(null);
      refreshHistory();
    } catch (err) {
      setActionMsg(`保存失败：${String(err)}`);
      setSavingName(null);
      setSavingIntent(null);
    }
  }, [savingName, savingIntent, executedQuery, query, refreshHistory]);

  const deleteSaved = useCallback(
    async (id: string) => {
      try {
        await invoke("delete_saved_search", { id });
        refreshHistory();
      } catch {
        // 删除失败无关紧要，下次刷新仍在
      }
    },
    [refreshHistory],
  );

  const clearHistory = useCallback(async () => {
    try {
      await invoke("clear_search_history");
      refreshHistory();
    } catch {
      // 忽略
    }
  }, [refreshHistory]);

  const handleConfirm = useCallback(async () => {
    try {
      const res = await invoke<{ action_kind: string; paths: string[] }>(
        "confirm_action",
      );
      setStatus({
        kind: "action_done",
        action_kind: res.action_kind,
        paths: res.paths,
      });
    } catch (err) {
      setStatus({ kind: "error", message: String(err) });
    }
  }, []);

  const handleCancel = useCallback(async () => {
    try {
      await invoke("cancel_action");
    } catch {
      // 取消失败无关紧要,直接回 idle
    }
    setStatus({ kind: "idle" });
  }, []);

  // 打开 / 在文件夹中显示：经后端 open_path / locate_path（复用 FileActionTool + Policy）
  const runPathAction = useCallback(
    async (cmd: "open_path" | "locate_path", path: string) => {
      setMenu(null);
      try {
        const res = await invoke<{ action_kind: string; paths: string[] }>(cmd, {
          path,
        });
        setActionMsg(describeAction(res.action_kind, res.paths));
      } catch (err) {
        setActionMsg(`操作失败：${String(err)}`);
      }
    },
    [],
  );

  const handleOpen = useCallback(
    (path: string) => runPathAction("open_path", path),
    [runPathAction],
  );
  const handleLocate = useCallback(
    (path: string) => runPathAction("locate_path", path),
    [runPathAction],
  );

  const toggleSort = useCallback((key: ColKey) => {
    setSort((prev) =>
      prev.key === key
        ? { key, dir: prev.dir === "asc" ? "desc" : "asc" }
        : { key, dir: "asc" },
    );
  }, []);

  // 当前要展示的结果集（streaming / ready 都有）
  const results =
    status.kind === "streaming" || status.kind === "ready"
      ? status.results
      : null;
  const intent =
    status.kind === "streaming" || status.kind === "ready"
      ? status.intent
      : null;

  const sortedResults = useMemo(() => {
    if (!results) return null;
    const f = filter.trim().toLowerCase();
    const filtered = f
      ? results.filter(
          (r) =>
            r.name.toLowerCase().includes(f) ||
            r.path.toLowerCase().includes(f),
        )
      : results;
    return sortResults(filtered, sort);
  }, [results, sort, filter]);

  const isStreaming = status.kind === "streaming";

  // BETA-20：当前选中的结果行（驱动预览面板）。
  const selectedResult = useMemo(
    () => results?.find((r) => r.id === selected) ?? null,
    [results, selected],
  );

  // 选中变化 / 面板开启时拉取预览（只读本地索引；竞态用 cancelled 守卫；失败回退 unindexed）。
  useEffect(() => {
    if (!showPreview || !selectedResult) {
      setPreview(null);
      return;
    }
    let cancelled = false;
    setPreviewLoading(true);
    invoke<PreviewPayload>("get_preview", {
      path: selectedResult.path,
      query: executedQuery || null,
    })
      .then((p) => {
        if (!cancelled) {
          setPreview(p);
          setPreviewLoading(false);
        }
      })
      .catch(() => {
        if (!cancelled) {
          setPreview({ kind: "unindexed" });
          setPreviewLoading(false);
        }
      });
    return () => {
      cancelled = true;
    };
  }, [showPreview, selectedResult, executedQuery]);

  // BETA-15B-5：选中语义命中结果时，按需算命中段落高亮区间。
  useEffect(() => {
    setExplain(null);
    if (!selectedResult || !executedQuery) return;
    const isSemantic =
      selectedResult.match_type === "semantic" ||
      (selectedResult.sources?.includes("semanticindex") ?? false);
    if (!isSemantic) return;
    let cancelled = false;
    invoke<ExplainPayload>("explain_semantic_hit", {
      path: selectedResult.path,
      query: executedQuery,
    })
      .then((e) => {
        if (!cancelled) setExplain(e);
      })
      .catch(() => {
        if (!cancelled) setExplain(null);
      });
    return () => {
      cancelled = true;
    };
  }, [selectedResult?.path, selectedResult?.match_type, executedQuery]);

  // BETA-33 cycle 2：菜单栏 / 全局快捷键 → SearchView 内部 handler 桥接。
  // 闭包闭合最新 selectedResult / history / handler 引用，依赖列表全列。
  useEffect(() => {
    return onMenuAction((action) => {
      switch (action) {
        case "new-search":
        case "reset-query":
          setQuery("");
          setSelected(null);
          inputRef.current?.focus();
          inputRef.current?.select();
          return;
        case "focus-search":
          inputRef.current?.focus();
          inputRef.current?.select();
          return;
        case "toggle-preview":
          togglePreview();
          return;
        case "show-history":
          if (history.length > 0) setShowHistory(true);
          return;
        case "clear-history":
          clearHistory();
          return;
        case "save-search":
          beginSave();
          return;
        case "open-selected":
          if (selectedResult) handleOpen(selectedResult.path);
          return;
        case "locate-selected":
          if (selectedResult) handleLocate(selectedResult.path);
          return;
        case "copy-path":
          if (selectedResult) {
            navigator.clipboard
              .writeText(selectedResult.path)
              .then(() => setActionMsg(`已复制路径：${selectedResult.path}`))
              .catch((err) =>
                setActionMsg(`复制失败：${String(err)}`),
              );
          }
          return;
      }
    });
  }, [
    selectedResult,
    history.length,
    togglePreview,
    clearHistory,
    beginSave,
    handleOpen,
    handleLocate,
  ]);

  return (
    <div className="search-view">
      <div className="page-heading">
        <div>
          <span className="page-eyebrow">Scout Search</span>
          <h1>找文件</h1>
          <p>说出你记得的关键字、时间段或文件线索，Scout 完成深度检索。</p>
        </div>
        <div className="local-only-chip">仅在本机处理</div>
      </div>

      {/* 主搜索区：从传统工具条升级为整页核心任务卡。 */}
      <div className="search-bar-wrap">
        <div className="search-box">
          <MagnifyingGlass className="search-icon" size={22} weight="duotone" />
          <input
            ref={inputRef}
            type="text"
            className="search-input"
            placeholder="用自然语言描述你要找的文件…"
            value={query}
            onChange={(e) => setQuery(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Enter") {
                // BETA-29 v2：Shift+Enter = 先预览意图草稿再搜索。
                if (e.shiftKey) {
                  e.preventDefault();
                  void previewBeforeSearch();
                } else {
                  handleSearch();
                }
              } else if (e.key === "Escape") {
                setShowHistory(false);
              }
            }}
            autoFocus
          />
          {/* BETA-22 历史下拉开关（有历史时才显示） */}
          {history.length > 0 && (
            <button
              type="button"
              className={`search-aux${showHistory ? " active" : ""}`}
              onClick={() => setShowHistory((v) => !v)}
              title="搜索历史"
              aria-label="搜索历史"
            >
              <ClockCounterClockwise size={18} />
            </button>
          )}
          {/* BETA-22 保存当前搜索（有可保存的 query 时显示） */}
          {(executedQuery || query).trim() && savingName === null && (
            <button
              type="button"
              className="search-aux"
              onClick={beginSave}
              title="保存此搜索"
              aria-label="保存此搜索"
            >
              <Star size={18} />
            </button>
          )}
          {/* BETA-29 v2：搜索前预览意图草稿（Shift+Enter 同款入口） */}
          {query.trim() && (
            <button
              type="button"
              className={`search-aux${preSearchDraft ? " active" : ""}`}
              onClick={() => void previewBeforeSearch()}
              title="先预览意图草稿再搜索（Shift+Enter）"
              aria-label="调整搜索条件"
            >
              <SlidersHorizontal size={18} />
            </button>
          )}
          <button
            type="button"
            className="search-button"
            onClick={handleSearch}
            disabled={isStreaming}
          >
            {isStreaming ? "搜索中…" : "搜索"}
          </button>
        </div>

        {/* BETA-22 历史下拉（浮于搜索框下方） */}
        {showHistory && history.length > 0 && (
          <>
            <div
              className="history-backdrop"
              onClick={() => setShowHistory(false)}
            />
            <ul className="history-dropdown">
              <li className="history-head">
                <span>搜索历史</span>
                <button
                  type="button"
                  className="history-clear"
                  onClick={clearHistory}
                  title="清空搜索历史"
                >
                  清空
                </button>
              </li>
              {history.map((h) => (
                <li
                  key={h.query}
                  className="history-item"
                  onClick={() => runSearch(h.query)}
                  title={`${h.run_count} 次 · ${formatDate(h.last_run)}`}
                >
                  <span className="history-query">{h.query}</span>
                  {h.run_count > 1 && (
                    <span className="history-count">{h.run_count}×</span>
                  )}
                </li>
              ))}
            </ul>
          </>
        )}
      </div>

      {/* BETA-22 保存的搜索条（chip 列表）+ 命名输入 */}
      {(saved.length > 0 || savingName !== null) && (
        <div className="saved-bar">
          {savingName !== null && (
            <span className="saved-form">
              <input
                type="text"
                className="saved-name-input"
                value={savingName}
                placeholder="为这个搜索取个名字…"
                autoFocus
                onChange={(e) => setSavingName(e.target.value)}
                onKeyDown={(e) => {
                  if (e.key === "Enter") commitSave();
                  else if (e.key === "Escape") setSavingName(null);
                }}
              />
              <button type="button" className="saved-save" onClick={commitSave}>
                保存
              </button>
              <button
                type="button"
                className="saved-cancel"
                onClick={() => setSavingName(null)}
              >
                取消
              </button>
            </span>
          )}
          {saved.map((s) => (
            <span
              key={s.id}
              className="saved-chip"
              title={s.intent ? `${s.query}（含意图草稿）` : s.query}
            >
              <button
                type="button"
                className="saved-chip-run"
                onClick={() =>
                  // BETA-29 v2：带意图草稿的条目重跑走 search_with_intent，保留草稿修正。
                  s.intent
                    ? void runDraftSearch(s.intent, s.query)
                    : runSearch(s.query)
                }
              >
                <PushPin size={14} weight="fill" /> {s.name}
                {s.intent && (
                  <span className="saved-chip-draft">
                    <GearSix size={13} />
                  </span>
                )}
              </button>
              <button
                type="button"
                className="saved-chip-del"
                onClick={() => deleteSaved(s.id)}
                title="删除此保存的搜索"
              >
                ×
              </button>
            </span>
          ))}
        </div>
      )}

      {status.kind === "idle" && !query.trim() && (
        <section className="search-start" aria-label="搜索示例">
          <div className="search-start-copy">
            <div className="search-start-icon">
              <Sparkle size={22} weight="duotone" />
            </div>
            <div>
              <h2>从你还记得的一点点开始</h2>
              <p>
                不需要准确文件名，主题、时间、类型、或中英文线索，都可以是搜索条件。
              </p>
            </div>
          </div>
          <div className="search-examples">
            {history.length > 0 ? (
              history.slice(0, 3).map((h) => (
                <button
                  key={h.query}
                  type="button"
                  onClick={() => runSearch(h.query)}
                >
                  <ClockCounterClockwise size={18} weight="duotone" />
                  <span>
                    <strong>{h.query}</strong>
                    <small>{formatDate(h.last_run)}</small>
                  </span>
                </button>
              ))
            ) : (
              <>
                <button
                  type="button"
                  onClick={() => runSearch("上个月修改的项目方案")}
                >
                  <Clock size={18} weight="duotone" />
                  <span>
                    <strong>上个月修改的项目方案</strong>
                    <small>按时间与内容一起寻找</small>
                  </span>
                </button>
                <button type="button" onClick={() => runSearch("年假和休假规定")}>
                  <Translate size={18} weight="duotone" />
                  <span>
                    <strong>年假和休假规定</strong>
                    <small>中文线索也能找英文文档</small>
                  </span>
                </button>
                <button
                  type="button"
                  onClick={() => runSearch("老板最近的讲话发文")}
                >
                  <FileText size={18} weight="duotone" />
                  <span>
                    <strong>老板最近的讲话发文</strong>
                    <small>同义词与语义条件组合</small>
                  </span>
                </button>
              </>
            )}
          </div>
        </section>
      )}

      {/* BETA-29 v2：搜索前意图预览条 + 草稿面板（只解析未执行；确认/修正后再搜） */}
      {preSearchDraft && (
        <>
          <div className="intent-bar">
            <span className="intent-label">预览</span>
            <code>{preSearchDraft.summary}</code>
            <span className="intent-tool">尚未执行搜索</span>
            <button
              type="button"
              className="intent-draft-toggle"
              onClick={() => setPreSearchDraft(null)}
              title="放弃预览"
            >
              收起 ▴
            </button>
          </div>
          <IntentDraftPanel
            base={preSearchDraft.json}
            busy={isStreaming}
            rerunLabel="按此条件搜索"
            onRerun={(i) => void runDraftSearch(i, preSearchDraft.query)}
            onSaveDraft={beginSaveDraft}
          />
        </>
      )}

      {/* 意图信息条（Scout 特有，薄薄一行） */}
      {intent && (
        <IntentBar
          intent={intent}
          count={results?.length ?? 0}
          streaming={isStreaming}
          elapsed_ms={status.kind === "ready" ? status.elapsed_ms : undefined}
          draftOpen={draftOpen}
          onToggleDraft={
            intent.intent_json ? () => setDraftOpen((v) => !v) : undefined
          }
        />
      )}

      {/* BETA-29：意图草稿面板（默认折叠；修正类型/时间/排序/关键词后重跑） */}
      {draftOpen && intent?.intent_json && (
        <IntentDraftPanel
          base={intent.intent_json}
          busy={isStreaming}
          onRerun={runDraftSearch}
          onSaveDraft={beginSaveDraft}
        />
      )}

      {/* BETA-23：模型 fallback 推理中提示（复用回退提示样式，不新增 CSS） */}
      {modelThinking && status.kind === "streaming" && (
        <div className="backend-switch-note">
          <span>正在理解查询…</span>
        </div>
      )}

      {/* 后端回退提示（fallback chain）：薄薄一行，说明结果来自备用后端 */}
      {switchNotes.length > 0 && (status.kind === "streaming" || status.kind === "ready") && (
        <div className="backend-switch-note">
          {switchNotes.map((note, i) => (
            <span key={i}>
              <ArrowBendDownRight size={14} /> {note}
            </span>
          ))}
        </div>
      )}

      {status.kind === "confirm_pending" && (
        <div className="search-status confirm-pending">
          <p>
            {describeConfirm(
              status.action_kind,
              status.paths,
              status.destination,
              status.new_name,
            )}
          </p>
          <div className="confirm-actions">
            <button type="button" className="confirm-yes" onClick={handleConfirm}>
              确认
            </button>
            <button type="button" className="confirm-no" onClick={handleCancel}>
              取消
            </button>
          </div>
        </div>
      )}

      {status.kind === "action_done" && (
        <div className="search-status action-done">
          {describeAction(status.action_kind, status.paths)}
        </div>
      )}

      {status.kind === "error" && (
        <div className="search-status error">
          <strong>错误：</strong> {status.message}
        </div>
      )}

      {/* 结果内筛选框（仅在有结果时显示） */}
      {results && results.length > 0 && (
        <div className="filter-bar">
          <input
            type="text"
            className="filter-input"
            placeholder="在结果中筛选（名称 / 路径）…"
            value={filter}
            onChange={(e) => setFilter(e.target.value)}
          />
          {filter && (
            <button
              type="button"
              className="filter-clear"
              onClick={() => setFilter("")}
              title="清除筛选"
            >
              ×
            </button>
          )}
          <button
            type="button"
            className={`preview-toggle${showPreview ? " active" : ""}`}
            onClick={togglePreview}
            title="切换结果预览面板"
          >
            {showPreview ? "隐藏预览" : "显示预览"}
          </button>
          {/* BETA-11D：零命中同义词提示开关 */}
          <button
            type="button"
            className={`preview-toggle${suggestSynonymOnEmpty ? " active" : ""}`}
            onClick={toggleSuggestSynonym}
            title="搜索无结果时提示添加同义词"
          >
            {suggestSynonymOnEmpty ? "同义词提示：开" : "同义词提示：关"}
          </button>
        </div>
      )}

      {/* 结果表格（仿 Everything 列表）+ BETA-20 预览面板（右侧竖分割） */}
      {sortedResults && (
        <div className="results-with-preview">
          <ResultTable
            results={sortedResults}
            sort={sort}
            onSort={toggleSort}
            selected={selected}
            onSelect={(id) => {
              setSelected(id);
              setActionMsg(null);
            }}
            onOpen={handleOpen}
            onContextMenu={(x, y, result) => setMenu({ x, y, result })}
          />
          {showPreview && selectedResult && (
            <PreviewPanel
              result={selectedResult}
              preview={preview}
              explain={explain}
              loading={previewLoading}
              onClose={togglePreview}
              semanticFloor={semanticFloor}
            />
          )}
        </div>
      )}

      {/* 空结果提示 + BETA-11D 零命中教学流程 */}
      {status.kind === "ready" && status.total === 0 && !synonymFlow && (
        <div className="search-status empty">没有命中结果。</div>
      )}

      {/* BETA-11D：零命中同义词提示条（prompt 阶段） */}
      {synonymFlow?.stage === "prompt" && (
        <SynonymPromptBar
          head={synonymFlow.head}
          aliasesRaw={synonymFlow.aliasesRaw}
          onHeadChange={(h) =>
            setSynonymFlow({ stage: "prompt", head: h, aliasesRaw: synonymFlow.aliasesRaw })
          }
          onAliasesChange={(a) =>
            setSynonymFlow({ stage: "prompt", head: synonymFlow.head, aliasesRaw: a })
          }
          onSearch={() => {
            const aliases = synonymFlow.aliasesRaw
              .split(/[,，\s]+/)
              .map((s) => s.trim())
              .filter((s) => s.length > 0);
            if (aliases.length === 0) return;
            runAdhocSearch(synonymFlow.head, aliases);
          }}
          onDismiss={() => setSynonymFlow(null)}
          onDisablePrompt={() => {
            // 用户点「不再提示」：关闭开关（含 localStorage 持久化）并收起当前提示条。
            if (suggestSynonymOnEmpty) toggleSuggestSynonym();
            setSynonymFlow(null);
          }}
          disabled={status.kind === "streaming"}
        />
      )}

      {/* BETA-11D：adhoc 重查后「记住?」确认条 */}
      {synonymFlow?.stage === "confirm" && (
        <SynonymConfirmBar
          head={synonymFlow.head}
          aliases={synonymFlow.aliases}
          onRemember={() => handleRemember(synonymFlow.head, synonymFlow.aliases)}
          onForget={handleForget}
        />
      )}

      {/* 右键上下文菜单 */}
      {menu && (
        <ContextMenu
          x={menu.x}
          y={menu.y}
          onClose={() => setMenu(null)}
          onOpen={() => handleOpen(menu.result.path)}
          onLocate={() => handleLocate(menu.result.path)}
        />
      )}

      {/* 底部状态栏（仿 Everything 底栏） */}
      <StatusBar
        status={status}
        count={sortedResults?.length ?? 0}
        total={results?.length ?? 0}
        selected={selected}
        actionMsg={actionMsg}
      />
    </div>
  );
}

// ---- 右键上下文菜单 ----

function ContextMenu({
  x,
  y,
  onClose,
  onOpen,
  onLocate,
}: {
  x: number;
  y: number;
  onClose: () => void;
  onOpen: () => void;
  onLocate: () => void;
}) {
  return (
    <>
      {/* 全屏遮罩：点击任意处关闭菜单 */}
      <div className="context-menu-backdrop" onClick={onClose} onContextMenu={(e) => { e.preventDefault(); onClose(); }} />
      <ul className="context-menu" style={{ left: x, top: y }}>
        <li onClick={onOpen}>打开</li>
        <li onClick={onLocate}>在文件夹中显示</li>
      </ul>
    </>
  );
}

// ---- BETA-11D 零命中同义词教学 UI ----

function SynonymPromptBar({
  head,
  aliasesRaw,
  onHeadChange,
  onAliasesChange,
  onSearch,
  onDismiss,
  onDisablePrompt,
  disabled,
}: {
  head: string;
  aliasesRaw: string;
  onHeadChange: (v: string) => void;
  onAliasesChange: (v: string) => void;
  onSearch: () => void;
  onDismiss: () => void;
  /** 点「不再提示」：永久关闭零命中提示开关并收起当前提示条。 */
  onDisablePrompt: () => void;
  disabled: boolean;
}) {
  return (
    <div className="synonym-prompt-bar">
      <span className="synonym-prompt-text">没找到结果。为</span>
      <input
        type="text"
        className="synonym-head-input"
        value={head}
        onChange={(e) => onHeadChange(e.target.value)}
        title="关键词（head）"
        aria-label="关键词"
      />
      <span className="synonym-prompt-text">添加同义词扩展搜索？</span>
      <input
        type="text"
        className="synonym-aliases-input"
        placeholder="同义词，用逗号分隔…"
        value={aliasesRaw}
        onChange={(e) => onAliasesChange(e.target.value)}
        onKeyDown={(e) => {
          if (e.key === "Enter") {
            // Fix 2: 与按钮保持一致，disabled 时不触发搜索。
            if (!disabled && aliasesRaw.trim().length > 0) onSearch();
          } else if (e.key === "Escape") {
            onDismiss();
          }
        }}
        aria-label="同义词（逗号分隔）"
        autoFocus
      />
      <button
        type="button"
        className="synonym-action-btn synonym-search-btn"
        onClick={onSearch}
        disabled={disabled || aliasesRaw.trim().length === 0}
      >
        扩展搜索
      </button>
      <button
        type="button"
        className="synonym-no-prompt-btn"
        onClick={onDisablePrompt}
        title="关闭零命中同义词提示（可在筛选栏重新开启）"
      >
        不再提示
      </button>
      <button
        type="button"
        className="synonym-dismiss-btn"
        onClick={onDismiss}
        title="关闭提示"
      >
        ×
      </button>
    </div>
  );
}

function SynonymConfirmBar({
  head,
  aliases,
  onRemember,
  onForget,
}: {
  head: string;
  aliases: string[];
  onRemember: () => void;
  onForget: () => void;
}) {
  return (
    <div className="synonym-confirm-bar">
      <span className="synonym-prompt-text">
        是否记住映射「{head} → {aliases.join("、")}」？
      </span>
      <button
        type="button"
        className="synonym-action-btn synonym-remember-btn"
        onClick={onRemember}
      >
        记住
      </button>
      <button
        type="button"
        className="synonym-action-btn synonym-forget-btn"
        onClick={onForget}
      >
        不记住
      </button>
    </div>
  );
}

// ---- BETA-20 结果预览面板 ----

/** 把含高亮哨兵（\x02 / \x03）的文本渲染为带 <mark> 的片段。 */
function renderHighlighted(text: string): React.ReactNode[] {
  const parts: React.ReactNode[] = [];
  let buf = "";
  let inMark = false;
  let key = 0;
  const flush = () => {
    if (!buf) return;
    parts.push(inMark ? <mark key={key++}>{buf}</mark> : buf);
    buf = "";
  };
  for (const ch of text) {
    if (ch === HL_START) {
      flush();
      inMark = true;
    } else if (ch === HL_END) {
      flush();
      inMark = false;
    } else {
      buf += ch;
    }
  }
  flush();
  return parts;
}

/** 把语义命中段落区间（字符偏移）叠加到正文上高亮。区间不重叠、已按 start 排序处理。 */
function renderWithSemanticRanges(
  body: string,
  passages: { start: number; end: number; score: number }[],
): React.ReactNode[] {
  const ranges = [...passages].sort((a, b) => a.start - b.start);
  const parts: React.ReactNode[] = [];
  let buf = "";
  let key = 0;
  let i = 0; // 当前 code point 序号
  let ri = 0; // 当前 range 指针
  let inMark = false;
  let curScore: number | undefined;
  const flush = () => {
    if (!buf) return;
    parts.push(
      inMark ? (
        <mark
          key={key++}
          className="semantic-highlight"
          title={
            curScore !== undefined ? `语义相似度 ${curScore.toFixed(2)}` : undefined
          }
        >
          {buf}
        </mark>
      ) : (
        <span key={key++}>{buf}</span>
      ),
    );
    buf = "";
  };
  for (const ch of body) {
    const inRange =
      ri < ranges.length && i >= ranges[ri].start && i < ranges[ri].end;
    if (inRange !== inMark) {
      flush();
      inMark = inRange;
      curScore = inRange ? ranges[ri].score : undefined;
    }
    buf += ch;
    i += 1;
    if (ri < ranges.length && i >= ranges[ri].end) {
      flush();
      inMark = false;
      curScore = undefined;
      ri += 1;
    }
  }
  flush();
  return parts;
}

/** 秒数 → `mm:ss` / `h:mm:ss`。 */
function formatDuration(secs: number): string {
  const total = Math.round(secs);
  const h = Math.floor(total / 3600);
  const m = Math.floor((total % 3600) / 60);
  const s = total % 60;
  const pad = (n: number) => String(n).padStart(2, "0");
  return h > 0 ? `${h}:${pad(m)}:${pad(s)}` : `${m}:${pad(s)}`;
}

function PreviewMetaRow({
  label,
  value,
}: {
  label: string;
  value: React.ReactNode;
}) {
  if (value === null || value === undefined || value === "") return null;
  return (
    <div className="preview-meta-row">
      <span className="preview-meta-label">{label}</span>
      <span className="preview-meta-value">{value}</span>
    </div>
  );
}

function PreviewPanel({
  result,
  preview,
  explain,
  loading,
  onClose,
  semanticFloor,
}: {
  result: SearchResultJson;
  preview: PreviewPayload | null;
  explain: ExplainPayload | null;
  loading: boolean;
  onClose: () => void;
  semanticFloor: number;
}) {
  return (
    <aside className="preview-panel">
      <div className="preview-header">
        <span className="preview-title" title={result.path}>
          {result.name}
        </span>
        <button
          type="button"
          className="preview-close"
          onClick={onClose}
          title="关闭预览面板"
        >
          ×
        </button>
      </div>
      <div className="preview-path" title={result.path}>
        {result.path}
      </div>

      <div className="preview-body">
        {loading && <div className="preview-hint">加载预览…</div>}

        {!loading && preview?.kind === "music" && (
          <div className="preview-music">
            <PreviewMetaRow label="标题" value={preview.title} />
            <PreviewMetaRow label="艺术家" value={preview.artist} />
            <PreviewMetaRow label="专辑" value={preview.album} />
            <PreviewMetaRow
              label="时长"
              value={
                preview.duration_secs !== null
                  ? formatDuration(preview.duration_secs)
                  : null
              }
            />
            <PreviewMetaRow label="格式" value={preview.format} />
            <PreviewMetaRow
              label="码率"
              value={preview.bitrate !== null ? `${preview.bitrate} kbps` : null}
            />
          </div>
        )}

        {!loading && preview?.kind === "document" && (
          <div className="preview-document">
            <PreviewMetaRow
              label="类型"
              value={
                IMAGE_DOC_TYPES.has(preview.doc_type.toLowerCase())
                  ? `图片 OCR（${preview.doc_type}）`
                  : preview.doc_type
              }
            />
            <PreviewMetaRow label="标题" value={preview.title} />
            <PreviewMetaRow label="作者" value={preview.author} />
            <PreviewMetaRow
              label="页/节数"
              value={preview.page_count !== null ? preview.page_count : null}
            />
            {preview.scanned_pages.length > 0 && (
              <PreviewMetaRow
                label="扫描版"
                value={
                  `共 ${preview.page_count ?? "?"} 页 · ` +
                  `${preview.scanned_pages.length} 段 OCR 成功` +
                  (preview.failed_pages.length > 0
                    ? ` · ${preview.failed_pages.length} 页失败`
                    : "")
                }
              />
            )}
            {preview.snippet && (
              <div className="preview-snippet">
                <div className="preview-section-label">命中片段</div>
                <p className="preview-snippet-text">
                  {renderHighlighted(preview.snippet)}
                </p>
              </div>
            )}
            {explain && explain.passages.length > 0 && (
              <div className="preview-semantic-note">
                <div className="preview-section-label">语义命中</div>
                <p>
                  最相似段落 · <strong>段落级</strong>相似度 {explain.passages[0].score.toFixed(2)}（
                  {confidenceBand(explain.passages[0].score)}）
                  <span style={{ color: "var(--subtle)", marginLeft: "8px" }}>
                    与结果表「相似度」列（文档级）不同粒度、可能相差 0.1–0.2 属正常
                  </span>
                </p>
              </div>
            )}
            <div className="preview-content">
              <div className="preview-section-label">
                {IMAGE_DOC_TYPES.has(preview.doc_type.toLowerCase())
                  ? "OCR 文本"
                  : "正文"}
              </div>
              <pre className="preview-content-text">
                {explain && explain.passages.length > 0
                  ? renderWithSemanticRanges(preview.body, explain.passages)
                  : preview.body || "（无可预览的文本内容）"}
                {preview.body_truncated && "\n…（已截断）"}
              </pre>
            </div>
            {preview.scanned_pages.length > 0 && (
              <div className="preview-scanned-pages">
                <div className="preview-section-label">
                  OCR 段落（扫描版 PDF · 第 N 页 · OCR）
                </div>
                {preview.scanned_pages.map((page) => (
                  <div
                    key={`${page.page_no}-${page.seq}`}
                    style={{ marginTop: "8px" }}
                  >
                    <div
                      style={{
                        fontSize: "12px",
                        color: "var(--muted)",
                        marginBottom: "2px",
                      }}
                    >
                      第 {page.page_no} 页 · OCR
                    </div>
                    <pre className="preview-content-text">
                      {page.text}
                      {page.text_truncated && "\n…（段已截断）"}
                    </pre>
                  </div>
                ))}
              </div>
            )}
            {preview.failed_pages.length > 0 && (
              <div className="preview-failed-pages" style={{ marginTop: "8px" }}>
                <div className="preview-section-label">
                  失败页（OCR 未识别 · 验收 ③）
                </div>
                <ul style={{ margin: "4px 0 0 20px", padding: 0 }}>
                  {preview.failed_pages.map((f) => (
                    <li
                      key={f.page_no}
                      style={{ color: "var(--status-err-fg)", fontSize: "12px" }}
                    >
                      第 {f.page_no} 页：{f.reason}
                    </li>
                  ))}
                </ul>
              </div>
            )}
          </div>
        )}

        {!loading && (!preview || preview.kind === "unindexed") && (
          <div className="preview-unindexed">
            <div className="preview-hint">该文件未被本地索引，仅显示基本信息。</div>
            <PreviewMetaRow label="来源" value={result.source} />
            <PreviewMetaRow label="匹配" value={result.match_type} />
            <PreviewMetaRow
              label="大小"
              value={
                result.size_bytes !== null ? formatSize(result.size_bytes) : null
              }
            />
            <PreviewMetaRow
              label="修改时间"
              value={
                result.modified_time ? formatDate(result.modified_time) : null
              }
            />
          </div>
        )}
      </div>
    </aside>
  );
}

// ---- 结果表格 ----

function ResultTable({
  results,
  sort,
  onSort,
  selected,
  onSelect,
  onOpen,
  onContextMenu,
}: {
  results: SearchResultJson[];
  sort: SortState;
  onSort: (key: ColKey) => void;
  selected: string | null;
  onSelect: (id: string | null) => void;
  onOpen: (path: string) => void;
  onContextMenu: (x: number, y: number, result: SearchResultJson) => void;
}) {
  // 列偏好（可见列 + 列宽）从 localStorage 初始化并持久化。
  const [prefs, setPrefs] = useState<ColumnPrefs>(() => loadColumnPrefs());
  // 列选择器菜单位置（右键列头打开）
  const [colMenu, setColMenu] = useState<{ x: number; y: number } | null>(null);
  // 拖拽中的列与起始坐标；用 ref 避免拖拽过程频繁重渲染闭包 stale
  const dragRef = useRef<{ key: ColKey; startX: number; startW: number } | null>(
    null,
  );

  const persist = useCallback((next: ColumnPrefs) => {
    setPrefs(next);
    saveColumnPrefs(next);
  }, []);

  // 当前可见列（按 ALL_COLUMNS 顺序）
  const visibleCols = ALL_COLUMNS.filter((c) => prefs.visible.includes(c.key));
  const widthOf = (key: ColKey) => prefs.widths[key] ?? COLS_BY_KEY[key].defaultWidth;

  const onResizeStart = useCallback(
    (key: ColKey, e: React.MouseEvent) => {
      e.preventDefault();
      e.stopPropagation();
      const startW = prefs.widths[key] ?? COLS_BY_KEY[key].defaultWidth;
      dragRef.current = { key, startX: e.clientX, startW };

      const onMove = (ev: MouseEvent) => {
        const d = dragRef.current;
        if (!d) return;
        const next = Math.max(48, d.startW + (ev.clientX - d.startX));
        setPrefs((p) => ({ ...p, widths: { ...p.widths, [d.key]: next } }));
      };
      const onUp = () => {
        const d = dragRef.current;
        dragRef.current = null;
        window.removeEventListener("mousemove", onMove);
        window.removeEventListener("mouseup", onUp);
        // 拖拽结束时持久化最终宽度
        if (d) setPrefs((p) => (saveColumnPrefs(p), p));
      };
      window.addEventListener("mousemove", onMove);
      window.addEventListener("mouseup", onUp);
    },
    [prefs.widths],
  );

  const toggleColumn = useCallback(
    (key: ColKey) => {
      if (COLS_BY_KEY[key].alwaysOn) return; // 名称列不可隐藏
      setPrefs((p) => {
        const has = p.visible.includes(key);
        const visible = has
          ? p.visible.filter((k) => k !== key)
          : // 按 ALL_COLUMNS 顺序插入，保持列序稳定
            ALL_COLUMNS.filter(
              (c) => p.visible.includes(c.key) || c.key === key,
            ).map((c) => c.key);
        const next = { ...p, visible };
        saveColumnPrefs(next);
        return next;
      });
    },
    [],
  );

  if (results.length === 0) {
    return null;
  }
  // 表格总宽 = 各可见列宽之和。显式设宽（而非 100%）后，拖动单列只改该列、
  // 表宽随之变化、超出容器即横向滚动——其它列宽保持不变（仿 Everything）。
  const totalWidth =
    INDEX_WIDTH + visibleCols.reduce((sum, c) => sum + widthOf(c.key), 0);
  return (
    <div className="result-table-wrap">
      <table className="result-table" style={{ width: totalWidth }}>
        <colgroup>
          <col className="col-index" style={{ width: INDEX_WIDTH }} />
          {visibleCols.map((c) => (
            <col key={c.key} style={{ width: widthOf(c.key) }} />
          ))}
        </colgroup>
        <thead>
          <tr
            onContextMenu={(e) => {
              e.preventDefault();
              setColMenu({ x: e.clientX, y: e.clientY });
            }}
            title="右键选择显示哪些列"
          >
            <th className="col-index" scope="col">
              #
            </th>
            {visibleCols.map((c) => (
              <th
                key={c.key}
                scope="col"
                className={`${c.cellClass} sortable`}
                onClick={() => onSort(c.key)}
                title="点击按此列排序，右键选择列"
              >
                {c.label}
                <span className="sort-arrow">
                  {sort.key === c.key ? (sort.dir === "asc" ? "▲" : "▼") : ""}
                </span>
                <span
                  className="col-resizer"
                  onMouseDown={(e) => onResizeStart(c.key, e)}
                  onClick={(e) => e.stopPropagation()}
                  title="拖拽调整列宽"
                />
              </th>
            ))}
          </tr>
        </thead>
        <tbody>
          {results.map((r, idx) => (
            <tr
              key={r.id}
              className={selected === r.id ? "selected" : undefined}
              onClick={() => onSelect(r.id)}
              onDoubleClick={() => onOpen(r.path)}
              onContextMenu={(e) => {
                e.preventDefault();
                onSelect(r.id);
                onContextMenu(e.clientX, e.clientY, r);
              }}
              title={`${r.source} · ${r.match_type} — 双击打开，右键更多`}
            >
              <td className="col-index">{idx + 1}</td>
              {visibleCols.map((c) => (
                <td key={c.key} className={c.cellClass}>
                  {c.render(r)}
                </td>
              ))}
            </tr>
          ))}
        </tbody>
      </table>

      {colMenu && (
        <ColumnChooser
          x={colMenu.x}
          y={colMenu.y}
          visible={prefs.visible}
          onToggle={toggleColumn}
          onReset={() => persist(defaultColumnPrefs())}
          onClose={() => setColMenu(null)}
        />
      )}
    </div>
  );
}

// ---- 列选择器（右键列头） ----

function ColumnChooser({
  x,
  y,
  visible,
  onToggle,
  onReset,
  onClose,
}: {
  x: number;
  y: number;
  visible: ColKey[];
  onToggle: (key: ColKey) => void;
  onReset: () => void;
  onClose: () => void;
}) {
  return (
    <>
      <div
        className="context-menu-backdrop"
        onClick={onClose}
        onContextMenu={(e) => {
          e.preventDefault();
          onClose();
        }}
      />
      <ul className="context-menu column-chooser" style={{ left: x, top: y }}>
        {ALL_COLUMNS.map((c) => {
          const checked = visible.includes(c.key);
          return (
            <li
              key={c.key}
              className={c.alwaysOn ? "disabled" : undefined}
              onClick={() => onToggle(c.key)}
            >
              <span className="check">{checked ? "✓" : ""}</span>
              {c.label}
            </li>
          );
        })}
        <li className="separator" />
        <li
          onClick={() => {
            onReset();
            onClose();
          }}
        >
          <span className="check" />
          恢复默认列
        </li>
      </ul>
    </>
  );
}

// ---- 意图信息条 ----

function IntentBar({
  intent,
  count,
  streaming,
  elapsed_ms,
  draftOpen,
  onToggleDraft,
}: {
  intent: IntentSummary;
  count: number;
  streaming: boolean;
  elapsed_ms?: number;
  // BETA-29：草稿面板显隐 + 切换（undefined = 本轮无 intent_json，隐藏入口）。
  draftOpen?: boolean;
  onToggleDraft?: () => void;
}) {
  return (
    <div className="intent-bar">
      <span className="intent-label">意图</span>
      <code>{intent.intent_summary}</code>
      {intent.signals.length > 0 && (
        <code className="intent-signals" title={intent.signals.join(", ")}>
          {intent.signals.join(", ")}
        </code>
      )}
      {intent.fallback_used && <span className="intent-fallback">模型补全</span>}
      <span className="intent-tool">via {intent.tool_id}</span>
      {onToggleDraft && (
        <button
          type="button"
          className="intent-draft-toggle"
          onClick={onToggleDraft}
          title="查看并修正本轮搜索条件（类型 / 时间 / 排序 / 关键词），修正后重跑"
        >
          {draftOpen ? "收起调整 ▴" : "调整 ▾"}
        </button>
      )}
      <span className="intent-stats">
        {streaming
          ? `${count} 条 · 流式中…`
          : `${count} 条${elapsed_ms !== undefined ? ` · ${elapsed_ms}ms` : ""}`}
      </span>
    </div>
  );
}

// ---- BETA-29 意图草稿面板 ----

// 下拉选项：与 schema 的 FileType / RelativeTime / SortOrder wire 值一一对应。
const DRAFT_FILE_TYPES: { value: string; label: string }[] = [
  { value: "document", label: "文档" },
  { value: "spreadsheet", label: "表格" },
  { value: "presentation", label: "演示文稿" },
  { value: "image", label: "图片" },
  { value: "screenshot", label: "截图" },
  { value: "video", label: "视频" },
  { value: "audio", label: "音频" },
  { value: "archive", label: "压缩包" },
  { value: "code", label: "代码" },
  { value: "executable", label: "可执行" },
];

const DRAFT_TIMES: { value: string; label: string }[] = [
  { value: "today", label: "今天" },
  { value: "yesterday", label: "昨天" },
  { value: "last_3_days", label: "最近 3 天" },
  { value: "last_7_days", label: "最近 7 天" },
  { value: "last_14_days", label: "最近 14 天" },
  { value: "last_30_days", label: "最近 30 天" },
  { value: "this_week", label: "本周" },
  { value: "last_week", label: "上周" },
  { value: "this_month", label: "本月" },
  { value: "last_month", label: "上月" },
  { value: "this_year", label: "今年" },
  { value: "last_year", label: "去年" },
];

const DRAFT_SORTS: { value: string; label: string }[] = [
  { value: "relevance_desc", label: "相关度" },
  { value: "modified_desc", label: "最近修改" },
  { value: "modified_asc", label: "最早修改" },
  { value: "created_desc", label: "最近创建" },
  { value: "created_asc", label: "最早创建" },
  { value: "accessed_desc", label: "最近访问" },
  { value: "size_desc", label: "从大到小" },
  { value: "size_asc", label: "从小到大" },
  { value: "name_asc", label: "名称 A→Z" },
  { value: "name_desc", label: "名称 Z→A" },
];

// 「保持原样」哨兵：多类型（BETA-18 数组）与非 relative 时间（absolute/before/after）
// 草稿 v1 不提供细粒度编辑，选中此项 = 重跑时原字段不动。
const KEEP_AS_IS = "__keep__";

/**
 * BETA-29 意图草稿面板：展示本轮生效 intent 的关键字段（关键词 / 扩展名 / 类型 /
 * 修改时间 / 排序），用户一键修正后重跑。未编辑字段（location / size / media 专有
 * 字段等）经 `base` 展开原样回传，不丢失。
 */
function IntentDraftPanel({
  base,
  busy,
  onRerun,
  onSaveDraft,
  rerunLabel,
}: {
  base: IntentJson;
  busy: boolean;
  onRerun: (intent: IntentJson) => void;
  // BETA-29 v2：把当前草稿保存为「保存的搜索」（带意图）；缺省隐藏按钮。
  onSaveDraft?: (intent: IntentJson) => void;
  // BETA-29 v2：主按钮文案（搜索前预览用「按此条件搜索」，默认「按此条件重跑」）。
  rerunLabel?: string;
}) {
  const [keywords, setKeywords] = useState<string[]>([]);
  const [extensions, setExtensions] = useState<string[]>([]);
  // "" = 不限 / KEEP_AS_IS = 保持原样（多类型 / 自定义时间）
  const [fileType, setFileType] = useState("");
  const [time, setTime] = useState("");
  const [sortSel, setSortSel] = useState("");
  const [newKw, setNewKw] = useState("");

  // base 变化（新一轮搜索的 started 回带）→ 重置本地草稿到实际生效值。
  useEffect(() => {
    setKeywords(base.keywords ?? []);
    setExtensions(base.extensions ?? []);
    const ft = base.file_type;
    setFileType(
      ft == null
        ? ""
        : Array.isArray(ft)
          ? ft.length === 1
            ? ft[0]
            : KEEP_AS_IS
          : ft,
    );
    const mt = base.modified_time;
    setTime(
      mt == null ? "" : mt.type === "relative" && mt.value ? mt.value : KEEP_AS_IS,
    );
    setSortSel(base.sort ?? "");
    setNewKw("");
  }, [base]);

  const addKeyword = () => {
    const kw = newKw.trim();
    if (kw && !keywords.includes(kw)) {
      setKeywords([...keywords, kw]);
    }
    setNewKw("");
  };

  // 从 base 拷贝起步：草稿未覆盖的字段（location / size / media 专有等）原样保留。
  // 重跑与保存草稿共用同一份组装逻辑（保证「所存即所跑」）。
  const buildDraft = (): IntentJson => {
    const next: IntentJson = { ...base };
    if (keywords.length > 0) next.keywords = keywords;
    else delete next.keywords;
    if (extensions.length > 0) next.extensions = extensions;
    else delete next.extensions;
    if (fileType === "") delete next.file_type;
    else if (fileType !== KEEP_AS_IS) next.file_type = fileType;
    if (time === "") delete next.modified_time;
    else if (time !== KEEP_AS_IS) next.modified_time = { type: "relative", value: time };
    if (sortSel === "") delete next.sort;
    else next.sort = sortSel;
    return next;
  };

  const rerun = () => onRerun(buildDraft());

  const ftHasMulti = Array.isArray(base.file_type) && base.file_type.length > 1;
  const timeIsCustom =
    base.modified_time != null && base.modified_time.type !== "relative";

  return (
    <div className="intent-draft">
      <div className="intent-draft-row">
        <span className="intent-draft-label">关键词</span>
        {keywords.map((kw) => (
          <span key={kw} className="intent-draft-chip">
            {kw}
            <button
              type="button"
              title="移除此关键词"
              onClick={() => setKeywords(keywords.filter((k) => k !== kw))}
            >
              ×
            </button>
          </span>
        ))}
        <input
          className="intent-draft-kw-input"
          value={newKw}
          placeholder="添加关键词…"
          onChange={(e) => setNewKw(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === "Enter") {
              e.preventDefault();
              addKeyword();
            }
          }}
          onBlur={addKeyword}
        />
        {extensions.length > 0 && (
          <>
            <span className="intent-draft-label">扩展名</span>
            {extensions.map((ext) => (
              <span key={ext} className="intent-draft-chip">
                .{ext}
                <button
                  type="button"
                  title="移除此扩展名"
                  onClick={() =>
                    setExtensions(extensions.filter((x) => x !== ext))
                  }
                >
                  ×
                </button>
              </span>
            ))}
          </>
        )}
      </div>
      <div className="intent-draft-row">
        <span className="intent-draft-label">类型</span>
        <select value={fileType} onChange={(e) => setFileType(e.target.value)}>
          <option value="">不限</option>
          {ftHasMulti && <option value={KEEP_AS_IS}>多类型（保持原样）</option>}
          {DRAFT_FILE_TYPES.map((t) => (
            <option key={t.value} value={t.value}>
              {t.label}
            </option>
          ))}
        </select>
        <span className="intent-draft-label">修改时间</span>
        <select value={time} onChange={(e) => setTime(e.target.value)}>
          <option value="">不限</option>
          {timeIsCustom && <option value={KEEP_AS_IS}>自定义（保持原样）</option>}
          {DRAFT_TIMES.map((t) => (
            <option key={t.value} value={t.value}>
              {t.label}
            </option>
          ))}
        </select>
        <span className="intent-draft-label">排序</span>
        <select value={sortSel} onChange={(e) => setSortSel(e.target.value)}>
          <option value="">默认</option>
          {DRAFT_SORTS.map((s) => (
            <option key={s.value} value={s.value}>
              {s.label}
            </option>
          ))}
        </select>
        <button
          type="button"
          className="intent-draft-rerun"
          disabled={busy}
          onClick={rerun}
        >
          {rerunLabel ?? "按此条件重跑"}
        </button>
        {onSaveDraft && (
          <button
            type="button"
            className="intent-draft-save"
            onClick={() => onSaveDraft(buildDraft())}
            title="把当前草稿保存为「保存的搜索」（重跑时按此条件执行）"
          >
            保存草稿…
          </button>
        )}
      </div>
    </div>
  );
}

// ---- 底部状态栏 ----

function StatusBar({
  status,
  count,
  total,
  selected,
  actionMsg,
}: {
  status: Status;
  /** 当前显示（筛选后）的条数 */
  count: number;
  /** 筛选前的总条数 */
  total: number;
  selected: string | null;
  actionMsg: string | null;
}) {
  let text: string;
  // 是否处于筛选态（显示数 < 总数）
  const filtered = count < total;
  switch (status.kind) {
    case "idle":
      text = "就绪";
      break;
    case "streaming":
      text = `正在搜索… 已返回 ${total} 个对象`;
      break;
    case "ready":
      if (status.total === 0) {
        text = "0 个对象";
      } else {
        text =
          (filtered ? `${count} / ${total} 个对象（已筛选）` : `${total} 个对象`) +
          ` · ${status.elapsed_ms}ms` +
          (selected ? " · 已选中 1 项" : "");
      }
      break;
    case "action_done":
      text = "操作完成";
      break;
    case "confirm_pending":
      text = "等待确认…";
      break;
    case "error":
      text = "出错";
      break;
  }
  return (
    <footer className="status-bar">
      {actionMsg ? <span className="status-action">{actionMsg}</span> : text}
    </footer>
  );
}

// ---- 排序 ----

function sortResults(
  results: SearchResultJson[],
  sort: SortState,
): SearchResultJson[] {
  if (sort.key === null) {
    return results;
  }
  const col = COLS_BY_KEY[sort.key];
  const factor = sort.dir === "asc" ? 1 : -1;
  // 复制后排序，避免原数组（ref 来源）被原地修改
  return [...results].sort((a, b) => {
    const va = col.sortValue(a);
    const vb = col.sortValue(b);
    let cmp: number;
    if (typeof va === "number" && typeof vb === "number") {
      cmp = va - vb;
    } else {
      cmp = String(va).localeCompare(String(vb), "zh-CN");
    }
    return cmp * factor;
  });
}

// ---- 格式化辅助 ----

function formatDate(iso: string): string {
  const d = new Date(iso);
  return d.toLocaleString("zh-CN", {
    year: "numeric",
    month: "2-digit",
    day: "2-digit",
    hour: "2-digit",
    minute: "2-digit",
  });
}

function formatSize(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
  if (bytes < 1024 * 1024 * 1024) return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
  return `${(bytes / (1024 * 1024 * 1024)).toFixed(2)} GB`;
}

function basename(p: string): string {
  const parts = p.split(/[\\/]/);
  return parts[parts.length - 1] || p;
}

// 取目录部分（去掉文件名），用于「路径」列；Everything 的路径列展示所在目录
function dirOf(p: string): string {
  const parts = p.split(/[\\/]/);
  if (parts.length <= 1) return p;
  parts.pop();
  return parts.join("\\");
}

// 取文件扩展名（小写，不含点）；无扩展名返回空串。用于「扩展名」列与排序。
function extOf(name: string): string {
  const dot = name.lastIndexOf(".");
  return dot > 0 ? name.slice(dot + 1).toLowerCase() : "";
}

// 按扩展名给文件类型图标。使用统一图标系统，避免不同平台 emoji 风格漂移。
function fileGlyph(name: string): React.ReactNode {
  const ext = name.includes(".") ? name.split(".").pop()!.toLowerCase() : "";
  if (["png", "jpg", "jpeg", "gif", "bmp", "webp", "svg", "heic"].includes(ext))
    return <FileImage size={18} weight="duotone" />;
  if (["mp4", "mov", "avi", "mkv", "webm"].includes(ext))
    return <FileVideo size={18} weight="duotone" />;
  if (["mp3", "wav", "flac", "aac", "m4a", "ogg"].includes(ext))
    return <FileAudio size={18} weight="duotone" />;
  if (ext === "pdf") return <FilePdf size={18} weight="duotone" />;
  if (["doc", "docx"].includes(ext))
    return <FileDoc size={18} weight="duotone" />;
  if (["xls", "xlsx", "csv"].includes(ext))
    return <FileXls size={18} weight="duotone" />;
  if (["ppt", "pptx"].includes(ext))
    return <FilePpt size={18} weight="duotone" />;
  if (["zip", "rar", "7z", "tar", "gz"].includes(ext))
    return <FileArchive size={18} weight="duotone" />;
  if (["txt", "md", "log", "json", "yaml", "yml"].includes(ext))
    return <FileText size={18} weight="duotone" />;
  return <File size={18} weight="duotone" />;
}

// 后端 tool_id → 用户友好名（fallback 提示用）。未知 id 原样返回。
function friendlyBackend(id: string): string {
  switch (id) {
    case "search.windows":
      return "Windows Search";
    case "search.native_file_index":
      return "内置原生索引";
    case "search.spotlight":
      return "Spotlight";
    case "search.local":
      return "本地索引";
    default:
      return id;
  }
}

// 切换原因 → 中文短语（与 fallback_chain::SwitchReason::as_str 对应）。
function friendlyReason(reason: string): string {
  switch (reason) {
    case "empty":
      return "无结果";
    case "unavailable":
      return "不可用";
    case "error":
      return "出错";
    default:
      return reason;
  }
}

function describeAction(kind: string, paths: string[]): string {
  const verb =
    kind === "locate"
      ? "已在文件管理器中显示"
      : kind === "copy"
        ? "已复制"
        : kind === "move"
          ? "已移动"
          : kind === "rename"
            ? "已重命名"
            : "已打开";
  if (paths.length === 1) {
    return `${verb} ${basename(paths[0])}`;
  }
  return `${verb} ${paths.length} 个文件`;
}

function describeConfirm(
  kind: string,
  paths: string[],
  destination: string | null,
  newName: string | null,
): string {
  const count = paths.length;
  const subject = count === 1 ? basename(paths[0]) : `${count} 个文件`;
  if (kind === "copy") {
    return `复制 ${subject} 到 ${destination ?? ""}?`;
  }
  if (kind === "move") {
    return `移动 ${subject} 到 ${destination ?? ""}?`;
  }
  if (kind === "rename") {
    return `重命名 ${basename(paths[0] ?? "")} 为 ${newName ?? ""}?`;
  }
  return `确认对 ${subject} 执行 ${kind}?`;
}
