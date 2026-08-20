// BETA-35 cycle 6：扫描版 PDF OCR 页渲染依赖引导（poppler-utils 里的 pdftoppm）。
// 与 NativeIndexCheckStep 同构：3s 自动轮询，装了自动绿标；未装展示复制按钮 + 官网链接。
// Windows 优先 winget 命令；macOS 走 brew。**opt-in**：只在需要检索扫描版 PDF
// （律所卷宗、扫描合同、老档案）时才需装；跳过不影响文本层 PDF / docx / 图片 OCR。
import React, { useCallback, useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";

const WINGET_CMD = "winget install oschwartz10612.Poppler";
const BREW_CMD = "brew install poppler";
const POPPLER_WIN_URL = "https://github.com/oschwartz10612/poppler-windows/releases";
const POPPLER_MAC_URL = "https://formulae.brew.sh/formula/poppler";

/**
 * 平台判定：Tauri window 上暴露 __TAURI_METADATA__ / navigator.platform，简单靠 UA 判 Windows / macOS。
 * 判错也不致命：仅影响展示的命令与链接，用户自己看得懂。
 */
function detectPlatform(): "windows" | "macos" | "other" {
  if (typeof navigator === "undefined") return "other";
  const ua = navigator.userAgent.toLowerCase();
  if (ua.includes("windows")) return "windows";
  if (ua.includes("mac os") || ua.includes("macintosh")) return "macos";
  return "other";
}

export interface PdftoppmCheckStepProps {
  onReady?: () => void;
}

export const PdftoppmCheckStep: React.FC<PdftoppmCheckStepProps> = () => {
  const platform = detectPlatform();
  const cmd = platform === "macos" ? BREW_CMD : WINGET_CMD;
  const url = platform === "macos" ? POPPLER_MAC_URL : POPPLER_WIN_URL;

  const [available, setAvailable] = useState<boolean | null>(null);
  const [checking, setChecking] = useState(false);
  const [copied, setCopied] = useState(false);

  const check = useCallback(async () => {
    setChecking(true);
    try {
      const ok = await invoke<boolean>("check_pdftoppm_available");
      setAvailable(ok);
    } catch (err) {
      console.error("[PdftoppmCheckStep] check_pdftoppm_available failed:", err);
      setAvailable(false);
    } finally {
      setChecking(false);
    }
  }, []);

  useEffect(() => {
    void check();
    const t = setInterval(() => void check(), 3000);
    return () => clearInterval(t);
  }, [check]);

  const copyCmd = async () => {
    try {
      await navigator.clipboard.writeText(cmd);
      setCopied(true);
      setTimeout(() => setCopied(false), 1500);
    } catch (err) {
      console.error("[PdftoppmCheckStep] clipboard write failed:", err);
    }
  };

  if (available === null) {
    return (
      <div style={{ padding: "8px 0", color: "var(--muted)", fontSize: "13px" }}>
        正在检测 pdftoppm 是否可用…
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
          ✓ 已检测到 pdftoppm（扫描版 PDF OCR 已就绪）
        </div>
        <div style={{ fontSize: "12.5px", color: "var(--status-ok-fg)", lineHeight: 1.5 }}>
          扫描件 / 图像化 PDF 会自动走"页渲染 → OCR"管线，命中卡预览可回到具体页码。
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
        <strong>扫描版 PDF OCR</strong>{" "}
        需要 <code>pdftoppm</code>（poppler-utils）把每页渲染成图后交 OCR 识别。
        <strong>典型场景</strong>：律所卷宗扫描件、老合同扫描件、公司归档纸质材料。
        <span style={{ color: "var(--status-warn-fg)" }}>
          本步可选，不装不影响文本层 PDF、docx、图片 OCR 等其他文档检索。
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
          方式 A：{platform === "macos" ? "Homebrew" : "winget"} 命令（推荐）
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
          <code style={{ flex: 1, wordBreak: "break-all" }}>{cmd}</code>
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
          {platform === "macos"
            ? "在终端里粘贴执行；poppler 是 Homebrew 里的通用 PDF 工具集。"
            : "在 PowerShell 或 CMD 里粘贴执行；这是社区维护的 poppler-windows 分发。"}
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
        {platform === "macos" ? "Homebrew 主页" : "从 GitHub Releases 下载"}{" "}
        <a
          href={url}
          target="_blank"
          rel="noreferrer"
          style={{ color: "var(--accent)", wordBreak: "break-all" }}
        >
          {url}
        </a>
        {platform !== "macos" && (
          <span style={{ color: "var(--muted)", display: "block", marginTop: "4px" }}>
            解压后把 <code>bin/</code> 加入 PATH（重启终端 / Scout 生效）。
          </span>
        )}
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

export default PdftoppmCheckStep;
