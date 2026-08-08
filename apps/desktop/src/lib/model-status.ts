// BETA-33 cycle 9：模型状态类型 + 文案单一信源。
//
// 此前的散乱现状：`EmbedStatus` 类型在 StatusIndicator / PreferencesDialog /
// SettingsPage 三处各自复制；`embedStatusLine` 在后两处逐字重复；顶栏灯的
// tooltip 第三套文案直接拼 raw `expected_path` / Rust 错误串（「backend detail
// 字符串前端拼句」）。本模块收拢为：类型一份 + 面向两种表面的文案函数——
// 设置面板（全句、含诊断细节）与顶栏灯（短句、细节引导到选项对话框）。
//
// 生成模型 fallback（get_model_status）走相反约定：`detail` 由后端拼好整句、
// 前端裸展示——类型也收拢到此处，文案维持后端单一信源不在前端二次拼。

/** embedding 模型状态（与后端 `EmbedStatus` serde 标签枚举对应）。 */
export type EmbedStatus =
  | { state: "ready"; active_path: string }
  | { state: "loading" }
  | { state: "not_found"; expected_path: string }
  | { state: "failed"; reason: string }
  | { state: "unavailable"; reason: string };

/** 从绝对路径取文件名（跨平台，兼容 / 与 \ 分隔）。 */
export function baseName(path: string): string {
  const parts = path.split(/[\\/]/);
  return parts[parts.length - 1] || path;
}

/** 生成模型 fallback 状态（`get_model_status` 返回）：`detail` 为后端拼好的整句。 */
export interface ModelStatusJson {
  state: string;
  detail: string;
}

/**
 * 设置面板（选项对话框「语义召回」pane）状态行：全句 + apple 系色板。
 * 诊断细节（期望路径 / 加载失败原因）在此表面完整展示——这里就是「详见」的落点。
 */
export function embedStatusLine(s: EmbedStatus): { text: string; color: string } {
  switch (s.state) {
    case "ready":
      return {
        text: `语义召回：已就绪（当前模型：${baseName(s.active_path)}）`,
        color: "var(--status-ok-fg)",
      };
    case "loading":
      return { text: "语义召回：模型加载中…", color: "var(--accent)" };
    case "not_found":
      return {
        text: `语义召回模型未找到 —— 放到 ${s.expected_path} 后将自动启用`,
        color: "var(--muted)",
      };
    case "failed":
      return {
        text: `语义召回模型加载失败：${s.reason}`,
        color: "var(--status-err-fg)",
      };
    case "unavailable":
      return { text: `语义召回不可用：${s.reason}`, color: "var(--muted)" };
  }
}

/**
 * 顶栏状态灯：短句 + 灯色板。tooltip 不再拼 raw 路径 / Rust 错误串——
 * 细节统一引导到「选项 → 语义召回」（那里有完整路径、原因与下载按钮）。
 */
export function embedStatusBadge(s: EmbedStatus): { text: string; color: string } {
  switch (s.state) {
    case "ready":
      return { text: "就绪", color: "var(--status-ok)" };
    case "loading":
      return { text: "模型加载中", color: "var(--accent)" };
    case "not_found":
      return {
        text: "模型未找到 — 可在「选项 → 语义召回」下载",
        color: "var(--status-warn)",
      };
    case "failed":
      return {
        text: "模型加载失败 — 详见「选项 → 语义召回」",
        color: "var(--status-err)",
      };
    case "unavailable":
      return { text: "本构建不含语义召回", color: "var(--status-neutral)" };
  }
}
