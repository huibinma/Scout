import { SynonymsPane } from "./SynonymsPane";

/**
 * BETA-47：「杂项」面板。收纳不属于其他分类的功能。
 *
 * 2026-07-07：原「我的同义词」是独立整页 `/synonyms`、进入后无返回入口，改为
 * 直接内联在本 tab（SynonymsPane）——设置内容统一收进选项对话框、不再跳出整页。
 * 后续零散设置（如 tracing 开关等）优先落这里、不再挤「常规」。
 */
export function MiscPane() {
  return <SynonymsPane />;
}
