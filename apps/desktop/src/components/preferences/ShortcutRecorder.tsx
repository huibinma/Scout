import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import {
  DEFAULT_GLOBAL_SHORTCUT,
  eventToShortcut,
  formatShortcutForDisplay,
} from "../../lib/shortcut";

/**
 * 全局唤起快捷键录制器。自成一体、不经 `useAppSettings` 的表单快照流转
 * （同 McpPane 的架构选择）——原因见后端 `settings.rs` 的
 * `merge_backend_managed_fields` 注释：改动直接调 `update_global_shortcut`
 * 校验 + 重新注册 + 落盘，用户按下新组合的瞬间就知道是否与其他程序冲突，
 * 不必等重启后发现快捷键唤不起来；也因此不经「常规」表单的整体 Save，
 * 避免挂载时的旧快照把刚生效的新值冲回去。
 */
export function ShortcutRecorder({
  initialValue,
  disabled = false,
  disabledHint,
}: {
  initialValue: string;
  /** 为 true 时只读展示当前值、不可录制——用于「关窗即退出时快捷键其实唤不起来」
   *  这类场景，调用方（GeneralPane）负责判断何时禁用，本组件不关心具体原因。 */
  disabled?: boolean;
  /** disabled 为 true 时替换默认提示文案，说明为什么现在改不了。 */
  disabledHint?: string;
}) {
  const [current, setCurrent] = useState(
    initialValue || DEFAULT_GLOBAL_SHORTCUT,
  );
  const [recording, setRecording] = useState(false);
  const [busy, setBusy] = useState(false);
  const [feedback, setFeedback] = useState<{
    text: string;
    error: boolean;
  } | null>(null);

  const apply = async (shortcut: string) => {
    setBusy(true);
    setFeedback(null);
    try {
      await invoke("update_global_shortcut", { shortcut });
      setCurrent(shortcut);
      setFeedback({ text: "已保存并立即生效", error: false });
      setTimeout(() => setFeedback(null), 3000);
    } catch (err) {
      setFeedback({ text: String(err), error: true });
    } finally {
      setBusy(false);
    }
  };

  useEffect(() => {
    // 录制过程中外部条件变化（如用户顺手关掉了「关闭窗口时驻留系统托盘」）
    // 导致快捷键变不可用——立刻退出录制，别让用户对着一个不会生效的输入框按键。
    if (disabled && recording) setRecording(false);
  }, [disabled, recording]);

  useEffect(() => {
    if (!recording) return;
    const onKeyDown = (e: KeyboardEvent) => {
      // 录制期间任何按键都必须在这里截停——不能只 preventDefault：那只拦掉浏览器
      // 默认动作，同一事件仍会继续派发给其他监听器。真实踩过的坑：Esc 本意只是
      // 取消录制，若不 stopPropagation，还会被外层「设置面板 Esc 关闭」的全局
      // 监听器收到，整个设置弹窗被一起关掉。
      e.preventDefault();
      e.stopPropagation();
      // 双保险认 Escape：优先 code（物理键、跨布局稳定），退化到 key（部分合成 /
      // 特殊输入法路径下 code 可能缺失，key 更少见地跟着一起丢）。
      if (e.code === "Escape" || e.key === "Escape") {
        setRecording(false);
        return;
      }
      const shortcut = eventToShortcut(e);
      if (!shortcut) {
        // 纯修饰键，或还没等到「修饰键 + 主键」的完整组合——继续录制。
        return;
      }
      setRecording(false);
      void apply(shortcut);
    };
    // capture 阶段监听，确保录制时不会被输入框等子元素抢先处理。
    window.addEventListener("keydown", onKeyDown, true);
    return () => window.removeEventListener("keydown", onKeyDown, true);
    // apply 引用每次渲染都会变，但只在 recording 从 false→true 时才需要重新订阅。
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [recording]);

  return (
    <div className="prefs-field">
      <label className="prefs-label">全局唤起快捷键</label>
      <div style={{ display: "flex", gap: "8px", alignItems: "center" }}>
        <kbd
          className={`prefs-kbd${recording ? " recording" : ""}${disabled ? " disabled" : ""}`}
        >
          {recording ? "请按下新的快捷键…" : formatShortcutForDisplay(current)}
        </kbd>
        <button
          type="button"
          className="prefs-btn small"
          disabled={busy || disabled}
          onClick={() => setRecording((r) => !r)}
        >
          {recording ? "取消（Esc）" : "修改"}
        </button>
        {current !== DEFAULT_GLOBAL_SHORTCUT && (
          <button
            type="button"
            className="prefs-btn small"
            disabled={busy || recording || disabled}
            onClick={() => void apply(DEFAULT_GLOBAL_SHORTCUT)}
          >
            恢复默认
          </button>
        )}
      </div>
      {feedback && (
        <p
          className={`prefs-status ${feedback.error ? "status-text-err" : "status-text-ok"}`}
        >
          {feedback.error ? "⚠ " : "✓ "}
          {feedback.text}
        </p>
      )}
      <p className="prefs-hint">
        {disabled && disabledHint
          ? disabledHint
          : "点击「修改」后按下想要的组合键（须含 Ctrl / Alt / Shift / Cmd " +
            "中至少一个修饰键）。若与其他程序冲突会当场提示、不会保存；按 Esc " +
            "取消录制。"}
      </p>
    </div>
  );
}

export default ShortcutRecorder;
