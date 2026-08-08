import { CheckCircle, X } from "@phosphor-icons/react";
import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import {
  DEFAULT_GLOBAL_SHORTCUT,
  formatShortcutForDisplay,
} from "../lib/shortcut";

/**
 * 启动后短暂提示全局唤起快捷键。快捷键现在可在「常规」设置里自定义，
 * 此处必须读真实生效值（`get_settings`），不能再按平台硬编码猜——
 * 用户改过之后硬编码的提示文案会跟实际不符。
 */
export const ShortcutBanner = () => {
  const [visible, setVisible] = useState(true);
  const [shortcut, setShortcut] = useState(DEFAULT_GLOBAL_SHORTCUT);

  useEffect(() => {
    invoke<{ global_shortcut?: string }>("get_settings")
      .then((s) => {
        if (s.global_shortcut) setShortcut(s.global_shortcut);
      })
      .catch(console.error);
  }, []);

  useEffect(() => {
    const timer = window.setTimeout(() => setVisible(false), 5000);
    return () => window.clearTimeout(timer);
  }, []);

  if (!visible) return null;

  return (
    <div className="shortcut-toast" role="status">
      <CheckCircle size={18} weight="fill" />
      <span>
        Scout 已就绪 · 使用 <kbd>{formatShortcutForDisplay(shortcut)}</kbd>{" "}
        随时唤起
      </span>
      <button
        type="button"
        onClick={() => setVisible(false)}
        aria-label="关闭快捷键提示"
      >
        <X size={15} />
      </button>
    </div>
  );
};

export default ShortcutBanner;
