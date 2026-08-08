import { useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { AppSettings } from "../../hooks/useAppSettings";
import { EmbedStatus, ModelStatusJson, embedStatusLine } from "../../lib/model-status";
import { ModelDownloadStep } from "../ModelDownloadStep";

/** 与后端 search.rs::ModelProbe 对应（「检测」按钮返回）。 */
interface ModelProbe {
  path: string;
  exists: boolean;
  size_bytes: number;
  is_gguf: boolean;
  usable: boolean;
  message: string;
}

/** 与后端 model_download.rs::GgufCandidate 对应（「自动发现」列表项）。 */
interface GgufCandidate {
  path: string;
  name: string;
  size_bytes: number;
}

/** 与后端 model_download.rs::ScanBackend 对应（本次「扫描本机 gguf」实际用的后端）。 */
type ScanBackend = "everything" | "windows_search" | "spotlight" | "none";

/** 与后端 model_download.rs::DiscoverGgufResult 对应。 */
interface DiscoverGgufResult {
  backend: ScanBackend;
  candidates: GgufCandidate[];
}

const SCAN_BACKEND_LABEL: Record<ScanBackend, string> = {
  everything: "Everything",
  windows_search: "Windows 搜索",
  spotlight: "Spotlight",
  none: "",
};

/** 字节数转人类可读（MB / GB）。 */
function formatSize(n: number): string {
  if (n >= 1024 * 1024 * 1024) return `${(n / (1024 * 1024 * 1024)).toFixed(1)} GB`;
  return `${(n / (1024 * 1024)).toFixed(0)} MB`;
}

/**
 * 「语义召回」面板：语义参数（相似度下限 / 融合权重）+ 嵌入模型状态与下载
 * + **模型管理**小节（BETA-47 从「常规」迁入：生成模型 fallback 开关 / 状态 /
 * 下载 / 路径覆盖——模型下载与管理归位，一处管所有模型）。
 */
export function SemanticPane({
  settings,
  setSettings,
  embedStatus,
  onEmbedReload,
  modelStatus,
}: {
  settings: AppSettings;
  setSettings: (s: AppSettings) => void;
  embedStatus: EmbedStatus | null;
  onEmbedReload: () => void;
  modelStatus: ModelStatusJson | null;
}) {
  // 「检测」按钮结果（语义 / 生成 各一份）；null = 尚未检测。
  const [embedProbe, setEmbedProbe] = useState<ModelProbe | null>(null);
  const [genProbe, setGenProbe] = useState<ModelProbe | null>(null);
  // 「自动发现」状态。
  const [discovering, setDiscovering] = useState(false);
  const [discovered, setDiscovered] = useState<DiscoverGgufResult | null>(null);

  const runProbe = async (
    path: string | null,
    kind: "embedding" | "generation",
    set: (p: ModelProbe | null) => void,
  ) => {
    try {
      set(await invoke<ModelProbe>("probe_model_file", { path: path ?? "", kind }));
    } catch (err) {
      set({
        path: path ?? "",
        exists: false,
        size_bytes: 0,
        is_gguf: false,
        usable: false,
        message: String(err),
      });
    }
  };

  const runDiscover = async () => {
    setDiscovering(true);
    try {
      setDiscovered(await invoke<DiscoverGgufResult>("discover_gguf_models"));
    } catch (err) {
      setDiscovered({ backend: "none", candidates: [] });
      console.error("discover_gguf_models:", err);
    } finally {
      setDiscovering(false);
    }
  };

  // 选用某发现候选为语义 / 生成模型：回填路径覆盖并立即检测反馈。
  const useAsEmbedding = (path: string) => {
    setSettings({ ...settings, embedding_model_path: path });
    void runProbe(path, "embedding", setEmbedProbe);
  };
  const useAsGeneration = (path: string) => {
    setSettings({ ...settings, model_path: path });
    void runProbe(path, "generation", setGenProbe);
  };

  // 手动浏览本地文件系统选一个 gguf 模型文件，回填路径覆盖并立即检测反馈——
  // 不依赖自动发现（后者靠 Everything/Windows 搜索/Spotlight 三选一，用户本机
  // 可能三者都不可用，或模型放在这些后端扫不到的位置）。
  const browseFor = async (
    kind: "embedding" | "generation",
    apply: (path: string) => void,
  ) => {
    try {
      const { open } = await import("@tauri-apps/plugin-dialog");
      const picked = await open({
        multiple: false,
        filters: [{ name: "GGUF 模型", extensions: ["gguf"] }],
      });
      if (typeof picked === "string") {
        apply(picked);
      }
    } catch (err) {
      console.error(`[SemanticPane] browse dialog (${kind}) failed:`, err);
    }
  };

  return (
    <div className="prefs-form">
      <div className="prefs-field">
        <label className="prefs-label">语义召回（按意思找到）</label>
        {embedStatus ? (
          (() => {
            const { text, color } = embedStatusLine(embedStatus);
            return (
              <p style={{ fontSize: "13px", color, margin: 0 }}>{text}</p>
            );
          })()
        ) : (
          <p className="prefs-status">语义召回：状态获取中…</p>
        )}
        <p className="prefs-hint">
          语义召回让「找放假相关的通知」等按含义命中文件，支持跨语言模糊匹配。
        </p>
        {embedStatus?.state === "not_found" && (
          <ModelDownloadStep compact onComplete={onEmbedReload} />
        )}
      </div>

      <div className="prefs-field">
        <label className="prefs-label">
          语义相似度下限（0–1，越高越严，默认 0.30）
        </label>
        <input
          type="number"
          min={0}
          max={1}
          step={0.05}
          className="prefs-input prefs-input-small"
          value={settings.semantic_similarity_floor ?? 0.3}
          onChange={(e) =>
            setSettings({
              ...settings,
              semantic_similarity_floor:
                e.target.value === "" ? null : parseFloat(e.target.value),
            })
          }
        />
        <p className="prefs-hint">
          语义结果低于此 cosine 分数将被过滤；改后重新搜索即生效。
        </p>
      </div>

      <div className="prefs-field">
        <label className="prefs-label">
          语义臂权重（融合 FTS vs 向量，越高越偏向量，默认 10.0）
        </label>
        <input
          type="number"
          min={0.5}
          max={50}
          step={0.5}
          className="prefs-input prefs-input-small"
          value={settings.semantic_weight ?? 10.0}
          onChange={(e) =>
            setSettings({
              ...settings,
              semantic_weight:
                e.target.value === "" ? null : parseFloat(e.target.value),
            })
          }
        />
        <p className="prefs-hint">
          融合时语义臂（按意思）相对 FTS 臂（关键词）的权重。0.5–50；改后重新搜索即生效。
        </p>
      </div>

      {/* BETA-47：模型管理小节（原「常规」面板的生成模型部分整体迁入）。 */}
      <div className="prefs-field">
        <label className="prefs-label">模型管理 — 生成模型（Qwen3-0.6B，可选）</label>
        <label className="prefs-checkbox">
          <input
            type="checkbox"
            checked={settings.enable_model_fallback}
            onChange={(e) =>
              setSettings({
                ...settings,
                enable_model_fallback: e.target.checked,
              })
            }
          />
          启用模型 Fallback（可选，仅在解析复杂查询时使用）
        </label>
        <p className="prefs-hint">
          仅在 parser 解不出复杂多条件自然语言（如「上周从张三收到的关于 Q3 报表的 PDF」）时才会触发；
          日常关键词与语义召回不需要它。装了 parser 覆盖率从 88% → ~95%+。
        </p>
        {modelStatus && (
          <p
            className={`prefs-status${modelStatus.state === "ready" ? " status-text-ok" : ""}`}
          >
            状态：{modelStatus.detail}
          </p>
        )}
        {/* BETA-33 cycle 3 v4：not_found 时嵌入一键下载（与 embedding 同款） */}
        {modelStatus?.state === "not_found" && (
          <ModelDownloadStep
            compact
            kind="generation"
            onComplete={() => {
              // 触发一次 get_model_status 重读、让状态从 not_found 变 ready。
              // 3s 轮询也会自动更新，但立即触发一次响应更快。
              void invoke<ModelStatusJson>("get_model_status");
            }}
          />
        )}
      </div>

      {/* 2026-07-07：自动发现本机 gguf 模型——为切换更强的本地/局域网可信模型服务。
          发现结果只回填下方路径覆盖、不复制不加载（错架构模型误载可能 crash，故交用户判断）。 */}
      <div className="prefs-field">
        <label className="prefs-label">自动发现本机模型</label>
        <p className="prefs-hint">
          扫描本机已有的 gguf 模型文件，选用后回填到下方「语义 / 生成模型路径覆盖」——
          用于切换到更强的本地模型或局域网可信模型。请自行确认所选模型与用途匹配
          （embedding 模型用于语义、生成模型用于复杂查询解析）。按可用性依次尝试
          Everything（Windows，全盘扫描，需装）→ Windows 搜索（Windows 自带，仅
          覆盖系统索引范围）→ Spotlight（macOS 自带，默认全盘扫描）。
        </p>
        <button
          type="button"
          className="prefs-btn"
          onClick={runDiscover}
          disabled={discovering}
        >
          {discovering ? "扫描中…" : "扫描本机 gguf 模型"}
        </button>
        {discovered && discovered.backend === "none" && (
          <p className="prefs-hint status-text-warn">
            当前不可用：Windows 上未装/未开 Everything 且拿不到系统搜索服务；
            非 Windows/macOS 平台无自动发现能力。请在下方手动浏览选择模型文件。
          </p>
        )}
        {discovered && discovered.backend === "windows_search" && (
          <p className="prefs-hint status-text-warn">
            经 Windows 搜索扫描——只覆盖系统「索引选项」纳入的目录，不在索引范围内的
            文件（如另一块盘、临时下载目录）扫不到；装 Everything 可获得全盘覆盖。
          </p>
        )}
        {discovered && discovered.backend !== "none" && discovered.candidates.length === 0 && (
          <p className="prefs-status">
            经 {SCAN_BACKEND_LABEL[discovered.backend]} 未发现本机 gguf 模型文件。
          </p>
        )}
        {discovered && discovered.backend !== "none" && discovered.candidates.length > 0 && (
          <p className="prefs-status">
            经 {SCAN_BACKEND_LABEL[discovered.backend]} 发现 {discovered.candidates.length} 个候选：
          </p>
        )}
        {discovered && discovered.candidates.length > 0 && (
          <div
            style={{
              border: "1px solid var(--border)",
              borderRadius: "8px",
              overflow: "hidden",
              marginTop: "8px",
            }}
          >
            <table
              style={{
                width: "100%",
                fontSize: "12px",
                borderCollapse: "collapse",
              }}
            >
              <tbody>
                {discovered.candidates.map((c, i) => (
                  <tr
                    key={c.path}
                    style={{
                      borderTop: i > 0 ? "1px solid var(--border)" : undefined,
                    }}
                  >
                    <td style={{ padding: "8px 12px" }}>
                      <div style={{ fontWeight: 500 }}>{c.name}</div>
                      <div
                        style={{
                          color: "var(--subtle)",
                          fontFamily: "monospace",
                          wordBreak: "break-all",
                        }}
                        title={c.path}
                      >
                        {c.path} · {formatSize(c.size_bytes)}
                      </div>
                    </td>
                    <td
                      style={{
                        padding: "8px 12px",
                        whiteSpace: "nowrap",
                        textAlign: "right",
                      }}
                    >
                      <span style={{ display: "inline-flex", gap: "4px" }}>
                        <button
                          type="button"
                          className="prefs-btn"
                          onClick={() => useAsEmbedding(c.path)}
                        >
                          设为语义
                        </button>
                        <button
                          type="button"
                          className="prefs-btn"
                          onClick={() => useAsGeneration(c.path)}
                        >
                          设为生成
                        </button>
                      </span>
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        )}
      </div>

      {/* BETA-48：embedding 路径覆盖 UI（与下方生成模型对称）。此前该字段只能手工改
          settings.json 且会被 UI 保存冲掉；接口透传修复 + 一并暴露。 */}
      <div className="prefs-field">
        <label className="prefs-label">语义模型路径覆盖（留空用默认）</label>
        <div style={{ display: "flex", gap: "8px", alignItems: "flex-start" }}>
          <input
            type="text"
            className="prefs-input"
            style={{ flex: 1, minWidth: 0 }}
            value={settings.embedding_model_path ?? ""}
            onChange={(e) => {
              setEmbedProbe(null);
              setSettings({
                ...settings,
                embedding_model_path: e.target.value || null,
              });
            }}
            placeholder="默认：数据目录/models/embeddinggemma-300m-q8_0.gguf"
          />
          <button
            type="button"
            className="prefs-btn"
            onClick={() =>
              void browseFor("embedding", (path) => {
                setEmbedProbe(null);
                setSettings({ ...settings, embedding_model_path: path });
                void runProbe(path, "embedding", setEmbedProbe);
              })
            }
            title="在文件系统中选择一个 gguf 模型文件"
          >
            浏览…
          </button>
          <button
            type="button"
            className="prefs-btn"
            onClick={() =>
              runProbe(settings.embedding_model_path, "embedding", setEmbedProbe)
            }
            title="检测该路径的模型文件是否可用（不加载模型）；留空则检测当前生效的默认模型"
          >
            检测
          </button>
        </div>
        {embedProbe && (
          <p
            className={
              embedProbe.usable ? "status-text-ok" : "status-text-err"
            }
            style={{ fontSize: "13px", margin: "6px 0 0" }}
          >
            {embedProbe.usable ? "✓ " : "✗ "}
            {embedProbe.message}
          </p>
        )}
        <p className="prefs-hint">
          可指向自定义或局域网可信模型的 gguf 文件路径（为切换更强的本地/内网模型预留）；
          先「检测」确认可用、再「应用」，修改后建议重启应用生效。留空时用默认路径。
        </p>
      </div>

      <div className="prefs-field">
        <label className="prefs-label">生成模型路径覆盖（留空用默认）</label>
        <div style={{ display: "flex", gap: "8px", alignItems: "flex-start" }}>
          <input
            type="text"
            className="prefs-input"
            style={{ flex: 1, minWidth: 0 }}
            value={settings.model_path ?? ""}
            onChange={(e) => {
              setGenProbe(null);
              setSettings({ ...settings, model_path: e.target.value || null });
            }}
            placeholder="默认：数据目录/models/qwen3-0.6b-q4_k_m.gguf"
          />
          <button
            type="button"
            className="prefs-btn"
            onClick={() =>
              void browseFor("generation", (path) => {
                setGenProbe(null);
                setSettings({ ...settings, model_path: path });
                void runProbe(path, "generation", setGenProbe);
              })
            }
            title="在文件系统中选择一个 gguf 模型文件"
          >
            浏览…
          </button>
          <button
            type="button"
            className="prefs-btn"
            onClick={() => runProbe(settings.model_path, "generation", setGenProbe)}
            title="检测该路径的模型文件是否可用（不加载模型）；留空则检测当前生效的默认模型"
          >
            检测
          </button>
        </div>
        {genProbe && (
          <p
            className={genProbe.usable ? "status-text-ok" : "status-text-err"}
            style={{ fontSize: "13px", margin: "6px 0 0" }}
          >
            {genProbe.usable ? "✓ " : "✗ "}
            {genProbe.message}
          </p>
        )}
        <p className="prefs-hint">
          可指向自定义或局域网可信模型的 gguf 文件路径；先「检测」再「应用」，
          修改后需重启应用生效。留空时用默认路径。
        </p>
      </div>
    </div>
  );
}
