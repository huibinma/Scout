// 全局唤起快捷键：内部存储格式（"Ctrl+Alt+F"，与后端 shortcut.rs 的 parse_shortcut
// 直接对应）与展示 / 录制逻辑的单一信源，供 ShortcutBanner 与 preferences 的
// ShortcutRecorder 共用，避免两处各写一份漂移。

/** 须与后端 `settings::DEFAULT_GLOBAL_SHORTCUT` 保持一致。 */
export const DEFAULT_GLOBAL_SHORTCUT = "Ctrl+Space";

const DISPLAY_LABELS: Record<string, string> = {
  Ctrl: "Ctrl",
  Alt: "Alt",
  Shift: "Shift",
  Cmd: "Cmd",
  BracketLeft: "[",
  BracketRight: "]",
  Backslash: "\\",
  Semicolon: ";",
  Quote: "'",
  Comma: ",",
  Period: ".",
  Slash: "/",
  Minus: "-",
  Equal: "=",
  Backquote: "`",
};

/** 主键部分录制时存的是 `KeyboardEvent.code`（如 "KeyF" / "Digit1"），展示时
 *  去掉后端解析也认得的前缀，还原成用户直觉认得的单字符。 */
function displayKeyToken(part: string): string {
  if (DISPLAY_LABELS[part]) return DISPLAY_LABELS[part];
  const keyMatch = /^Key([A-Z])$/.exec(part);
  if (keyMatch) return keyMatch[1];
  const digitMatch = /^Digit([0-9])$/.exec(part);
  if (digitMatch) return digitMatch[1];
  return part;
}

/** 内部格式（"Ctrl+Alt+KeyF"）转用户可读展示（"Ctrl + Alt + F"）。 */
export function formatShortcutForDisplay(raw: string): string {
  return raw
    .split("+")
    .map((part) => part.trim())
    .filter(Boolean)
    .map(displayKeyToken)
    .join(" + ");
}

/** 只表示修饰键本身的 `KeyboardEvent.code`——按下这些时还不构成完整组合，继续等主键。 */
const MODIFIER_CODES = new Set([
  "ControlLeft",
  "ControlRight",
  "AltLeft",
  "AltRight",
  "ShiftLeft",
  "ShiftRight",
  "MetaLeft",
  "MetaRight",
  "OSLeft",
  "OSRight",
]);

/**
 * 键盘事件 → 内部快捷键字符串。`event.code`（物理键、不受输入法/大小写影响）与后端
 * `parse_key` 接受的 token 高度重合（Space / KeyA / Digit1 / F1 / ArrowUp ...），
 * 直接透传即可。
 *
 * 返回 `null` 表示「还不是一个完整组合，继续等下一次按键」——包括：单独按下修饰键本身、
 * 或没有按住任何修饰键（后者故意拒绝：全局快捷键若无修饰键会吞掉该键的全部正常输入，
 * 必须在录制阶段就拦住，跟后端 `parse_shortcut` 的校验对称）。
 */
export function eventToShortcut(e: KeyboardEvent): string | null {
  // 没有 code（部分 IME 组合输入 / 合成事件）或纯修饰键本身，都还不构成完整组合。
  if (!e.code || MODIFIER_CODES.has(e.code)) return null;
  const mods: string[] = [];
  if (e.ctrlKey) mods.push("Ctrl");
  if (e.altKey) mods.push("Alt");
  if (e.shiftKey) mods.push("Shift");
  if (e.metaKey) mods.push("Cmd");
  if (mods.length === 0) return null;
  return [...mods, e.code].join("+");
}
