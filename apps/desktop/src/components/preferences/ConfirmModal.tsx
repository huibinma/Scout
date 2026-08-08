import { useState } from "react";

/**
 * cycle 7-c：应用内二次确认 modal 的请求描述。
 *
 * **为什么不用 window.confirm**：wry/WebView2 生产装机版里 `window.confirm` 不显示
 * 任何对话框且守卫直接放行（v0.9.7 真机验证实锤、cycle 7-a 关闭守卫因此失效），
 * 二次确认类交互必须用 in-DOM modal。
 */
export interface ConfirmOption {
  key: string;
  label: string;
  hint?: string;
}

export interface ConfirmRequest {
  title: string;
  message: string;
  confirmLabel: string;
  /** 确认按钮用红色 danger 样式（默认蓝色 primary）。 */
  danger?: boolean;
  /** 非空 = 单选组；onConfirm 收到选中项 key。 */
  options?: ConfirmOption[];
  defaultOption?: string;
  onConfirm: (optionKey: string | null) => void;
}

/** cycle 7-c：通用 in-DOM 确认弹窗（关闭守卫 + 移除目录二次确认共用）。 */
export function ConfirmModal({
  req,
  onCancel,
}: {
  req: ConfirmRequest;
  onCancel: () => void;
}) {
  const [selected, setSelected] = useState<string | null>(
    req.defaultOption ?? req.options?.[0]?.key ?? null,
  );
  return (
    <div
      className="prefs-confirm-backdrop"
      onClick={(e) => {
        // 点击遮罩 = 取消本次确认。
        e.stopPropagation();
        onCancel();
      }}
    >
      <div
        className="prefs-confirm-card"
        role="alertdialog"
        aria-modal="true"
        onClick={(e) => e.stopPropagation()}
      >
        <h3 className="prefs-confirm-title">{req.title}</h3>
        <p className="prefs-confirm-msg">{req.message}</p>
        {req.options?.map((o) => (
          <label key={o.key} className="prefs-confirm-option">
            <input
              type="radio"
              name="prefs-confirm-opt"
              checked={selected === o.key}
              onChange={() => setSelected(o.key)}
            />
            <span>
              {o.label}
              {o.hint && <em className="prefs-confirm-hint">{o.hint}</em>}
            </span>
          </label>
        ))}
        <div className="prefs-confirm-actions">
          <button type="button" className="prefs-btn" onClick={onCancel}>
            取消
          </button>
          <button
            type="button"
            className={`prefs-btn ${req.danger ? "danger" : "primary"}`}
            onClick={() => req.onConfirm(selected)}
          >
            {req.confirmLabel}
          </button>
        </div>
      </div>
    </div>
  );
}
