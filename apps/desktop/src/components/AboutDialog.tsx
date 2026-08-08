import { useEffect, useState } from "react";
import { getVersion } from "@tauri-apps/api/app";

// BETA-33 cycle 1：「关于 Scout」模态对话框。
// 简单展示版本号 + 一句话定位 + GitHub 链接。Esc / 点遮罩 / 点关闭 三种方式关闭。

interface Props {
  onClose: () => void;
}

const REPO_URL = "https://github.com/huibinma/Scout";

export default function AboutDialog({ onClose }: Props) {
  const [version, setVersion] = useState<string>("");

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
        <div className="about-actions">
          <button type="button" className="about-close-btn" onClick={onClose}>
            关闭
          </button>
        </div>
      </div>
    </div>
  );
}
