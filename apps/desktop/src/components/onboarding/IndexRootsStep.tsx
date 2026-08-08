// 快速入门共用步骤：展示当前会被索引的目录（生效 index_roots），
// 提供跳转到「选项 → 索引」的入口。不在这里内嵌完整的目录管理 UI —
// 复用 PreferencesDialog 的索引分类，避免两份实现漂移。
import React, { useCallback, useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { emitMenuAction } from "../../lib/menu-events";

interface AppSettingsLite {
  index_roots: string[];
  include_system_defaults: boolean;
}

export const IndexRootsStep: React.FC = () => {
  const [settings, setSettings] = useState<AppSettingsLite | null>(null);
  const [effectiveRoots, setEffectiveRoots] = useState<string[] | null>(null);
  const [error, setError] = useState<string | null>(null);

  const load = useCallback(async () => {
    try {
      const s = await invoke<AppSettingsLite>("get_settings");
      setSettings({
        index_roots: s.index_roots ?? [],
        include_system_defaults: s.include_system_defaults ?? false,
      });
      const roots = await invoke<string[]>("get_effective_index_roots", {
        indexRoots: s.index_roots,
        includeSystemDefaults: s.include_system_defaults,
      });
      setEffectiveRoots(roots);
      setError(null);
    } catch (err) {
      console.error("[IndexRootsStep] load failed:", err);
      setError(String(err));
    }
  }, []);

  useEffect(() => {
    void load();
    // 用户点「打开索引选项」调走到 PreferencesDialog 改完关掉后，
    // 会自动回到本步；这里 2s 轮询确保列表反映最新配置。
    const t = setInterval(() => void load(), 2000);
    return () => clearInterval(t);
  }, [load]);

  const openIndexingPrefs = () => {
    emitMenuAction("open-prefs-indexing");
  };

  const isEmpty = effectiveRoots !== null && effectiveRoots.length === 0;
  const usingConfigured =
    settings !== null && settings.index_roots.length > 0;
  // 2026-07-06 新语义：系统默认三夹仅当勾选 include_system_defaults 时纳入（与自定义解耦）。
  const usingDefaults = settings?.include_system_defaults === true;

  return (
    <>
      <p
        style={{
          color: "var(--muted)",
          margin: 0,
          marginBottom: "10px",
          lineHeight: 1.55,
          fontSize: "13px",
        }}
      >
        Scout <strong>只扫描你明确选择的目录，默认不索引任何位置</strong>
        。请添加你的常用工作目录（如 D 盘项目文件夹）；也可在索引选项里勾选
        「系统默认目录（音频 / 文档 / 图片）」。
      </p>

      <div
        style={{
          padding: "10px 12px",
          borderRadius: "10px",
          backgroundColor: "var(--header-bg)",
          marginBottom: "10px",
        }}
      >
        <div
          style={{
            fontSize: "13px",
            fontWeight: 600,
            marginBottom: "6px",
            color: "var(--fg)",
          }}
        >
          当前会被扫描的目录
          {effectiveRoots !== null && (
            <span
              style={{
                marginLeft: "6px",
                fontSize: "11.5px",
                color: "var(--subtle)",
                fontWeight: 400,
              }}
            >
              （共 {effectiveRoots.length} 个）
            </span>
          )}
        </div>

        {error && (
          <div style={{ color: "var(--status-err-fg)", fontSize: "12.5px" }}>
            读取配置失败：{error}
          </div>
        )}

        {!error && effectiveRoots === null && (
          <div style={{ color: "var(--muted)", fontSize: "12.5px" }}>加载中…</div>
        )}

        {!error && isEmpty && (
          <div
            style={{
              color: "var(--status-warn-fg)",
              fontSize: "12.5px",
              backgroundColor: "var(--status-warn-bg)",
              border: "1px solid rgba(245, 158, 11, 0.35)",
              padding: "6px 10px",
              borderRadius: "5px",
              lineHeight: 1.5,
            }}
          >
            尚未选择任何目录（默认不索引）——点下方「打开索引选项」添加至少一个，
            否则搜索不会有本地索引结果。
          </div>
        )}

        {!error && effectiveRoots && effectiveRoots.length > 0 && (
          <ul
            style={{
              listStyle: "none",
              padding: 0,
              margin: 0,
              maxHeight: "140px",
              overflowY: "auto",
              fontSize: "12.5px",
              color: "var(--fg)",
            }}
          >
            {effectiveRoots.map((p) => (
              <li
                key={p}
                style={{
                  padding: "4px 6px",
                  borderBottom: "1px solid var(--border)",
                  wordBreak: "break-all",
                }}
              >
                {p}
              </li>
            ))}
          </ul>
        )}

        {!error && effectiveRoots && effectiveRoots.length > 0 && (
          <div
            style={{
              marginTop: "6px",
              fontSize: "11.5px",
              color: "var(--muted)",
              lineHeight: 1.5,
            }}
          >
            {usingConfigured
              ? usingDefaults
                ? "= 你配置的目录 + 系统默认三夹（音频 / 文档 / 图片）"
                : "= 你配置的目录（不含系统默认）"
              : "= 系统默认三夹（音频 / 文档 / 图片）"}
          </div>
        )}
      </div>

      <div style={{ display: "flex", gap: "10px", alignItems: "center" }}>
        <button
          onClick={openIndexingPrefs}
          style={{
            backgroundColor: "#1c1917",
            color: "white",
            border: "none",
            padding: "7px 16px",
            borderRadius: "7px",
            cursor: "pointer",
            fontSize: "13px",
            fontWeight: 500,
          }}
        >
          打开索引选项…
        </button>
        <span
          style={{
            fontSize: "11.5px",
            color: "var(--subtle)",
            lineHeight: 1.5,
          }}
        >
          添加/移除目录 · 排除规则 · 关闭后本页自动刷新
        </span>
      </div>
    </>
  );
};

export default IndexRootsStep;
