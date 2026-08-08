# Scout 产品定位与竞品分析

> 状态：v0.2（2026-05-27，经 Codex + Gemini 双工具核查后修订）
> 作者：Claude Code (Opus 4.7)
> 关联：[PROJECT.md](../PROJECT.md)、[local-personal-search-agent-project-plan.md](./local-personal-search-agent-project-plan.md) §1-§2、[Scout项目注意事项与风险清单.md](./Scout项目注意事项与风险清单.md)

本文是 Scout 的产品定位与外部沟通底稿。技术层面的"做什么 / 不做什么"以 [PROJECT.md](../PROJECT.md) 为准；本文档补充"为谁做 / 比谁好 / 价值多大"。

竞品价格 / 收入数据基于 2026 年 5 月公开信息（Gemini web 核查），对外使用前应再次复核当前数字。修订记录见文末 §8。

## 1. 一句话定位

> Scout 是一个本地优先、跨平台（macOS + Windows）的个人搜索 Agent —— 用自然语言操作 Spotlight / Windows Search 现有的索引能力，让普通人查得到、找得快、隐私不出本地。

英文：

> Scout is a local-first, cross-platform personal search agent for your files, documents, media, and memories on macOS and Windows.

口号：

> Deep Local Search.

## 2. 解决的实际问题

核心洞察：**Spotlight / Windows Search 的索引能力其实很强，但 UI 完全暴露不出来**。用户脑子里的查询往往是自然语言的组合条件，系统搜索的前端却只接受关键词。

举例（用户实测痛点）：

| 用户脑子里的查询 | Spotlight UI 怎么做 | mdfind 命令怎么做 |
|---|---|---|
| 最近一周下载的最大的文件 | 没法做 | `mdfind -onlyin ~/Downloads "kMDItemFSSize > 0 && kMDItemDownloadedDate > ..."` + 自己 sort |
| 一周内编辑过的 ppt | 模糊搜 "ppt" 然后翻页 | `kMDItemContentTypeTree == "public.presentation" && kMDItemContentModificationDate >= $time.this_week` |
| 把这些 pdf 复制到桌面 | 完全做不到 | 不支持 |

> 以上两条中文 query 是项目实测用户痛点，分别在 STATUS.md 第 10 阶段会话日志、parser v0.2.1 验收节点有记录。

Scout 不是"做一个新的索引引擎"，是"做一个让人能说人话的前端"。底层引擎免费、成熟、已经在用户机器上跑着；缺的是好用的入口。

## 3. 目标用户

### 3.1 已立项的目标用户（来自 [计划书 §2](./local-personal-search-agent-project-plan.md)）

- macOS 普通用户（Spotlight 用户）
- Windows 普通用户（Windows Search 用户）
- Everything 用户
- 文件较多但不擅长高级搜索语法的办公用户
- 本地资料、音乐、图片、文档较多的个人用户
- 重视隐私、不希望把个人文件上传给云端 AI 的用户

### 3.2 按价值分层

| 用户类型 | 价值 | 原因 |
|---|---|---|
| 信息工作者 / 研究员 / 设计师 | 高 | 文件多、跨平台、隐私敏感 |
| 程序员 / 跨 Mac+Win 工作者 | 高 | 跨平台一致体验，对 Agent + Policy 模型理解度高 |
| 普通办公族 | 中 | Spotlight 凑合够，差异主要在复杂组合查询 |
| 纯娱乐用户 | 低 | 文件管理需求低，不构成主要场景 |

### 3.3 三个典型用户画像（待用户校准）

- **A. 跨平台知识工作者**：Mac + Win 双机，文件分散，常找"前几天那份合同 / 设计稿"；不愿意上传 NDA 内容到云端
- **B. 隐私敏感开发者 / 研究员**：本地工作流为主，对系统级 AI（Apple Intelligence / Microsoft Copilot）持保留态度
- **C. Alfred / Raycast 老用户**：习惯快捷键启动器，想要"启动器 + 真正能找到文件的搜索"的升级版

## 4. 竞品全景

### 4.1 直接竞品（同代际桌面工具）

| 产品 | 平台 | 自然语言 | 本地优先 | 跨平台 | 价格 | 备注 |
|---|---|---|---|---|---|---|
| **Alfred** | Mac | 弱（关键词 + Workflow） | 是 | 否 | £34 / £59 一次性 Powerpack | 启动器为主，文件搜索次要 |
| **Raycast** | Mac + Win Public Beta | 中（Quick AI 走云端 GPT-4o / Claude） | 部分（本地 Ollama 实验） | 部分 | $8/月 Pro 订阅 + 免费版 | 当红新秀，AI 走云 |
| **HoudahSpot** | Mac | 否（表单式） | 是 | 否 | $34-39 一次性（v6.8.1，2026 仍活跃） | Spotlight 专业前端，专业用户向 |
| **Everything** | Win | 否 | 是 | 否 | 免费（个人用户） | 文件名搜索神器，但只搜文件名 |
| **Listary** | Win | 否 | 是 | 否 | $19.95 Pro 一次性，Freemium | Everything 的 UI 加强版 |
| **Recoll** | 跨平台（Mac/Win/Linux） | 否 | 是 | 是 | 开源 | 基于 Xapian 引擎，UI 老旧 |
| **DEVONthink Pro** | Mac | 部分 AI | 是 | 否 | $199 起一次性 | 重型知识管理，搜索是附带 |

> 上述价格 / 平台信息为 2026 年 5 月 Gemini web 核查。来源：各产品官网（alfredapp.com / raycast.com / houdah.com / voidtools.com / listary.com / recoll.org / devontechnologies.com）。

**关键结论**：**当前市场上没有同时满足"本地优先 + 跨平台 + 自然语言"三点的工具**。这是 Scout 真正的差异化空间。

### 4.2 系统厂商（最大威胁）

| 产品 | 进度 | 对 Scout 的影响 |
|---|---|---|
| **Apple Intelligence** | 已随 macOS 26 (Tahoe) 深度集成 | 高 — Apple 逐步覆盖基础自然语言搜索；硬件门槛 M1+ / 8GB 起步 |
| **Microsoft Copilot + Windows Search** | Semantic Indexing 推进中 | 高 — 系统级 AI 搜索；Copilot+ PC 硬件门槛 40 TOPS NPU |
| **Windows Recall** | 受隐私反弹拖慢（TotalRecall Reloaded 漏洞），默认关闭、显式 Opt-in，仅 Copilot+ 硬件 | 中 — 隐私争议反而给"本地优先"留窗口 |

**最大风险**：系统厂商把"用自然语言找文件"做成系统默认能力后，Scout 的基础场景被替代。需要在系统厂商完成基础体验前，把"agent 化操作 + 本地记忆"做出真正差异化（见 §5.2）。

### 4.3 AI / LLM 同类产品

| 产品 | 定位 | 与 Scout 的差别 |
|---|---|---|
| Anything LLM / GPT4All | 本地 LLM 问答 | 偏问答，不是文件搜索 |
| Mem.ai / Reflect / Notion AI | 云端笔记 + AI | 云端，不是本地文件 |
| Ollama 生态扩展 | 本地 LLM 基础设施 | 基础设施，不是终端产品 |

这一类目前没有真正的同位竞品，但生态变化快，需要持续观察。

### 4.4 国内市场（待补）

下列需要后续单独调研：讯飞、百度网盘 / 阿里云盘的本地搜索功能、华为 / 小米鸿蒙系统的全局搜索。

## 5. 差异化空间

### 5.1 短期防御（M / Beta 阶段）

1. **跨平台**：Alfred / HoudahSpot 仅 Mac、Everything / Listary 仅 Win，没人同时做
2. **本地优先**：不上传文件名 / 内容，对隐私敏感人群强吸引力（与系统厂商"云端 AI"形成对位）
3. **Agent 工具调用可解释**：每一步可追踪，比黑盒 AI 让用户更放心（见 [PROJECT.md](../PROJECT.md) 核心原则 / [docs/harness-design.md](./harness-design.md)）
4. **不绑硬件门槛**：不需要 M 系列 / Copilot+ PC，老 Intel Mac 与普通 Windows 机也能跑

### 5.2 长期护城河（Beta / 1.0 才能做）

[计划书](./local-personal-search-agent-project-plan.md) 已经预留了几个真正能形成壁垒的方向：

1. **本地活动洞察**（[计划书 §10.5](./local-personal-search-agent-project-plan.md)）：回答 "我最近在忙什么" / "哪些文件占用了最多工作时间" —— **系统厂商的训练数据不在用户机器上，结构上做不了**
2. **跨应用记忆 / 上下文**："我上次跟同事聊到的那个文档" —— 需要接入聊天、邮件、日历；系统厂商深度集成反而做不灵活
3. **Agent 化操作**：找到 + 做（删除、整理、复制）—— policy engine 已经为此铺路（MVP-10A FileActionTool）

**真正的护城河不在"自然语言搜索"，而在"agent + 本地记忆 + 跨平台一致"**。前者会被苹果 / 微软覆盖；后者它们做不了那么灵活。

## 6. 价值评估（待校准）

### 6.1 商业模型雏形

参照 Alfred / Raycast 模式：

- 个人付费（一次性 $20-40 或订阅 $5-8/月）
- 不做企业市场（企业有 Microsoft 365 / Box / Google Workspace 一揽子方案）
- 不做"免费 + 数据变现"模式（与"本地优先"原则根本冲突）

### 6.2 $100K ARR 可行性分析

以"独立开发者可持续年收入门槛"为锚点（$100K ARR）反推所需条件。

#### 硬数字：定价模型 → 付费用户量

| 定价模型 | 单价 | 年付费用户量 | 难度 |
|---|---|---|---|
| 一次性付费 | $30 | 3,333 新客/年 ≈ 9 人/天 | 高（每年需新增） |
| 一次性付费 | $40 | 2,500 新客/年 ≈ 7 人/天 | 中高 |
| 订阅 | $5/月（$60/年） | 1,667 活跃订阅 | 中（需抗 churn） |
| 订阅 | $8/月（$96/年） | 1,042 活跃订阅 | 中 |
| 混合（推荐） | $39 一次 + $19/年更新 | 2,200 新客 + 800 续费 ≈ $86K + $15K = $101K | 中 |

**核心是凑齐 1,000-2,000 名付费用户**，按 Mac 桌面工具市场的转化率，对应**约 20K-50K 试用 / 关注用户的基数**。

#### 对标产品的收入区间（估算）

| 产品 | 估算年收入 | 团队规模 | 备注 |
|---|---|---|---|
| Alfred | $800K-$1.2M 年销售额（非订阅，ARR 口径不严谨） | 单开发者主导 | 2010 至今，15 年品牌沉淀（来源：GetLatka / 行业估算） |
| Bartender | $1.5M-$2.5M ARR（2024 被 Applause 收购后转订阅） | Applause 旗下 | 收购前为单开发者（来源：Applause 投资组合分析） |
| Things 3 | $2M-$5M | 小团队（Cultured Code） | 多年运营 |
| HoudahSpot | $250K-$450K | 单开发者 | 垂直专业工具（来源：Indie Mac 开发者基准） |
| Raycast | 数百万级 ARR（约 $5M，2025）；融资总额数千万 ($47.8M) | VC 投资团队 | 不同档，仅作上限参考 |

> 上述数字为 Gemini web 核查 + 行业第三方估算，**非官方披露**。对外使用前应注明 "第三方估算"。

**结论**：$100K ARR 是"成熟独立桌面工具的入门线"——比 HoudahSpot 等极垂直工具低一个量级（HoudahSpot 实际 $250K-$450K），比 Alfred / Things 等品类龙头低两个量级，**对单开发者持续投入 2-3 年是合理目标**。不是天花板也不是默认结果。

#### 达成的必要条件（按重要性排序）

1. **产品到达 sellable 状态**：parser 字段精确匹配 ≥80%（**当前 53.8%**，parser v0.5 已落地）、模型 fallback 默认开启、Windows 端真机实测、签名分发（ROADMAP 在 Beta 范围）+ 自动更新（ROADMAP 在 1.0 范围）+ 崩溃恢复（ROADMAP 在 1.0 范围）齐全。**从当前到 sellable 乐观 6 个月、现实 12 个月**。
2. **跨平台双优**：Scout 最大差异化是跨平台，但很多跨平台工具最后变成"Mac 好用、Win 凑合"，差异化垮一半。Windows 端必须真正打磨到位。
3. **抓住 12-24 个月窗口**：Apple Intelligence / Microsoft Copilot 在吃这块市场；24 个月后基础自然语言搜索是不是还能作为卖点存疑。
4. **营销 / 分发不靠运气**：需要至少一项支撑——Product Hunt 成功首发、Build in Public、技术博客 / 推特 / 油管影响力、Hacker News 偶发爆款、App Store / Microsoft Store 排名。**纯靠"产品好自然有人买"在 2026 年已不成立**。参考案例：Screen Studio（$720K ARR）、TypingMind（$1M+ ARR）、BoltAI（$360K ARR）均靠 Product Hunt + Build in Public 达成。

#### 时间表（乐观情况）

| 阶段 | 时间 | 预期年收入 | 用户量 |
|---|---|---|---|
| MVP 发布 + 灰度 | 2026 H2 | $0 | <100 灰度 |
| 1.0 + 公开发售 | 2027 H1 | $20-30K | 500-800 付费 |
| 口碑 + 内容营销 | 2027 H2 - 2028 | $50-60K | 1,200-1,500 付费 |
| 持续增长 | 2028-2029 | **$100K 达成** | 1,800-2,500 付费 |

**最快 2-3 年达成**，前提是上述四项必要条件全做对。

#### 风险打折因素

| 风险 | 影响 | 概率 |
|---|---|---|
| Apple Intelligence 把基础体验做到"够好" | 高 — 直接吃掉 Mac 端基础场景 | 中高 |
| Windows 用户付费意愿低于 Mac（行业数据：Mac 用户软件支出平均高 15-20%） | 中 — 拉低混合 ARPU | 高 |
| 1.5B 模型 + LoRA 微调没达预期 | 中 — 模型路径无净收益，差异化讲不出来 | 中 |
| 独立开发者中途断粮（缺副业 / 储蓄） | 高 — 项目停摆 | 中 |
| 跨平台分散注意力，两边都做不深 | 中 | 中 |

#### 判断

- **全部必要条件成立 + 抓住窗口 → 第 3 年达到 $100K ARR**
- **掉链子 1 项 → 停在 $30-50K**（够糊口但无增长）
- **掉链子 2 项及以上**（最常见）→ 停在用户 <500 的小众工具，年收入 <$10K

**朴素对比基准**：单人独立桌面工具做到 $100K ARR，业内大约是"小有名气的工具"档（不是"小众工具"档）。能不能到这一档，**愿意投入多少年 + 是否会做营销 比技术本身更决定结果**。

### 6.3 关键变量

- **苹果 / 微软的速度**：如果他们 1-2 年内把基础自然语言搜索做"够好"，Scout 必须在那之前完成往 agent + 记忆方向的过渡
- **隐私话题的张力**：Apple Intelligence 上云、Windows Recall 隐私争议都给"本地优先"加分；但需要用户教育跟上
- **本地小模型质量曲线**：1.5B 模型当前精度不够（见 [MVP-17 hybrid 实测](./reviews/mvp-17-fallback-evals.md)：v0.3 hybrid 与 parser-only 持平，pass 108 vs 109），等 LoRA / 3B 级别落地后体验跳一档

## 7. 待校准事项

本文档是 v0.2 修订稿，下列需要用户校准后定稿：

- [ ] §3.3 用户画像 A/B/C 是否对得上你心目中的目标用户
- [ ] §6.1 商业模型（一次性 vs 订阅 vs 混合）的偏好
- [ ] 是否要做"对每个竞品的差异化矩阵"细化版本（vs Alfred、vs Raycast、vs HoudahSpot 一对一打分）
- [ ] §4.4 国内市场竞品是否纳入正式分析
- [ ] §6.2 对标产品收入第三方估算的口径（销售额 vs ARR）外沟通前是否需要进一步收紧

## 8. 修订记录

### v0.2（2026-05-27） — 双工具核查后修订

**核查依据**：Codex 内部数据核查报告 + Gemini 外部数据 web 核查报告。

**修订项**：

1. **§2 痛点示例**："上周改过的 ppt" → "一周内编辑过的 ppt"（与 STATUS.md 实测原话对齐）
2. **§3.1 目标用户**：从 4 条恢复到计划书 §2 原本的 6 条
3. **§4.1 竞品表**：
   - Alfred 价格补 "£34 / £59 一次性 Powerpack"
   - Raycast 平台细化 "Mac + Win Public Beta"、AI 注明 "Quick AI 走云端 GPT-4o / Claude"、订阅 "$8/月 Pro"
   - HoudahSpot 补 "$34-39 一次性（v6.8.1，2026 仍活跃）"
   - Listary 补 "$19.95 Pro 一次性，Freemium"
   - DEVONthink Pro "$99+ 重型" → "$199 起一次性"
   - 加表后来源备注（产品官网）
4. **§4.2 系统厂商**：Apple Intelligence 注明 macOS 26 (Tahoe) + M1+ / 8GB；Copilot+ PC 注明 40 TOPS NPU；Windows Recall 加 TotalRecall Reloaded 漏洞与默认 Opt-in 状态
5. **§5.2 计划书章节引用**：本地活动洞察 §8.6 → **§10.5**（实际章节号）
6. **§6.2 必要条件 1**：parser 字段精确匹配数字 51.8% → **53.8%**（parser v0.5 已落地）；sellable 分阶段标注：签名分发(Beta) / 自动更新(1.0) / 崩溃恢复(1.0，ROADMAP 实际命名)
7. **§6.2 混合定价行**：原 "$39 一次 + $19/年更新（~2,000 历史 + 800 续费）" 算式 $93,200 ≠ $100K，调为 "2,200 新客 + 800 续费 ≈ $101K"
8. **§6.2 对标收入表**：
   - Alfred "$1-2M ARR" → "$800K-$1.2M 年销售额"（非订阅，ARR 口径不严谨；标 GetLatka 来源）
   - Bartender "$500K+" → "$1.5M-$2.5M ARR（2024 被 Applause 收购后转订阅）"
   - Things 3 "$500K-1M" → "$2M-$5M"
   - HoudahSpot "$50-200K" → "$250K-$450K"
   - Raycast "数千万级" → "数百万级 ARR（约 $5M，2025），融资总额数千万 ($47.8M)"
   - 表后加 "非官方披露，对外应注明第三方估算" 备注
9. **§6.2 营销条款**：补真实案例 Screen Studio ($720K ARR) / TypingMind ($1M+ ARR) / BoltAI ($360K ARR)
10. **§6.2 风险表**：Windows 付费意愿条加行业数据 "Mac 用户软件支出平均高 15-20%"

**保留未改**：
- §3.3 用户画像 A/B/C（主观判断，待用户校准）
- §6.1 商业模型雏形（待用户偏好确认）
- §6.2 时间表（基于当前进度的乐观推算，ROADMAP 未承诺绝对日期）
- §5.2 / §4.3 / §4.4 内容（无核查异议）
