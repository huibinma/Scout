// 应用壳层 / 全局快捷键 → SearchView 内部 handler 的轻量事件总线。
// 传统菜单栏已于 2026-07-24 移除；保留事件总线用于跨工作区分发搜索动作，
// 避免把 SearchView 的大量内部状态提升到根组件。

export type MenuAction =
  | "new-search"
  | "open-selected"
  | "locate-selected"
  | "copy-path"
  | "focus-search"
  | "toggle-preview"
  | "reset-query"
  | "show-history"
  | "clear-history"
  | "save-search"
  | "open-prefs"
  | "open-prefs-indexing"
  | "open-prefs-misc"
  | "open-prefs-mcp";

const CHANNEL = "scout:menu";

interface MenuEventDetail {
  action: MenuAction;
}

export function emitMenuAction(action: MenuAction): void {
  if (typeof window === "undefined") return;
  window.dispatchEvent(
    new CustomEvent<MenuEventDetail>(CHANNEL, { detail: { action } }),
  );
}

// 返回 unsubscribe 函数；调用方在 useEffect cleanup 中调即可。
export function onMenuAction(
  handler: (action: MenuAction) => void,
): () => void {
  if (typeof window === "undefined") return () => {};
  const listener = (e: Event) => {
    const ce = e as CustomEvent<MenuEventDetail>;
    if (ce.detail?.action) handler(ce.detail.action);
  };
  window.addEventListener(CHANNEL, listener);
  return () => window.removeEventListener(CHANNEL, listener);
}
