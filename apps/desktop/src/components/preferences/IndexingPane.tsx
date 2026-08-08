import { useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { AppSettings } from "../../hooks/useAppSettings";
import {
  AuditEntry,
  ExtractionFailure,
  IndexStatus,
  RootIndexOverview,
  StageTimings,
  formatBytes,
  formatIndexTime,
  phaseChipLabel,
  phaseProgressText,
} from "./shared";

// 与 privacy.rs::DataLocation 对应（BETA-21）。
interface DataLocation {
  label: string;
  path: string;
  exists: boolean;
  size_bytes: number;
}

// 与 privacy.rs::PrivacyOverview 对应（BETA-21）——本面板只用得到数据存储位置 /
// 搜索历史条数 / 调试追踪状态；已索引条数等字段「索引概貌」卡片另有更完整的展示
// （分色分布条 + 实时索引中状态），此处不重复声明。
interface PrivacyOverview {
  data_root: string;
  locations: DataLocation[];
  search_history_count: number;
  tracing_enabled: boolean;
}

// 与 uninstall.rs::CleanupItem / CleanupReport 对应（BETA-12）。
interface CleanupItem {
  label: string;
  path: string;
  existed: boolean;
  removed: boolean;
  detail: string | null;
}

interface CleanupReport {
  items: CleanupItem[];
  all_ok: boolean;
}

/** 2026-07-28：`last_run_stage_ms` 三个槽位 → 展示用的 (label, timings) 列表，跳过未知槽位。 */
const STAGE_ROWS: { key: "doc" | "image" | "music"; label: string }[] = [
  { key: "doc", label: "文档" },
  { key: "image", label: "图片" },
  { key: "music", label: "音频" },
];

function stageTotalMs(t: StageTimings): number {
  return t.walk_ms + t.extract_ms + t.write_ms + t.recycle_ms;
}

/**
 * BETA-33 cycle 5：单个索引 root 行。
 * `overview` = null 时统计显示"…"（尚未加载）。`onRemove` = null 时不显示移除按钮
 * （系统默认目录用户不能"移除"、只能通过"+ 添加目录"覆盖）。
 *
 * cycle 7-a：
 * - `isPending`：picker 加入但未保存的自定义 root，显示 `⏳ 待应用` 琥珀 badge。
 * - `flash`：picker 后 1.5s CSS flash 高亮 + scrollIntoView（消除"选了没反应"错觉）。
 */
function RootRow({
  path,
  isSystemDefault,
  overview,
  onRemove,
  isPending,
  flash,
  excludePatterns,
  onUpdateExcludes,
  onOpenDir,
  onRescan,
  rescanDisabled,
}: {
  path: string;
  isSystemDefault: boolean;
  overview: RootIndexOverview | null;
  onRemove: (() => void) | null;
  isPending?: boolean;
  flash?: boolean;
  /** cycle 7-b：该 root 的 per-root 子路径 exclude patterns（默认空）。 */
  excludePatterns?: string[];
  /** cycle 7-b：更新 patterns 回调；null = 只读（例如 fallback、无 root_excludes wiring）。 */
  onUpdateExcludes?: ((patterns: string[]) => void) | null;
  /** cycle 7-c：在系统文件管理器中打开该目录。 */
  onOpenDir?: () => void;
  /** cycle 7-c：单目录重扫；null = 不显示（如待应用的 pending root，排除配置尚未保存）。 */
  onRescan?: (() => void) | null;
  /** cycle 7-c：重扫按钮禁用（全局索引中）。 */
  rescanDisabled?: boolean;
}) {
  const stats = overview
    ? [
        `文档 ${overview.doc_count.toLocaleString()}`,
        `图片 ${overview.image_count.toLocaleString()}`,
        `音频 ${overview.music_count.toLocaleString()}`,
      ].join(" · ")
    : "…";
  const lastIndexed = overview?.last_indexed_time
    ? formatIndexTime(overview.last_indexed_time)
    : null;
  const rowRef = useRef<HTMLDivElement>(null);
  const [expanded, setExpanded] = useState(false);
  const [patternDraft, setPatternDraft] = useState("");
  useEffect(() => {
    if (flash) {
      rowRef.current?.scrollIntoView({ behavior: "smooth", block: "nearest" });
    }
  }, [flash]);
  const cls = [
    "prefs-root-row",
    isPending ? "pending" : "",
    flash ? "flash" : "",
  ]
    .filter(Boolean)
    .join(" ");
  const patterns = excludePatterns ?? [];
  const excludeEditable = onUpdateExcludes != null;
  const addPattern = () => {
    const t = patternDraft.trim();
    if (!t || !onUpdateExcludes) return;
    if (!patterns.includes(t)) {
      onUpdateExcludes([...patterns, t]);
    }
    setPatternDraft("");
  };
  return (
    <>
      {/* 2026-07-06（cycle 9 真机反馈二轮）：三行卡片式布局——单行 flex 会把路径列
          挤到极窄逐字断行。行 1 完整路径（独占整宽）、行 2 索引内容统计、行 3 操作按钮。 */}
      <div className={cls} ref={rowRef}>
        <div className="prefs-root-line">
          <span
            className={`prefs-root-path${isSystemDefault ? " sys" : ""}`}
            title={path}
          >
            📂 {path}
          </span>
          {isSystemDefault && <span className="prefs-root-tag">系统默认</span>}
          {isPending && (
            <span className="prefs-root-tag pending" title="picker 加入但未保存">
              ⏳ 待应用
            </span>
          )}
        </div>
        <div className="prefs-root-line">
          <span
            className="prefs-root-stats"
            title="该目录下索引条数（文档 · 图片 · 音频）"
          >
            {stats}
          </span>
          {lastIndexed && (
            <span
              className="prefs-root-time"
              title={`上次索引：${overview?.last_indexed_time ?? ""}`}
            >
              上次索引 {lastIndexed}
            </span>
          )}
        </div>
        <div className="prefs-root-line prefs-root-actions">
          {excludeEditable && (
            <button
              type="button"
              className={`prefs-btn small${patterns.length > 0 ? " has-excludes" : ""}`}
              onClick={() => setExpanded(!expanded)}
              title="配置该目录下的子路径排除（通配符）"
            >
              {expanded ? "▾" : "▸"} 子路径排除
              {patterns.length > 0 ? ` (${patterns.length})` : ""}
            </button>
          )}
          {onOpenDir && (
            <button
              type="button"
              className="prefs-btn small"
              onClick={onOpenDir}
              title="在系统文件管理器中打开该目录"
            >
              打开
            </button>
          )}
          {onRescan && (
            <button
              type="button"
              className="prefs-btn small"
              onClick={onRescan}
              disabled={rescanDisabled}
              title="只重扫该目录（排除规则仍生效，不影响其他目录）"
            >
              重扫
            </button>
          )}
          {onRemove && (
            <button type="button" className="prefs-btn small" onClick={onRemove}>
              移除
            </button>
          )}
        </div>
      </div>
      {excludeEditable && expanded && (
        <div className="prefs-root-excludes">
          <p className="prefs-hint">
            相对该目录的通配符：<code>**</code>=任意层，<code>*</code>=单段，
            <code>?</code>=单字符。示例：<code>临时/**</code>、
            <code>**/backup/**</code>、<code>*.old/*</code>。
          </p>
          {patterns.map((p, i) => (
            <div key={i} className="prefs-exclude-row">
              <code>{p}</code>
              <button
                type="button"
                className="prefs-btn small"
                onClick={() => {
                  if (!onUpdateExcludes) return;
                  onUpdateExcludes(patterns.filter((_, j) => j !== i));
                }}
              >
                移除
              </button>
            </div>
          ))}
          <div className="prefs-exclude-add-row">
            <input
              type="text"
              className="prefs-input"
              value={patternDraft}
              onChange={(e) => setPatternDraft(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === "Enter") addPattern();
              }}
              placeholder="如 临时/** 或 **/backup/**"
            />
            <button
              type="button"
              className="prefs-btn"
              onClick={addPattern}
              disabled={!patternDraft.trim()}
            >
              添加
            </button>
          </div>
        </div>
      )}
    </>
  );
}

export function IndexingPane({
  settings,
  setSettings,
  initialIndexRoots,
  effectiveRoots,
  indexOverview,
  indexStatus,
  indexStatusLine,
  extractionFailures,
  semanticLine,
  reindexing,
  reindexMsg,
  onReindex,
  onReindexRoot,
  onOpenRoot,
  onRequestRemoveRoot,
  onPickMessage,
  flashPath,
  onFlash,
  auditLog,
  onReloadAuditLog,
  onClearAuditLog,
}: {
  settings: AppSettings;
  setSettings: (s: AppSettings) => void;
  initialIndexRoots: string[];
  effectiveRoots: string[] | null;
  indexOverview: RootIndexOverview[] | null;
  indexStatus: IndexStatus | null;
  indexStatusLine: string;
  /** BETA-40：文件级提取失败留痕（null = 加载中）。 */
  extractionFailures: ExtractionFailure[] | null;
  semanticLine: string | null;
  reindexing: boolean;
  reindexMsg: string;
  onReindex: () => void;
  /** cycle 7-c：单目录重扫。 */
  onReindexRoot: (path: string) => void;
  /** cycle 7-c：文件管理器打开目录。 */
  onOpenRoot: (path: string) => void;
  /** cycle 7-c：移除目录（父组件弹二次确认、可选 purge）。 */
  onRequestRemoveRoot: (path: string) => void;
  onPickMessage: (m: string) => void;
  flashPath: string | null;
  onFlash: (path: string) => void;
  /** 2026-07-29：原「隐私与记录」tab 并入本面板——操作记录数据仍由对话框壳提供（props）。 */
  auditLog: AuditEntry[];
  onReloadAuditLog: () => void;
  onClearAuditLog: () => void;
}) {
  const [excludeDraft, setExcludeDraft] = useState("");
  // BETA-40：「未能索引的文件」清单折叠态（默认收起，仅显示条数）。
  const [failuresExpanded, setFailuresExpanded] = useState(false);
  // 2026-07-28：「本次索引用时明细」折叠态（默认收起）。
  const [stageDetailExpanded, setStageDetailExpanded] = useState(false);

  // 2026-07-29：原「隐私与记录」tab 的数据/隐私管理状态（本地状态、不经 props——
  // 与桌面壳共享的只有 auditLog，其余是本面板私有的加载/确认态）。
  const [overview, setOverview] = useState<PrivacyOverview | null>(null);
  const [clearMsg, setClearMsg] = useState("");
  const [confirmIndex, setConfirmIndex] = useState(false);
  const [working, setWorking] = useState(false);
  const [confirmCleanup, setConfirmCleanup] = useState(false);
  const [cleanupReport, setCleanupReport] = useState<CleanupReport | null>(
    null,
  );
  const [cleanupMsg, setCleanupMsg] = useState("");

  const loadOverview = () => {
    invoke<PrivacyOverview>("get_privacy_overview")
      .then(setOverview)
      .catch(console.error);
  };

  useEffect(() => {
    loadOverview();
    // 索引可能在后台进行，轻度轮询刷新统计（历史文件大小 / 搜索历史条数）。
    const timer = setInterval(loadOverview, 3000);
    return () => clearInterval(timer);
  }, []);

  const handleClearHistory = async () => {
    setWorking(true);
    setClearMsg("");
    try {
      await invoke("clear_search_history");
      setClearMsg("搜索历史已清除");
      loadOverview();
    } catch (err) {
      setClearMsg(`清除失败: ${err}`);
    } finally {
      setWorking(false);
    }
  };

  const handleClearIndex = async () => {
    setWorking(true);
    setClearMsg("");
    setConfirmIndex(false);
    try {
      await invoke("clear_local_index");
      setClearMsg("本地索引已清空（下次索引会重建）");
      loadOverview();
    } catch (err) {
      setClearMsg(`清除失败: ${err}`);
    } finally {
      setWorking(false);
    }
  };

  // BETA-12：卸载清理（删索引/模型/日志/操作记录/搜索历史/用户同义词库，保留设置）。
  const handleUninstallCleanup = async () => {
    setWorking(true);
    setCleanupMsg("");
    setCleanupReport(null);
    setConfirmCleanup(false);
    try {
      const report = await invoke<CleanupReport>("uninstall_cleanup");
      setCleanupReport(report);
      setCleanupMsg(
        report.all_ok
          ? "清理完成，设置已保留。现在可以放心卸载 Scout。"
          : "部分项目未能删除，详见下表。",
      );
      loadOverview();
    } catch (err) {
      setCleanupMsg(`清理失败: ${err}`);
    } finally {
      setWorking(false);
    }
  };

  const addExclude = () => {
    const t = excludeDraft.trim();
    if (!t) return;
    if (!settings.exclude_globs.includes(t)) {
      setSettings({
        ...settings,
        exclude_globs: [...settings.exclude_globs, t],
      });
    }
    setExcludeDraft("");
  };

  // 按 path 找对应统计（overview 里的顺序 = effectiveRoots 顺序、但按 path 匹配更稳）。
  const overviewOf = (path: string): RootIndexOverview | null =>
    indexOverview?.find((o) => o.path === path) ?? null;

  // 顶部总览合计（跨所有 root）。
  const totalDocs = indexOverview?.reduce((s, o) => s + o.doc_count, 0) ?? 0;
  const totalImages =
    indexOverview?.reduce((s, o) => s + o.image_count, 0) ?? 0;
  const totalMusic = indexOverview?.reduce((s, o) => s + o.music_count, 0) ?? 0;
  const grandTotal = totalDocs + totalImages + totalMusic;
  // cycle 7-a：数据源统一（Codex APPROVED 2 · 选 a）——概貌"上次索引"用 indexOverview.max()、
  // 与「本地索引」区文案一致；避免出现"顶部 Downloads-only 数字 vs 底部全库数字"两套口径。
  const latestTime = indexOverview
    ?.map((o) => o.last_indexed_time)
    .filter((t): t is string => !!t)
    .sort()
    .pop();

  // cycle 9：口径统一明示——概貌是「当前生效目录内」口径、「本地索引」行 last_summary 是
  // 「全库」口径，两者可合法不一致（「仅移除」目录保留的记录 / override 前旧默认目录的
  // 记录仍在库且仍可被搜索命中）。全库 > 概貌合计时显式提示差值来源，不放任两个数字
  // 各说各话。反向（概貌 > 全库，生效目录相互嵌套导致重复计数）不提示、属已知统计特性。
  const dbGrand = indexStatus?.db_totals
    ? indexStatus.db_totals[0] + indexStatus.db_totals[1] + indexStatus.db_totals[2]
    : null;
  const outsideRootsCount =
    dbGrand !== null && indexOverview !== null && dbGrand > grandTotal
      ? dbGrand - grandTotal
      : 0;

  // cycle 7-a：pending 集合——settings.index_roots 里但不在 initialIndexRoots 里 = picker 加入未保存。
  const pendingSet = new Set(
    settings.index_roots.filter((p) => !initialIndexRoots.includes(p)),
  );

  // cycle 7-b：查某 root 对应的 excludePatterns。后端按 normalize_root_key 归一化匹配、
  // 但前端保留 display 形式（跟 settings.index_roots 字符串一致）；简单按等值匹配。
  const excludesFor = (rootPath: string): string[] => {
    return (
      settings.root_excludes.find((re) => re.root === rootPath)?.patterns ?? []
    );
  };
  const updateExcludesFor = (rootPath: string, patterns: string[]) => {
    const others = settings.root_excludes.filter((re) => re.root !== rootPath);
    if (patterns.length === 0) {
      // 空 patterns → 从 root_excludes 里删（避免存空条目）
      setSettings({ ...settings, root_excludes: others });
    } else {
      setSettings({
        ...settings,
        root_excludes: [...others, { root: rootPath, patterns }],
      });
    }
  };
  // cycle 7-c：移除 root 走父组件的二次确认弹窗（onRequestRemoveRoot），
  // 确认后由父组件同步删 root_excludes 条目（不留孤儿）+ 可选 purge 索引记录。

  // 2026-07-25：概貌卡片改版——分色分布条取代六格纯数字网格，状态用 chip 一眼可辨。
  const idxSegments = [
    { key: "doc", label: "文档", count: totalDocs, color: "var(--accent)" },
    { key: "image", label: "图片", count: totalImages, color: "var(--accent-violet)" },
    { key: "music", label: "音频", count: totalMusic, color: "var(--accent-teal)" },
  ];
  const isIndexing = indexStatus?.indexing ?? false;
  const ftsProgress = isIndexing ? indexStatus?.fts_progress ?? null : null;
  const semProgress =
    isIndexing && indexStatus?.semantic_indexing
      ? indexStatus?.semantic_progress ?? null
      : null;
  // 2026-07-28：真百分比 + ETA（有 phase_total/phase_scanned 时）；否则 null，
  // 上面渲染处退回裸数字展示。
  const phaseProgress = isIndexing
    ? phaseProgressText(
        indexStatus?.phase_total ?? null,
        indexStatus?.phase_scanned ?? null,
        indexStatus?.phase_rate_per_min ?? null,
      )
    : null;
  const stageMs = indexStatus?.last_run_stage_ms ?? null;
  const hasStageDetail =
    stageMs !== null && (stageMs.doc || stageMs.image || stageMs.music);

  return (
    <div className="prefs-form">
      {/* BETA-33 cycle 5：顶部概貌卡片——总目录 / 分类分总 / 上次索引
          2026-07-25：改为总数 + 状态 chip + 分色分布条 + 图例。 */}
      <div className="prefs-overview-card">
        <div className="prefs-overview-title">索引概貌</div>
        {indexOverview === null ? (
          <p className="prefs-hint">加载中…</p>
        ) : indexOverview.length === 0 ? (
          <p className="prefs-hint err">
            ⚠️ 无生效索引目录（未添加 + 系统未检测到默认音频/文档/图片目录）。
          </p>
        ) : (
          <>
            <div className="idx-overview-head">
              <div>
                <span className="idx-total">{grandTotal.toLocaleString()}</span>
                <span className="idx-total-label">
                  条已索引 · {indexOverview.length} 个生效目录
                </span>
              </div>
              <span
                className={`idx-chip ${isIndexing ? "idx-chip-warn" : "idx-chip-ok"}`}
              >
                <span className="idx-chip-dot" />
                {isIndexing ? "索引中" : "就绪"}
              </span>
            </div>
            <div className="idx-seg-bar">
              {idxSegments.map((seg) => (
                <span
                  key={seg.key}
                  className="idx-seg"
                  style={{
                    width: `${grandTotal > 0 ? (seg.count / grandTotal) * 100 : 0}%`,
                    background: seg.color,
                  }}
                  title={`${seg.label} ${seg.count.toLocaleString()}`}
                />
              ))}
            </div>
            <div className="idx-seg-legend">
              {idxSegments
                .filter((seg) => seg.count > 0)
                .map((seg) => (
                  <span key={seg.key} className="idx-seg-legend-item">
                    <span
                      className="idx-seg-dot"
                      style={{ background: seg.color }}
                    />
                    {seg.label} {seg.count.toLocaleString()}
                  </span>
                ))}
            </div>
          </>
        )}
        {/* cycle 9：全库 vs 概貌口径差显式提示（差值来源 + 清理路径），替代两个数字各说各话。 */}
        {outsideRootsCount > 0 && (
          <p className="prefs-hint" style={{ marginTop: "8px" }}>
            ℹ️ 库内另有 <strong>{outsideRootsCount.toLocaleString()}</strong>{" "}
            条记录在当前生效目录之外（来自已移除的目录或旧配置），搜索仍会命中它们。
            如需清理：移除目录时选「移除并清除索引记录」，或在本页下方「清空本地索引」后重建。
          </p>
        )}
      </div>

      {/* 2026-07-25：管道状态卡片——索引进行中时展示当前阶段 + 全文/语义扫描进度；
          空闲时只留一行「最后索引」，避免展示已归零 / 失效的 progress 数字。 */}
      {indexStatus && (
        <div className="idx-pipeline">
          <div className="idx-pipeline-row">
            <span className="idx-pipeline-label">状态</span>
            <span className="idx-pipeline-value">
              {isIndexing
                ? indexStatus.current_phase
                  ? phaseChipLabel(indexStatus.current_phase)
                  : "扫描准备中…"
                : "空闲"}
            </span>
          </div>
          {ftsProgress && (
            <div className="idx-pipeline-row">
              <span className="idx-pipeline-label">全文搜索</span>
              {/* 2026-07-28：phase_total/phase_scanned 有值时是真百分比 + 速率/ETA
                  （数据来自当前 phase 的 walk/发现结果，不是编造）；没有时（如发现层
                  不可用的极端 fallback、phase 刚切换还没跑完 walk）退回裸数字展示，
                  呼应 cycle 7-a「不编造百分比」的既有原则。 */}
              {phaseProgress ? (
                <span className="idx-pipeline-value">
                  <span className="idx-bar-track">
                    <span
                      className="idx-bar-fill"
                      style={{ width: `${phaseProgress.pct}%` }}
                    />
                  </span>
                  <span className="idx-bar-num">{phaseProgress.label}</span>
                </span>
              ) : (
                <span className="idx-bar-num">
                  已扫描 {ftsProgress[0].toLocaleString()} · 已入库{" "}
                  {ftsProgress[1].toLocaleString()}
                </span>
              )}
            </div>
          )}
          {semProgress && (
            <div className="idx-pipeline-row">
              <span className="idx-pipeline-label">语义搜索</span>
              <span className="idx-pipeline-value">
                <span className="idx-bar-track">
                  <span
                    className="idx-bar-fill"
                    style={{
                      width: `${semProgress[1] > 0 ? Math.min(100, (semProgress[0] / semProgress[1]) * 100) : 0}%`,
                    }}
                  />
                </span>
                <span className="idx-bar-num">
                  {semProgress[0].toLocaleString()}/{semProgress[1].toLocaleString()}
                </span>
              </span>
            </div>
          )}
          <div className="idx-pipeline-row">
            <span className="idx-pipeline-label">最后索引</span>
            <span className="idx-pipeline-value">
              {latestTime ? formatIndexTime(latestTime) : "尚未"}
            </span>
          </div>
        </div>
      )}

      {/* 2026-07-28：本次索引用时明细——读 last_run_stage_ms，索引空闲后仍保留可查。
          旧库从未产生过这个字段（升级前索引过一次、之后再没重新索引）时不渲染整块，
          不展示全空表格制造困惑。 */}
      {hasStageDetail && stageMs && (
        <div className="prefs-field">
          <button
            type="button"
            className="prefs-btn small"
            onClick={() => setStageDetailExpanded((v) => !v)}
          >
            {stageDetailExpanded ? "▾" : "▸"} 本次索引用时明细
          </button>
          {stageDetailExpanded && (
            <table className="idx-stage-table">
              <thead>
                <tr>
                  <th>类型</th>
                  <th>发现/扫描</th>
                  <th>提取</th>
                  <th>写入</th>
                  <th>回收</th>
                  <th>合计</th>
                </tr>
              </thead>
              <tbody>
                {STAGE_ROWS.map(({ key, label }) => {
                  const t = stageMs[key];
                  if (!t) return null;
                  return (
                    <tr key={key}>
                      <td>{label}</td>
                      <td>{t.walk_ms.toLocaleString()} ms</td>
                      <td>{t.extract_ms.toLocaleString()} ms</td>
                      <td>{t.write_ms.toLocaleString()} ms</td>
                      <td>{t.recycle_ms.toLocaleString()} ms</td>
                      <td>{stageTotalMs(t).toLocaleString()} ms</td>
                    </tr>
                  );
                })}
              </tbody>
            </table>
          )}
        </div>
      )}

      <div className="prefs-field">
        <label className="prefs-label">
          索引目录（生效 {effectiveRoots?.length ?? 0} 个 = 自定义{" "}
          {settings.index_roots.length} +{" "}
          {effectiveRoots
            ? Math.max(0, effectiveRoots.length - settings.index_roots.length)
            : 0}{" "}
          系统默认）
        </label>
        {/* 2026-07-06 新语义：checkbox 常显——系统三夹纳入与否完全由它决定（默认不勾 =
            不索引系统目录）；旧「覆盖语义」banner 随之退役（勾选状态自解释）。 */}
        <label className="prefs-checkbox prefs-checkbox-strong">
          <input
            type="checkbox"
            checked={settings.include_system_defaults}
            onChange={(e) =>
              setSettings({
                ...settings,
                include_system_defaults: e.target.checked,
              })
            }
          />
          <strong>同时索引系统默认目录（音频 / 文档 / 图片）</strong>
        </label>
        {/* cycle 6 v4：统一按 effectiveRoots 渲染，自定义项显示「移除」、系统默认项显示 tag。
            cycle 7-a：pending 集合传 RootRow 显示琥珀 badge；flashPath 命中的行加 CSS flash 高亮。 */}
        {effectiveRoots?.map((path, i) => {
          const isCustom = settings.index_roots.includes(path);
          const isPending = pendingSet.has(path);
          return (
            <RootRow
              key={`${isCustom ? "usr" : "sys"}-${i}`}
              path={path}
              isSystemDefault={!isCustom}
              overview={overviewOf(path)}
              isPending={isPending}
              flash={flashPath === path}
              excludePatterns={excludesFor(path)}
              onUpdateExcludes={(patterns) => updateExcludesFor(path, patterns)}
              onOpenDir={() => onOpenRoot(path)}
              // pending root 的排除配置尚未保存、重扫口径会与预期不符 → 不给重扫入口。
              onRescan={isPending ? null : () => onReindexRoot(path)}
              rescanDisabled={reindexing || (indexStatus?.indexing ?? false)}
              onRemove={isCustom ? () => onRequestRemoveRoot(path) : null}
            />
          );
        })}
        {effectiveRoots && effectiveRoots.length === 0 && (
          <p className="prefs-hint err">
            ⚠️
            尚未选择任何索引目录——默认不索引、搜索不会有本地索引结果。请「+
            添加目录」，或勾选上方系统默认目录。
          </p>
        )}
        <button
          type="button"
          className="prefs-btn"
          onClick={async () => {
            const { open } = await import("@tauri-apps/plugin-dialog");
            const picked = await open({ directory: true, multiple: false });
            if (typeof picked === "string") {
              if (settings.index_roots.includes(picked)) {
                // cycle 7-a：已在列表也 flash 一下让用户知道"没重复添加、但确实是这条"
                onFlash(picked);
                onPickMessage("该目录已在列表中");
              } else {
                setSettings({
                  ...settings,
                  index_roots: [...settings.index_roots, picked],
                });
                onFlash(picked);
                onPickMessage(
                  "已加入下方列表 · 未保存 —— 点「应用」或「确定」生效",
                );
              }
            }
          }}
        >
          + 添加目录
        </button>
      </div>

      <div className="prefs-field">
        <label className="prefs-label">
          排除目录名（通配符，留空 = 默认排除 node_modules/.git 等）
        </label>
        {settings.exclude_globs.map((g, i) => (
          <div key={i} className="prefs-root-row">
            <span className="prefs-root-path">{g}</span>
            <button
              type="button"
              className="prefs-btn small"
              onClick={() =>
                setSettings({
                  ...settings,
                  exclude_globs: settings.exclude_globs.filter(
                    (_, j) => j !== i,
                  ),
                })
              }
            >
              移除
            </button>
          </div>
        ))}
        <div style={{ display: "flex", gap: "8px" }}>
          <input
            type="text"
            className="prefs-input"
            value={excludeDraft}
            onChange={(e) => setExcludeDraft(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Enter") addExclude();
            }}
            placeholder="如 node_modules 或 *cache*"
          />
          <button type="button" className="prefs-btn" onClick={addExclude}>
            添加
          </button>
        </div>
      </div>

      <div className="prefs-field">
        <label className="prefs-label">本地索引</label>
        <p className="prefs-hint">
          建立音频 metadata 与文档内容的本地索引；应用启动时会在后台自动索引。
        </p>
        <label className="prefs-label" htmlFor="auto-index-interval">
          自动增量索引
        </label>
        <p className="prefs-hint">
          定期检查新增与变动的文件（未变化的文件不会重新索引）。
        </p>
        <select
          id="auto-index-interval"
          className="prefs-input"
          value={settings.auto_index_interval_minutes}
          onChange={(e) =>
            setSettings({
              ...settings,
              auto_index_interval_minutes: Number(e.target.value),
            })
          }
        >
          <option value={0}>关闭</option>
          <option value={15}>15 分钟</option>
          <option value={30}>30 分钟</option>
          <option value={60}>60 分钟</option>
        </select>
        {/* BETA-39：图片语义索引 opt-in。默认关（防乱码 OCR 污染语义召回）；
            开启后图片文字走更严的质量门槛（0.75）入语义索引，需重新索引生效。 */}
        <label className="prefs-checkbox">
          <input
            type="checkbox"
            checked={settings.enable_image_semantics}
            onChange={(e) =>
              setSettings({
                ...settings,
                enable_image_semantics: e.target.checked,
              })
            }
          />
          <span>
            <strong>让图片文字参与语义搜索（实验性）</strong>
            <br />
            <span className="prefs-hint">
              默认关闭：图片 OCR 文字仅支持字面（关键词）匹配。开启后，通过更严格质量门槛的图片文字（如聊天截图、扫描笔记）也能被「按意思」搜到；乱码 OCR 会被自动挡下。
              <strong>需重新索引后生效。</strong>
            </span>
          </span>
        </label>
        {/* cycle 7-a：正在索引时显示 indeterminate 进度条（Codex OBJECT 3 · 不做百分比）
            + 阶段 chip + 当前目录 + 累计计数。文本行由 indexStatusLine 生成。 */}
        {indexStatus?.indexing && (
          <div className="prefs-progress-indeterminate" aria-hidden="true">
            <div className="prefs-progress-bar" />
          </div>
        )}
        <p className="prefs-status">{indexStatusLine}</p>
        {semanticLine && <p className="prefs-status">{semanticLine}</p>}
        <div style={{ display: "flex", gap: "12px", alignItems: "center" }}>
          <button
            type="button"
            className="prefs-btn primary"
            onClick={onReindex}
            disabled={reindexing}
          >
            {reindexing ? "索引中…" : "立即索引"}
          </button>
          {reindexMsg && <span className="prefs-status">{reindexMsg}</span>}
        </div>
      </div>

      {/* BETA-40：文件级提取失败留痕——哪些文件没能进索引、为什么。成功重扫 /
          文件从磁盘删除后自动从清单消失。无失败时不渲染整节（不制造焦虑）。 */}
      {extractionFailures !== null && extractionFailures.length > 0 && (
        <div className="prefs-field">
          <label className="prefs-label">未能索引的文件</label>
          <p className="prefs-hint">
            以下文件在索引时提取失败（损坏 / 加密 / 缺依赖等），搜索不到它们的内容。
            修复原因后「立即索引」会自动重试；成功或文件删除后自动从此清单消失。
          </p>
          <button
            type="button"
            className="prefs-btn small"
            onClick={() => setFailuresExpanded((v) => !v)}
          >
            {failuresExpanded ? "▾" : "▸"} 共 {extractionFailures.length} 个文件
          </button>
          {failuresExpanded && (
            <div
              style={{
                maxHeight: "220px",
                overflowY: "auto",
                marginTop: "8px",
              }}
            >
              {extractionFailures.map((f, i) => (
                <div key={i} className="prefs-root-row" title={f.path}>
                  <span className="prefs-root-path">
                    {f.path.split(/[\\/]/).pop() ?? f.path}
                    <span className="prefs-hint">
                      {" — "}
                      {f.reason}
                      {f.failed_time
                        ? `（${formatIndexTime(f.failed_time)}）`
                        : ""}
                    </span>
                  </span>
                </div>
              ))}
            </div>
          )}
        </div>
      )}

      {/* 2026-07-29：原「隐私与记录」tab 并入本面板。「索引了什么」一节未带过来——
          与上方「索引概貌」卡片是同一份数据的重复展示（且概貌卡片信息更全：分色
          分布条 + 实时索引中状态），带过来只会有两处数字打架。 */}
      <div className="prefs-section-title">隐私与数据管理</div>

      <div className="prefs-field">
        <label className="prefs-label">操作记录</label>
        <p className="prefs-hint">
          Scout 对文件执行的操作（打开 / 定位 / 复制 / 移动 / 重命名）记录在本地，便于查看与追溯。
          <strong>仅保存在本机、不上传</strong>，可随时一键清除。
        </p>
        <div
          style={{
            display: "flex",
            alignItems: "center",
            gap: "10px",
            marginBottom: "8px",
          }}
        >
          <button type="button" className="prefs-btn" onClick={onReloadAuditLog}>
            刷新
          </button>
          <button
            type="button"
            className="prefs-btn danger"
            onClick={onClearAuditLog}
            disabled={auditLog.length === 0}
          >
            清除记录
          </button>
          <span className="prefs-status">{auditLog.length} 条</span>
        </div>
        {auditLog.length === 0 ? (
          <p className="prefs-status">暂无操作记录</p>
        ) : (
          <div className="prefs-audit-wrap">
            <table className="prefs-audit-table">
              <thead>
                <tr>
                  <th>时间</th>
                  <th>操作</th>
                  <th>文件</th>
                  <th>结果</th>
                </tr>
              </thead>
              <tbody>
                {auditLog.slice(0, 200).map((e, i) => (
                  <tr key={i}>
                    <td className="ts">
                      {new Date(e.timestamp).toLocaleString()}
                    </td>
                    <td>{e.operation}</td>
                    <td className="files">
                      {e.source_paths.join(", ")}
                      {e.destination ? ` → ${e.destination}` : ""}
                      {e.new_name ? ` → ${e.new_name}` : ""}
                    </td>
                    <td className={e.result === "failed" ? "err" : "ok"}>
                      {e.result === "failed"
                        ? `失败${e.error ? `(${e.error})` : ""}`
                        : "已执行"}
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        )}
      </div>

      {/* 2026-07-29：数据存储位置——数据目录统一后（settings.json / 搜索历史 /
          同义词库都与 index.db 同目录）不再需要逐文件路径表格，一行「目录 + 总大小」
          即完整信息；原表格 5 行路径其实前缀完全相同，是纯噪音。 */}
      <div className="prefs-field">
        <label className="prefs-label">数据存储位置</label>
        <p className="prefs-hint">
          索引数据库、模型、设置、搜索历史、同义词库与操作记录，全部只保存在本机同一个目录，
          <strong>不会上传</strong>。
        </p>
        {overview && (
          <p className="prefs-status">
            <code>{overview.data_root}</code>
            {(() => {
              const totalBytes = overview.locations.reduce(
                (sum, loc) => sum + (loc.exists ? loc.size_bytes : 0),
                0,
              );
              return totalBytes > 0 ? ` · 共 ${formatBytes(totalBytes)}` : "";
            })()}
            {overview.tracing_enabled && " · 调试追踪已开启（日志仅本地）"}
          </p>
        )}
      </div>

      {/* 一键清除 */}
      <div
        className="prefs-field"
        style={{
          backgroundColor: "var(--status-err-bg)",
          padding: "16px",
          borderRadius: "8px",
          border: "1px solid rgba(239, 68, 68, 0.3)",
        }}
      >
        <label className="prefs-label status-text-err">一键清除</label>
        <p className="prefs-hint">
          可随时清除本机数据。清除后不可恢复，但本地索引可通过重新索引重建。
        </p>

        <div
          style={{
            display: "flex",
            alignItems: "center",
            gap: "12px",
            marginBottom: "12px",
            flexWrap: "wrap",
          }}
        >
          <button
            type="button"
            className="prefs-btn"
            onClick={handleClearHistory}
            disabled={
              working || !overview || overview.search_history_count === 0
            }
          >
            清除搜索历史
          </button>
          <span className="prefs-status">
            {overview ? `${overview.search_history_count} 条` : ""}
          </span>
        </div>

        <div
          style={{
            display: "flex",
            alignItems: "center",
            gap: "12px",
            flexWrap: "wrap",
          }}
        >
          {!confirmIndex ? (
            <button
              type="button"
              className="prefs-btn"
              onClick={() => setConfirmIndex(true)}
              disabled={working || indexOverview === null || grandTotal === 0}
            >
              清空本地索引
            </button>
          ) : (
            <>
              <span className="status-text-err" style={{ fontSize: "13px" }}>
                确定清空全部本地索引？
              </span>
              <button
                type="button"
                className="prefs-btn danger"
                onClick={handleClearIndex}
                disabled={working}
              >
                确认清空
              </button>
              <button
                type="button"
                className="prefs-btn"
                onClick={() => setConfirmIndex(false)}
                disabled={working}
              >
                取消
              </button>
            </>
          )}
        </div>

        {clearMsg && (
          <p
            className={
              clearMsg.includes("失败") ? "status-text-err" : "status-text-ok"
            }
            style={{ fontSize: "13px", marginTop: "12px" }}
          >
            {clearMsg}
          </p>
        )}
      </div>

      {/* BETA-12 卸载清理 */}
      <div
        className="prefs-field"
        style={{
          backgroundColor: "var(--status-err-bg)",
          padding: "16px",
          borderRadius: "8px",
          border: "1px solid rgba(239, 68, 68, 0.3)",
        }}
      >
        <label className="prefs-label status-text-err">卸载清理</label>
        <p className="prefs-hint">
          打算卸载 Scout？一键删除本机全部派生数据——索引数据库、已下载的模型、运行日志、
          操作记录、搜索历史、用户同义词库；<strong>设置文件保留</strong>（重装后配置仍在）。
          Windows 安装版直接运行系统卸载程序即可，卸载时会自动完成同等清理（版本升级不受影响）。
        </p>
        <div
          style={{
            display: "flex",
            alignItems: "center",
            gap: "12px",
            flexWrap: "wrap",
          }}
        >
          {!confirmCleanup ? (
            <button
              type="button"
              className="prefs-btn"
              onClick={() => setConfirmCleanup(true)}
              disabled={working}
            >
              清理全部数据（保留设置）
            </button>
          ) : (
            <>
              <span className="status-text-err" style={{ fontSize: "13px" }}>
                确定删除索引、模型、日志等全部数据？此操作不可恢复。
              </span>
              <button
                type="button"
                className="prefs-btn danger"
                onClick={handleUninstallCleanup}
                disabled={working}
              >
                确认清理
              </button>
              <button
                type="button"
                className="prefs-btn"
                onClick={() => setConfirmCleanup(false)}
                disabled={working}
              >
                取消
              </button>
            </>
          )}
        </div>
        {cleanupMsg && (
          <p
            className={
              cleanupMsg.includes("失败") || cleanupMsg.includes("未能")
                ? "status-text-err"
                : "status-text-ok"
            }
            style={{ fontSize: "13px", marginTop: "12px" }}
          >
            {cleanupMsg}
          </p>
        )}
        {cleanupReport && (
          <ul
            style={{
              fontSize: "12px",
              color: "var(--muted)",
              marginTop: "8px",
              paddingLeft: "18px",
            }}
          >
            {cleanupReport.items.map((item, i) => (
              <li
                key={i}
                className={item.removed ? undefined : "status-text-err"}
              >
                {item.label}：
                {item.removed
                  ? item.existed
                    ? "已删除"
                    : "本来就不存在"
                  : `删除失败（${item.detail ?? "未知原因"}）`}
              </li>
            ))}
          </ul>
        )}
      </div>
    </div>
  );
}
