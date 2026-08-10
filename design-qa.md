# Scout Desktop Design QA

## Evidence

- Implementation:
  - `docs/ui-reference/scout-find-files-1180x760.png`
  - `docs/ui-reference/scout-settings-1180x760.png`
  - `docs/ui-reference/scout-settings-900x620.png`
- Viewport and density:
  - 主要实现截图为 `1180 × 760` CSS px、浏览器 1× 截图。
  - 窄窗口补充验证为 `900 × 620` CSS px、1×。
- State:
  - 找文件：首次进入空状态。
  - 设置：常规分类，浏览器无 Tauri bridge，因此数据区为“加载中…”。

## Full-view review

Scout 的核心视觉语言为温暖的米白背景、黑色主操作、低饱和橙色强调、克制阴影、宽松留白、现代圆角和清晰的大标题层级。产品结构围绕 Scout 的文件检索任务组织为固定侧栏 + 整页工作区，不引入无关的聊天入口。

找文件页把搜索框置于首要层级，首次空状态用三条真实搜索示例解释“按意思寻找”；设置页沿用相同壳层和视觉 token，以分类侧栏承载已有设置能力。两个页面在同一框架下具有一致的留白、边界、圆角与操作按钮风格。

## Focused-region review

- 品牌区：Scout 使用 Image-Gen 生成的独立 S 图标；透明边缘、32px 识别度与深色底板均已检查，字母和构图保持原创。
- 搜索区：主搜索输入、辅助入口与黑色搜索按钮在 1180px 下对齐，无裁切；首次示例按钮使用统一 Phosphor 图标。
- 导航区：默认宽度显示主次说明；900px 下收成图标轨道，`aria-label` 仍保留“找文件 / 设置”可访问名称。
- 设置区：分类侧栏、内容面板与底部操作区在 1180px 和 900px 下均完整可见。

## Required fidelity surfaces

- Fonts and typography: 使用 Segoe UI Variable / Segoe UI / 系统中文字体回退；标题、正文、辅助文字形成三级层级。未发现异常换行、截断或字重漂移。
- Spacing and layout rhythm: 30px 主内容边距、14px 主卡片圆角与 10px 组件间距形成稳定节奏；1180px 与 900px 均无持续控件溢出。
- Colors and visual tokens: 米白 / 暖灰 / 黑色与橙色强调映射到统一 CSS tokens；绿色仅用于“仅在本机处理”等安全状态。
- Image quality and asset fidelity: 新 S 图标使用 1254px RGBA 源图生成 PNG / ICO / ICNS；透明角落与边缘无明显绿色残留。界面功能图标统一来自 Phosphor，没有用手写 SVG 或 emoji 替代主导航资产。
- Copy and content: “找文件”“按意思搜索本机内容”“仅在本机处理”等文案直接解释 Scout 价值和隐私边界；没有引入“新建对话”或聊天文案。

## Interaction and accessibility checks

- 已验证：找文件 ↔ 设置导航、设置分类切换、关于弹窗打开 / 关闭、默认与最小窗口响应式切换。
- 语义结构：主导航有 `aria-label`，设置分类使用 tab 语义，搜索示例为真实 button，关于窗口保留 dialog 语义。
- 控制台：浏览器预览只出现预期的 Tauri `invoke` 缺失错误；未发现 React 渲染、CSS 或导航错误。真实搜索、设置读写与后端状态需在 Tauri runtime 验证。
- 运行限制：本机没有 Rust 工具链，无法执行 `tauri dev`；前端 TypeScript / Vite 构建已通过。

## Comparison history

1. Initial responsive pass found a P2 issue: 900px 下侧栏文字仍占空间并被裁切。
2. Fix: compact breakpoint 改为物理隐藏品牌与导航说明，并为两个主导航按钮补充稳定 `aria-label`。
3. Post-fix evidence: `docs/ui-reference/scout-settings-900x620.png`，侧栏为完整图标轨道，内容与底部按钮无裁切，可访问名称仍存在。

## Findings

- No actionable P0/P1/P2 visual findings remain.
- P3: 真实 Windows WebView2 的字体栅格化、系统缩放和 Tauri 数据加载态仍需下一轮真机检查。

## Implementation checklist

- [x] 删除传统七菜单与无功能禁用入口。
- [x] 找文件改为整页主工作区。
- [x] 设置改为整页工作区并保留原设置分类。
- [x] 生成并接入 S 图标全套桌面规格。
- [x] 默认与最小窗口视觉验收。
- [x] TypeScript / Vite production build。
- [ ] Windows Tauri 真机逐页面功能回归。

final result: passed
