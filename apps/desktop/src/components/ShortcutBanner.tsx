import { CheckCircle, X } from "@phosphor-icons/react";
import { useEffect, useState } from "react";

/** 启动后短暂提示全局唤起快捷键。 */
export const ShortcutBanner = () => {
  const [visible, setVisible] = useState(true);
  const [isMac] = useState(() => navigator.userAgent.includes("Mac"));

  useEffect(() => {
    const timer = window.setTimeout(() => setVisible(false), 5000);
    return () => window.clearTimeout(timer);
  }, []);

  if (!visible) return null;

  return (
    <div className="shortcut-toast" role="status">
      <CheckCircle size={18} weight="fill" />
      <span>
        Scout 已就绪 · 使用 <kbd>{isMac ? "⌥ Space" : "Ctrl + Space"}</kbd>{" "}
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
