import { useEffect, useMemo, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import {
  ArrowLeft,
  ArrowsClockwise,
  Brain,
  Database,
  GearSix,
  PlugsConnected,
} from "@phosphor-icons/react";
// BETA-33 cycle 9：EmbedStatus / ModelStatusJson 类型 + embedStatusLine 文案改从
// 单一信源引入（原与 SettingsPage / StatusIndicator 三处复制，详见 lib/model-status.ts）。
import { EmbedStatus, ModelStatusJson } from "../lib/model-status";
// BETA-33 cycle 9：AppSettings 类型 + 加载/保存/未保存判定流收拢到 useAppSettings
// （原与 SettingsPage 复制 ~120 行；旧 /settings 路由 + SettingsPage 已随本 cycle 删除）。
import { useAppSettings } from "../hooks/useAppSettings";
// BETA-47：分类面板 / 确认弹窗 / 共享类型迁至 preferences/；
// 2026-07-24 起本文件承载整页设置壳（状态流 + 分类导航 + 底部按钮 + 离开守卫）。
import { ConfirmModal, ConfirmRequest } from "./preferences/ConfirmModal";
import { GeneralPane } from "./preferences/GeneralPane";
import { IndexingPane } from "./preferences/IndexingPane";
import { McpPane } from "./preferences/McpPane";
import { MiscPane } from "./preferences/MiscPane";
import { SemanticPane } from "./preferences/SemanticPane";
import {
  AuditEntry,
  CATEGORIES,
  Category,
  ExtractionFailure,
  IndexStatus,
  ReindexStats,
  RootIndexOverview,
  phaseChipLabel,
  reindexDoneMsg,
} from "./preferences/shared";

// 左侧分类导航 + 右侧表单 + 底部取消 / 应用 / 确定。
// 组件名因历史兼容保留 PreferencesDialog，实际 UI 已是 /settings 整页工作区。
// 2026-07-29：分类精简为 常规 / 索引 / 语义召回 / 术语与同义词 / 本机 MCP 服务 五 tab
// ——Everything / Windows 系统集成下沉进「常规」子分区（见 GeneralPane），
// 「隐私与记录」并入「索引」（见 IndexingPane）。

interface Props {
  onClose: () => void;
  /** 打开时默认选中的分类。快速入门第 5 步用（跳直接到「索引」）。缺省为 "general"。 */
  initialCategory?: Category;
  /** 外层导航请求离开设置页时递增；组件在这里统一执行未保存改动守卫。 */
  closeRequestToken?: number;
}

export default function PreferencesDialog({
  onClose,
  initialCategory,
  closeRequestToken = 0,
}: Props) {
  const [active, setActive] = useState<Category>(initialCategory ?? "general");
  // cycle 9：settings 加载/编辑/保存流（含 initialSettings 快照 + 未保存判定）收拢进 hook。
  const {
    settings,
    setSettings,
    initialSettings,
    hasUnsavedChanges,
    save,
    saving,
    message,
    setMessage,
  } = useAppSettings();
  const [reindexing, setReindexing] = useState(false);
  const [reindexMsg, setReindexMsg] = useState("");
  const [auditLog, setAuditLog] = useState<AuditEntry[]>([]);
  const [indexStatus, setIndexStatus] = useState<IndexStatus | null>(null);
  const [modelStatus, setModelStatus] = useState<ModelStatusJson | null>(null);
  const [embedStatus, setEmbedStatus] = useState<EmbedStatus | null>(null);
  const [effectiveRoots, setEffectiveRoots] = useState<string[] | null>(null);
  // BETA-33 cycle 5：每 root 的分类统计（doc / image / music + 上次索引）。
  const [indexOverview, setIndexOverview] = useState<RootIndexOverview[] | null>(
    null,
  );
  // BETA-40：文件级提取失败留痕（「未能索引的文件」清单）。
  const [extractionFailures, setExtractionFailures] = useState<
    ExtractionFailure[] | null
  >(null);
  // cycle 7-a：picker 后 flash 高亮的路径（1.5s 后清空、CSS animation 触发）。
  const [flashPath, setFlashPath] = useState<string | null>(null);
  const flashTimerRef = useRef<number | null>(null);
  // cycle 7-c：应用内二次确认弹窗（关闭守卫 + 移除目录共用；window.confirm 在
  // wry/WebView2 生产环境是 no-op、不可用）。
  const [confirmReq, setConfirmReq] = useState<ConfirmRequest | null>(null);

  // 初始加载（settings 的加载由 useAppSettings 承担）。
  useEffect(() => {
    invoke<AuditEntry[]>("get_audit_log").then(setAuditLog).catch(console.error);
    invoke<IndexStatus>("get_index_status")
      .then(setIndexStatus)
      .catch(console.error);
    invoke<ExtractionFailure[]>("get_extraction_failures")
      .then(setExtractionFailures)
      .catch(console.error);
  }, []);

  // 索引状态轮询（2s）
  useEffect(() => {
    const t = setInterval(() => {
      invoke<IndexStatus>("get_index_status")
        .then(setIndexStatus)
        .catch(() => {});
    }, 2000);
    return () => clearInterval(t);
  }, []);

  // 模型状态轮询（3s）
  useEffect(() => {
    let alive = true;
    const poll = () =>
      invoke<ModelStatusJson>("get_model_status")
        .then((s) => {
          if (alive) setModelStatus(s);
        })
        .catch(() => {});
    poll();
    const t = setInterval(poll, 3000);
    return () => {
      alive = false;
      clearInterval(t);
    };
  }, []);

  // embedding 状态轮询（3s）
  useEffect(() => {
    let alive = true;
    const poll = () =>
      invoke<EmbedStatus>("embedding_model_status")
        .then((s) => {
          if (alive) setEmbedStatus(s);
        })
        .catch(() => {});
    poll();
    const t = setInterval(poll, 3000);
    return () => {
      alive = false;
      clearInterval(t);
    };
  }, []);

  // 跟随 settings.index_roots / include_system_defaults 重 fetch effectiveRoots + indexOverview
  useEffect(() => {
    if (!settings) return;
    invoke<string[]>("get_effective_index_roots", {
      indexRoots: settings.index_roots,
      includeSystemDefaults: settings.include_system_defaults,
    })
      .then(setEffectiveRoots)
      .catch(console.error);
    // cycle 5：拉每 root 的分类统计。reindex 完成后（indexStatus.indexing false）
    // 触发也刷一次——见下 useEffect。
    invoke<RootIndexOverview[]>("get_index_overview", {
      indexRoots: settings.index_roots,
      includeSystemDefaults: settings.include_system_defaults,
    })
      .then(setIndexOverview)
      .catch(console.error);
  }, [settings?.index_roots, settings?.include_system_defaults]);

  // BETA-33 cycle 5：reindex 从 indexing=true 转 false 时（一次全量刚跑完）重 fetch overview。
  // cycle 7-a：Codex §10 数据源统一——强刷 indexStatus 让顶部概貌"上次索引"文案同步更新，
  // 避免出现"reindex 完成后 UI 仍显示 N 分钟前"的口径不一致。
  const prevIndexing = useRef(false);
  useEffect(() => {
    const now = indexStatus?.indexing ?? false;
    if (prevIndexing.current && !now && settings) {
      invoke<RootIndexOverview[]>("get_index_overview", {
        indexRoots: settings.index_roots,
        includeSystemDefaults: settings.include_system_defaults,
      })
        .then(setIndexOverview)
        .catch(console.error);
      // 强刷 indexStatus：last_indexed 应该刚被后端 apply_reindex_result 填、拉过来给 UI
      invoke<IndexStatus>("get_index_status")
        .then(setIndexStatus)
        .catch(console.error);
      // BETA-40：失败留痕随本轮 reindex 增删（成功重扫 / 磁盘删除自动清除），同步刷新。
      invoke<ExtractionFailure[]>("get_extraction_failures")
        .then(setExtractionFailures)
        .catch(console.error);
    }
    prevIndexing.current = now;
  }, [
    indexStatus?.indexing,
    settings?.index_roots,
    settings?.include_system_defaults,
  ]);

  // cycle 7-a：clean up flash timer on unmount
  useEffect(() => {
    return () => {
      if (flashTimerRef.current !== null) {
        window.clearTimeout(flashTimerRef.current);
      }
    };
  }, []);

  // cycle 7-a：包装 onClose——未保存改动时弹二次确认（hasUnsavedChanges 由 hook 提供）。
  // cycle 7-c：改用 in-DOM ConfirmModal——window.confirm 在 wry/WebView2 生产环境
  // 不弹窗直接放行（v0.9.7 真机验证发现守卫完全失效），不可用。
  const handleCloseWithGuard = () => {
    if (hasUnsavedChanges) {
      setConfirmReq({
        title: "放弃未保存的改动？",
        message: "你有未保存的改动，离开设置页将放弃这些改动。",
        confirmLabel: "放弃改动并关闭",
        danger: true,
        onConfirm: () => {
          setConfirmReq(null);
          onClose();
        },
      });
      return;
    }
    onClose();
  };

  const previousCloseRequest = useRef(closeRequestToken);
  useEffect(() => {
    if (previousCloseRequest.current === closeRequestToken) return;
    previousCloseRequest.current = closeRequestToken;
    handleCloseWithGuard();
  }, [closeRequestToken]);

  // Esc 离开：整页根节点自动获焦，继续复用 WebView2 下可靠的 React 合成事件路径。
  const settingsPageRef = useRef<HTMLElement>(null);
  useEffect(() => {
    settingsPageRef.current?.focus();
  }, []);

  // cycle 9：保存核心流在 hook（快照重置 + message 3s 自清）；此处只叠加本组件
  // 特有的收尾（清 picker flash 高亮）。
  const handleApply = async (): Promise<boolean> => {
    const ok = await save();
    if (ok) setFlashPath(null);
    return ok;
  };

  const handleOk = async () => {
    const ok = await handleApply();
    if (ok) onClose();
  };

  const handleReindex = async () => {
    setReindexing(true);
    setReindexMsg("正在索引音频、文档与图片目录…");
    try {
      const s = await invoke<ReindexStats>("reindex");
      setReindexMsg(reindexDoneMsg(s));
    } catch (err) {
      setReindexMsg(`索引失败: ${err}`);
    } finally {
      setReindexing(false);
    }
  };

  // cycle 7-c：单目录重扫（RootRow「重扫」按钮）。exclude / root_excludes 仍从已保存
  // settings 生效（后端 perform_reindex_for_roots 只 override roots）。
  const handleReindexRoot = async (root: string) => {
    setReindexing(true);
    setReindexMsg(`正在重扫 ${root} …`);
    try {
      const s = await invoke<ReindexStats>("reindex_root", { root });
      setReindexMsg(reindexDoneMsg(s));
    } catch (err) {
      setReindexMsg(`重扫失败: ${err}`);
    } finally {
      setReindexing(false);
    }
  };

  // cycle 7-c：在系统文件管理器中打开目录。复用既有 open_path 命令
  // （FileActionTool 策略 + audit 口径一致，Windows 走 cmd start / macOS 走 open）。
  const handleOpenRoot = async (path: string) => {
    try {
      await invoke("open_path", { path });
    } catch (err) {
      setMessage(`打开目录失败: ${err}`);
      setTimeout(() => setMessage(""), 5000);
    }
  };

  // cycle 7-c：从未保存 settings 中移除 root（同步清该 root 的 root_excludes、不留孤儿）。
  const removeRootFromSettings = (rootPath: string) => {
    if (!settings) return;
    setSettings({
      ...settings,
      index_roots: settings.index_roots.filter((p) => p !== rootPath),
      root_excludes: settings.root_excludes.filter(
        (re) => re.root !== rootPath,
      ),
    });
  };

  // cycle 7-c（Codex SUGGEST 8）：移除目录二次确认——区分「仅移除配置」与「移除并清除
  // 索引记录」，文案明确**不删除磁盘文件**。清除走 purge_root_from_db（存储层 SQL、
  // FTS 同步删、向量外键级联）；清除失败时不移除配置，避免"以为清了其实没清"。
  const requestRemoveRoot = (path: string) => {
    setConfirmReq({
      title: "移除索引目录",
      message: `${path}\n以下操作只影响 Scout 的数据库缓存，不会删除磁盘上的任何原文件。`,
      confirmLabel: "确定",
      options: [
        {
          key: "keep",
          label: "仅从索引配置移除",
          hint: "保留数据库缓存，重新添加可复用旧记录",
        },
        {
          key: "purge",
          label: "移除并清除索引记录",
          hint: "同时删除该目录下的文档 / 图片 / 音频索引缓存（不删原文件）",
        },
      ],
      defaultOption: "keep",
      onConfirm: (opt) => {
        setConfirmReq(null);
        void (async () => {
          if (opt === "purge") {
            try {
              const s = await invoke<{
                doc_deleted: number;
                music_deleted: number;
              }>("purge_root_from_db", { root: path });
              setMessage(
                `已清除索引记录：文档/图片 ${s.doc_deleted} 条 · 音频 ${s.music_deleted} 条（磁盘文件未动）`,
              );
              setTimeout(() => setMessage(""), 5000);
            } catch (err) {
              setMessage(`清除索引记录失败: ${err}`);
              setTimeout(() => setMessage(""), 5000);
              return;
            }
          }
          removeRootFromSettings(path);
        })();
      },
    });
  };

  const handleClearAuditLog = async () => {
    try {
      await invoke("clear_audit_log");
      setAuditLog([]);
    } catch (err) {
      console.error(err);
    }
  };

  const indexStatusLine = useMemo(() => {
    if (!indexStatus) return "状态加载中…";
    if (indexStatus.indexing) {
      // cycle 7-a：先显示 phase chip（帮 Everything 全盘发现"卡在 0·0"的场景解释状态）、
      // 再显示当前目录（文件父目录、非配置 root）+ 累计进度。
      const parts: string[] = [];
      if (indexStatus.current_phase) {
        parts.push(phaseChipLabel(indexStatus.current_phase));
      }
      if (indexStatus.current_root) {
        parts.push(`📁 当前目录：${indexStatus.current_root}`);
      }
      if (indexStatus.fts_progress) {
        const [scanned, indexed] = indexStatus.fts_progress;
        parts.push(
          `已扫描 ${scanned.toLocaleString()} · 已入库 ${indexed.toLocaleString()}`,
        );
      }
      const detail = parts.length > 0 ? parts.join("　") : "扫描准备中…";
      return `⏳ 正在索引：${detail}`;
    }
    if (indexStatus.last_indexed) {
      return `上次索引: ${new Date(indexStatus.last_indexed).toLocaleString()}${indexStatus.last_summary ? `（${indexStatus.last_summary}）` : ""}`;
    }
    if (indexStatus.last_summary) {
      return `当前索引: ${indexStatus.last_summary}`;
    }
    return "尚未索引";
  }, [indexStatus]);

  return (
    <section
      className="settings-page"
      onKeyDownCapture={(e) => {
        if (e.key === "Escape" || e.key === "Esc") {
          e.preventDefault();
          e.stopPropagation();
          // cycle 7-c：确认弹窗打开时 Esc 只关弹窗（= 取消），不触发外层关闭守卫。
          if (confirmReq) {
            setConfirmReq(null);
            return;
          }
          handleCloseWithGuard();
        }
      }}
      ref={settingsPageRef}
      tabIndex={-1}
      aria-labelledby="prefs-title"
    >
        <header className="settings-header">
          <button
            type="button"
            className="settings-back"
            onClick={handleCloseWithGuard}
            aria-label="返回找文档"
          >
            <ArrowLeft size={20} />
          </button>
          <div>
            <span className="page-eyebrow">PREFERENCES</span>
            <h1 id="prefs-title">设置</h1>
            <p>管理 Scout 在本机如何搜索、索引与保护你的数据。</p>
          </div>
        </header>

        <div className="prefs-body">
          <nav className="prefs-categories" aria-label="分类">
            <ul>
              {CATEGORIES.map((c) => (
                <li
                  key={c.key}
                  className={`prefs-category${active === c.key ? " active" : ""}`}
                  onClick={() => setActive(c.key)}
                  role="tab"
                  aria-selected={active === c.key}
                >
                  <CategoryIcon category={c.key} />
                  <span>{c.label}</span>
                </li>
              ))}
            </ul>
          </nav>

          <div className="prefs-pane" role="tabpanel">
            {!settings ? (
              <p style={{ color: "var(--subtle)" }}>加载中…</p>
            ) : active === "general" ? (
              <GeneralPane settings={settings} setSettings={setSettings} />
            ) : active === "semantic" ? (
              <SemanticPane
                settings={settings}
                setSettings={setSettings}
                embedStatus={embedStatus}
                onEmbedReload={() =>
                  invoke<EmbedStatus>("embedding_model_status")
                    .then(setEmbedStatus)
                    .catch(console.error)
                }
                modelStatus={modelStatus}
              />
            ) : active === "misc" ? (
              <MiscPane />
            ) : active === "mcp" ? (
              <McpPane />
            ) : (
              <IndexingPane
                settings={settings}
                setSettings={setSettings}
                initialIndexRoots={initialSettings?.index_roots ?? []}
                effectiveRoots={effectiveRoots}
                indexOverview={indexOverview}
                indexStatus={indexStatus}
                indexStatusLine={indexStatusLine}
                extractionFailures={extractionFailures}
                semanticLine={
                  indexStatus &&
                  (indexStatus.semantic_indexing ||
                    indexStatus.semantic_summary)
                    ? indexStatus.semantic_indexing
                      ? `🧠 语义索引中${indexStatus.semantic_progress ? ` ${indexStatus.semantic_progress[0]}/${indexStatus.semantic_progress[1]}` : "…"}`
                      : `🧠 ${indexStatus.semantic_summary ?? ""}`
                    : null
                }
                reindexing={reindexing}
                reindexMsg={reindexMsg}
                onReindex={handleReindex}
                onReindexRoot={handleReindexRoot}
                onOpenRoot={handleOpenRoot}
                onRequestRemoveRoot={requestRemoveRoot}
                onPickMessage={(m) => {
                  setMessage(m);
                  setTimeout(() => setMessage(""), 5000);
                }}
                flashPath={flashPath}
                onFlash={(path) => {
                  if (flashTimerRef.current !== null) {
                    window.clearTimeout(flashTimerRef.current);
                  }
                  setFlashPath(path);
                  flashTimerRef.current = window.setTimeout(() => {
                    setFlashPath(null);
                    flashTimerRef.current = null;
                  }, 1600);
                }}
                auditLog={auditLog}
                onReloadAuditLog={() =>
                  invoke<AuditEntry[]>("get_audit_log")
                    .then(setAuditLog)
                    .catch(console.error)
                }
                onClearAuditLog={handleClearAuditLog}
              />
            )}
          </div>
        </div>

        <div className="prefs-footer">
          <span
            className={`prefs-msg${message.includes("失败") ? " err" : message ? " ok" : hasUnsavedChanges ? " warn" : ""}`}
          >
            {message
              ? message
              : hasUnsavedChanges
                ? "⚠ 你有未保存的改动，点「应用」或「确定」生效"
                : ""}
          </span>
          <div className="prefs-actions">
            <button
              type="button"
              className="prefs-btn"
              onClick={handleCloseWithGuard}
              disabled={saving}
            >
              取消
            </button>
            <button
              type="button"
              className="prefs-btn"
              onClick={handleApply}
              disabled={saving || !settings || !hasUnsavedChanges}
              title={hasUnsavedChanges ? "" : "没有未保存改动"}
            >
              {saving ? "保存中…" : "应用"}
            </button>
            <button
              type="button"
              className="prefs-btn primary"
              onClick={handleOk}
              disabled={saving || !settings}
            >
              确定
            </button>
          </div>
        </div>

        {/* 应用内二次确认弹窗（关闭守卫 / 移除目录）。 */}
        {confirmReq && (
          <ConfirmModal req={confirmReq} onCancel={() => setConfirmReq(null)} />
        )}
    </section>
  );
}

function CategoryIcon({ category }: { category: Category }) {
  const props = { size: 18, weight: "duotone" as const };
  switch (category) {
    case "general":
      return <GearSix {...props} />;
    case "indexing":
      return <Database {...props} />;
    case "semantic":
      return <Brain {...props} />;
    case "misc":
      return <ArrowsClockwise {...props} />;
    case "mcp":
      return <PlugsConnected {...props} />;
  }
}
