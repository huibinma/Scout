# scout-desktop

Scout 桌面端应用，基于 Tauri 2 + React / TypeScript，面向 macOS 与 Windows。

## 产品结构

Desktop 使用固定应用壳层，不再模拟传统 Windows 七菜单：

- **找文件**：默认整页工作区，承载自然语言搜索、搜索历史、保存的搜索、意图调整、结果筛选、文件操作与内容预览。
- **设置**：整页工作区，五个分类——常规（含 Windows 下的 Everything 加速 / Windows 搜索集成与托盘子分区）、索引（含原「隐私与记录」的数据存储位置 / 一键清除 / 卸载清理）、语义召回、术语与同义词、本机 MCP 服务。
- **快速入门 / 关于**：放在侧栏底部的低频入口，不占用主任务空间。
- **首次启动引导**：仍使用 `/onboarding/mac` 与 `/onboarding/win` 独立路由，不套应用侧栏。

应用图标源文件为 `src-tauri/icons/generated/scout-icon.png`，由 Image-Gen 生成；`src-tauri/icons/` 下的 PNG / ICO / ICNS 为 Tauri 打包规格，`public/scout-icon.png` 用于前端品牌区。

## 目录结构

- `src/`：React / TypeScript 前端。
- `src/components/preferences/`：设置分类面板。
- `src-tauri/`：Rust 后端、Tauri Commands 与跨平台集成。
- `dist/`：前端构建产物（忽略入库）。

## 开发与构建

```bash
cd apps/desktop
npm install
npm run build
npm run tauri dev
```

仅检查前端视觉时可运行：

```bash
npm run dev
```

浏览器预览没有 Tauri Commands，设置数据、后端状态与真实搜索会显示加载或失败；完整功能验证必须使用 `npm run tauri dev`。

## 模块集成

Rust 端新增能力时：

1. 在 `src-tauri/src/main.rs` 导入模块。
2. 在 `tauri::generate_handler!` 中注册 Command。
3. 必要时在 Builder 中初始化插件。

前端新增设置能力时：

1. 把分类面板放入 `src/components/preferences/`。
2. 在 `preferences/shared.ts` 注册分类。
3. 在 `PreferencesDialog.tsx` 接入面板；该组件虽保留历史文件名，但现在渲染为整页设置工作区。

新增依赖必须同步 `docs/third-party-licenses.md`。
