import { useEffect, useState } from "react";
import { getVersion } from "@tauri-apps/api/app";
import { UseAutoUpdate } from "../hooks/useAutoUpdate";
import { formatBytes } from "./preferences/shared";

// BETA-33 cycle 1：「关于 Scout」模态对话框。
// 简单展示版本号 + 一句话定位 + GitHub 链接。Esc / 点遮罩 / 点关闭 三种方式关闭。
//
// 2026-08-10：新增「检查更新」手动入口——真机反馈自动检查有感知不到的窗口期（见
// useAutoUpdate.ts 顶部注释），需要一个用户能主动触发、且能看到明确结果（已是最新 /
// 发现新版本 / 检查失败）的手动按钮。`autoUpdate` 由 App.tsx 统一持有一份 hook 实例
// 传入——命中新版本时与左下角 toast 是同一份状态，不会两处各自查出不一致的结果。

interface Props {
  onClose: () => void;
  autoUpdate: UseAutoUpdate;
}

const REPO_URL = "https://github.com/huibinma/Scout";

export default function AboutDialog({ onClose, autoUpdate }: Props) {
  const [version, setVersion] = useState<string>("");
  const [checking, setChecking] = useState(false);
  const [checkMessage, setCheckMessage] = useState<string | null>(null);

  useEffect(() => {
    getVersion()
      .then(setVersion)
      .catch(() => setVersion("(未知)"));
  }, []);

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") onClose();
    };
    document.addEventListener("keydown", onKey);
    return () => document.removeEventListener("keydown", onKey);
  }, [onClose]);

  // 一旦全局状态自己变成 available（无论是本次手动检查触发、还是后台轮询/启动
  // 即时检查先一步命中），旧的「已是最新版本」提示就该让位，避免两条消息同时显示。
  useEffect(() => {
    if (autoUpdate.status === "available") {
      setCheckMessage(null);
    }
  }, [autoUpdate.status]);

  const handleCheck = async () => {
    setChecking(true);
    setCheckMessage(null);
    try {
      const result = await autoUpdate.checkNow();
      if (!result) setCheckMessage("已是最新版本");
    } catch (e) {
      const msg = typeof e === "string" ? e : JSON.stringify(e);
      setCheckMessage(`检查失败：${msg}`);
    } finally {
      setChecking(false);
    }
  };

  const busy =
    checking ||
    autoUpdate.status === "downloading" ||
    autoUpdate.status === "installing";

  return (
    <div className="about-backdrop" onClick={onClose}>
      <div
        className="about-dialog"
        role="dialog"
        aria-modal="true"
        aria-labelledby="about-title"
        onClick={(e) => e.stopPropagation()}
      >
        <div className="about-brand">
          <img src="/scout-icon.png" alt="" />
          <div>
            <h2 id="about-title" className="about-title">
              Scout
            </h2>
            <p className="about-version">版本 {version || "..."}</p>
          </div>
        </div>
        <p className="about-tagline">
          一切均在本地计算和存储，面向人和 Agent 的文件检索工具，查找电脑里的文档、图片、音频和记忆线索等。
        </p>
        <p className="about-link">
          <a
            href={REPO_URL}
            target="_blank"
            rel="noopener noreferrer"
            // BETA-33 cycle 1：webview 内 target=_blank 是否能拉系统浏览器
            // 取决于 Tauri 默认拦截配置；本 cycle 不装 plugin-shell。
            // 不通时用户可手动复制 URL；cycle 2 接 plugin-shell 后改 invoke。
          >
            {REPO_URL}
          </a>
        </p>

        <div className="about-update">
          <div className="about-update-row">
            <button
              type="button"
              className="about-update-btn"
              onClick={handleCheck}
              disabled={busy}
            >
              {checking ? "检查中…" : "检查更新"}
            </button>
            {checkMessage && !busy && (
              <span className="about-update-message">{checkMessage}</span>
            )}
          </div>

          {autoUpdate.status === "available" && autoUpdate.info && (
            <div className="about-update-found">
              <span>发现新版本 v{autoUpdate.info.version}</span>
              <button
                type="button"
                className="about-update-btn"
                onClick={autoUpdate.install}
              >
                立即更新
              </button>
            </div>
          )}

          {autoUpdate.status === "downloading" && (
            <p className="about-update-message">
              正在下载 v{autoUpdate.info?.version} ·{" "}
              {formatBytes(autoUpdate.progress.downloaded)}
              {autoUpdate.progress.total
                ? ` / ${formatBytes(autoUpdate.progress.total)}`
                : ""}
            </p>
          )}

          {autoUpdate.status === "installing" && (
            <p className="about-update-message">正在安装，即将自动重启…</p>
          )}

          {autoUpdate.status === "error" && autoUpdate.error && (
            <p className="about-update-message about-update-error">
              更新失败：{autoUpdate.error}
            </p>
          )}
        </div>

        <div className="about-actions">
          <button type="button" className="about-close-btn" onClick={onClose}>
            关闭
          </button>
        </div>
      </div>
    </div>
  );
}
