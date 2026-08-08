// 快速入门 Windows 独有步骤：Everything CLI 检测与安装引导。
// 复用 `get_backend_status`（后端已注册 search.everything），前端 filter 判 is_available。
import React, { useCallback, useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";

interface BackendSummary {
  id: string;
  name: string;
  backend_kind: string | null;
  is_available: boolean;
  implementation_status: "real" | "stub";
}

const WINGET_CMD =
  "winget install voidtools.Everything voidtools.Everything.Cli";
const VOIDTOOLS_URL = "https://www.voidtools.com/downloads/";

export interface EverythingCheckStepProps {
  onReady: () => void;
}

export const EverythingCheckStep: React.FC<EverythingCheckStepProps> = () => {
  // null = 首次加载中；true = 已装且可用；false = 未装 / 未运行
  const [available, setAvailable] = useState<boolean | null>(null);
  const [checking, setChecking] = useState(false);
  const [copied, setCopied] = useState(false);

  const check = useCallback(async () => {
    setChecking(true);
    try {
      const list = await invoke<BackendSummary[]>("get_backend_status");
      const es = list.find((b) => b.id === "search.everything");
      setAvailable(es?.is_available ?? false);
    } catch (err) {
      console.error("[EverythingCheckStep] get_backend_status failed:", err);
      setAvailable(false);
    } finally {
      setChecking(false);
    }
  }, []);

  useEffect(() => {
    void check();
    // 用户可能在这一步切窗去装 Everything。3s 一轮询自动感知装完的时刻。
    const t = setInterval(() => void check(), 3000);
    return () => clearInterval(t);
  }, [check]);

  const copyCmd = async () => {
    try {
      await navigator.clipboard.writeText(WINGET_CMD);
      setCopied(true);
      setTimeout(() => setCopied(false), 1500);
    } catch (err) {
      console.error("[EverythingCheckStep] clipboard write failed:", err);
    }
  };

  if (available === null) {
    return (
      <div style={{ padding: "8px 0", color: "var(--muted)", fontSize: "13px" }}>
        正在检测 Everything 是否可用…
      </div>
    );
  }

  if (available) {
    return (
      <div
        style={{
          padding: "12px 14px",
          borderRadius: "10px",
          backgroundColor: "var(--status-ok-bg)",
          border: "1px solid rgba(34, 197, 94, 0.35)",
        }}
      >
        <div style={{ fontSize: "14px", color: "var(--status-ok-fg)", marginBottom: "3px" }}>
          ✓ 已检测到 Everything（es.exe CLI 可用）
        </div>
        <div style={{ fontSize: "12.5px", color: "var(--status-ok-fg)", lineHeight: 1.5 }}>
          Scout 会在文件名搜索、"忽然想不起在哪个盘"等场景下自动调用它加速；
          你不需要做任何额外配置。
        </div>
      </div>
    );
  }

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
        <strong>Everything</strong>{" "}
        是 Windows 上超快的文件名搜索工具。装了之后 Scout 会自动调用它加速
        <strong>按文件名找文件</strong>、并在 Windows 索引未覆盖的路径下兜底
        （如 <code>%TEMP%</code>、外接盘）。
        <span style={{ color: "var(--status-warn-fg)" }}>
          本步可选，不装也不影响语义/关键词搜索。
        </span>
      </p>

      <div
        style={{
          padding: "10px 12px",
          borderRadius: "10px",
          backgroundColor: "var(--header-bg)",
          marginBottom: "8px",
        }}
      >
        <div style={{ fontSize: "13px", fontWeight: 600, marginBottom: "5px" }}>
          方式 A：winget 命令（推荐）
        </div>
        <div
          style={{
            display: "flex",
            gap: "8px",
            alignItems: "center",
            backgroundColor: "#1e1e1e",
            color: "#e6e6e6",
            padding: "7px 10px",
            borderRadius: "5px",
            fontFamily: "Consolas, Menlo, monospace",
            fontSize: "12.5px",
          }}
        >
          <code style={{ flex: 1, wordBreak: "break-all" }}>{WINGET_CMD}</code>
          <button
            onClick={copyCmd}
            style={{
              backgroundColor: copied ? "var(--status-ok-fg)" : "#1c1917",
              color: "white",
              border: "none",
              padding: "3px 10px",
              borderRadius: "4px",
              cursor: "pointer",
              fontSize: "12px",
              whiteSpace: "nowrap",
            }}
          >
            {copied ? "已复制" : "复制"}
          </button>
        </div>
        <div
          style={{
            fontSize: "11.5px",
            color: "var(--muted)",
            marginTop: "5px",
            lineHeight: 1.5,
          }}
        >
          在 PowerShell 或 CMD 里粘贴执行；同时装主程序 + es.exe（两者都需要）。
        </div>
      </div>

      <div
        style={{
          padding: "10px 12px",
          borderRadius: "10px",
          backgroundColor: "var(--header-bg)",
          marginBottom: "8px",
          fontSize: "12.5px",
          lineHeight: 1.55,
        }}
      >
        <span style={{ fontWeight: 600 }}>方式 B：</span>
        官网下载{" "}
        <a
          href={VOIDTOOLS_URL}
          target="_blank"
          rel="noreferrer"
          style={{ color: "var(--accent)", wordBreak: "break-all" }}
        >
          {VOIDTOOLS_URL}
        </a>
        （除主程序外务必勾选 <strong>Command-line Interface</strong>）。
      </div>

      <div style={{ display: "flex", gap: "10px", alignItems: "center" }}>
        <button
          onClick={() => void check()}
          disabled={checking}
          style={{
            backgroundColor: "#1c1917",
            color: "white",
            border: "none",
            padding: "7px 16px",
            borderRadius: "7px",
            cursor: checking ? "wait" : "pointer",
            fontSize: "13px",
            fontWeight: 500,
            opacity: checking ? 0.7 : 1,
          }}
        >
          {checking ? "正在检测…" : "我已安装好，重新检测"}
        </button>
        <span style={{ fontSize: "11.5px", color: "var(--subtle)" }}>
          每 3 秒自动检测一次
        </span>
      </div>
    </>
  );
};

export default EverythingCheckStep;
