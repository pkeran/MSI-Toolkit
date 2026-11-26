# MSI-Toolkit UI 设计规范（UI Design Spec）

> 文档所有者：`@Pi-UI`　|　状态：v1.1.0（按用户三条硬标准重构，并已落地为可编译实现）　|　最后实测：2026-11-27
> 上游契约：`docs/ARCHITECTURE.md`、`docs/CONTRACTS.md`、`docs/HARDWARE_PROTOCOLS.md`、`docs/AUTOMATION_SPEC.md`
> 下游实现：`crates/msilt-ui`（Slint 1.19.1 + `renderer-software` + `backend-winit` + Win32 DWM）
> 许可证红线：LenovoLegionToolkit / GhostDeck 为 GPL-3.0，本规范只参考**设计范式与信息架构**，不复制任何源码、资源或布局文件；全部 Slint/Win32 代码独立实现。

---

## 0. 三条用户硬标准（v1.1 唯一裁决依据）

> 本节优先级高于本文其余任何描述；冲突时以本节为准。每一条都有可机械验收的落地位置。

### 0.1 零 emoji、零 AI 味

| 规则 | 具体要求 | 落地位置 |
|---|---|---|
| 禁止 emoji 图标 | 代码与文档中**不得**出现任何 emoji 充当 UI 图标；本机实测 `⚡❄▤▣` 在 Segoe UI Variable 下渲染为豆腐块 | `ui/icons.slint`，全部为 `Path` |
| 统一矢量图标 | 全部图标是 **Fluent System Icons 风格的单色描边 `Path`**：20×20 viewbox；16 px 用 **2 px** 线宽、20 px 用 **1.5 px** 线宽；`stroke-line-cap/join: round` | `ui/icons.slint`（33 个字形，见 §3.7） |
| 禁止 AI 味视觉 | 无紫蓝赛博渐变、无装饰光晕、无霓虹描边、无大面积高斯模糊、无营销式大标语、无空洞文案 | `ui/theme.slint` 全文无 `@linear-gradient`、无 shadow |
| 单一强调色 | 只有一个 accent（深色 `#4CC2FF` / 浅色 `#0F6CBD`），只用于「当前选择 / 主操作 / 危险」 | `ui/theme.slint` |
| 平面 + 1 px 描边 | 层级靠 1 px 低对比描边与背景明度差表达，**不用阴影**（软件渲染下阴影代价高且廉价） | `components/compact_card.slint` |
| 文案克制 | 只写「作用 + 当前硬件状态 + 限制原因」；不写「赋能 / 一键 / 极致 / 智能」这类营销词 | 全部 `pages/*.slint` |

### 0.2 优先复用成熟组件库

- `build.rs` 用 `slint_build::CompilerConfiguration::new().with_style("fluent-dark".into())` 编译 **Slint 官方 `fluent` 样式**，全部通用交互控件直接从 `std-widgets.slint` 复用（复用表见 §4.1）。
- 只在标准库确实没有等价物时才自研 **领域专属** 组件（§4.2）：`CompactCard`、`StatusPill`、`Banner`、`FanCurveEditor`、`ZoneMatrix`、`RuleCard`。
- 亮/暗主题不是两次构建：编译期固定 `fluent` 控件集，运行期通过 `Palette.color-scheme`（由 `Theme.apply()` 写入）跟随 Windows 11 应用主题。

### 0.3 极致紧凑、零大空洞

- 默认窗口 **1080×700**，最小 **960×620**（Win32 `WM_GETMINMAXINFO` + `WM_WINDOWPOSCHANGING` 双重钳制；实测 500×360 的请求被钳到 1440×930 物理 = 960×620 @150%）。
- 顶栏 **40 px**、左导航 **168 px**（紧凑 46 px）、内容区边距 **12 px**、卡片间距 **8 px**、紧凑卡片 **58 px** / 标准 **66 px**。
- 页面高度由 `body-h = 窗口高 − chrome − 页面头 − 2×边距` 显式推导，可伸缩行用显式高度填满，**任何页面都不允许出现底部空白带**（§2.4 有强制公式）。

---

## 0.4 结论摘要（给赶时间的人）

1. **渲染后端：`renderer-software`（Slint 软件光栅）+ `backend-winit`**。本机实测（Windows 11 26200 / 2560×1600@240Hz / 150% DPI / RTX 5090 Laptop）：EXE **8.75 MiB**、首帧 **101–118 ms**、Private **11.7–11.8 MiB**、Working Set **33.0 MiB**、0 GPU 上下文、0 显存、Mica 实测可用。FemtoVG 262 ms / 110–163 MiB、Skia ~840 ms / 114–156 MiB、WGPU 1237 ms / 198–205 MiB 全部淘汰（§9.3）。
2. **Mica 桥接已真机验证**：`DwmSetWindowAttribute(20/33/34/38)` + `DwmExtendFrameIntoClientArea(-1)` 全部 `hr=0`，`DWMWA_SYSTEMBACKDROP_TYPE` 回读 2；浅色下客户区像素 `#F5F2EF` 证明 alpha 合成成功。`Theme.mica` 在回读失败时自动退化为不透明实底。
3. **设计主张**：MSI Center 的痛点是「卡顿、简陋、信息不明确、功能不明确」。四条不可妥协的体验铁律：
   - **零废话**：一屏内回答「现在是什么状态、改这个会发生什么、为什么不能改」。
   - **零假值**：`None`/`Stale`/`Unknown` 一律显示 `—` 与时间，绝不补 0、绝不假装已生效。
   - **零轮询**：UI 只消费 daemon 推送，禁止 `Timer` 拉状态；遥测只在订阅期间 1 Hz。
   - **关窗即退**：无托盘、无后台线程、无字体缓存残留；进程消失即 0 MiB / 0 显存（实测 `CloseMainWindow` 后 0 残留）。
4. **交付边界**：UI **不创建** OSD/准星窗口，不直接调用 HAL，不写 EC；所有硬件动作经 IPC v1，并携带 `expectedRevision`。UI 只负责「可理解的状态 + 可撤销的意图」。
5. **实现状态**：`crates/msilt-ui` 已完整落地，`cargo check -p msilt-ui` 0 error / 0 warning、`cargo test -p msilt-ui` 19 passed、`--check-layout` 0 项失败；8 个页面全部可渲染（附录 D）。

---

## 1. 设计目标、原则与反模式

### 1.1 四条体验铁律（可验收）

| 铁律 | 具体定义 | 验收方式 |
|---|---|---|
| 零废话 | 每个控件 = 图标 + 主标题 + 解释「作用 / 当前硬件状态」的副标题 + 即时控件；任何需要用户猜的缩写都展开 | 5 秒可用性测试：新用户能在 5 s 内找到「限制充电到 80%」并说出副作用 |
| 零假值 | `Option::None` → `—` + 原因/时间；`EcWriteStatus != Enabled` → 控件禁用并给出原因；请求确认 ≠ 已生效 | 断连、只读固件、未验证映射、官软占用、待重启 5 种状态逐页核对 |
| 零轮询 | 无 `Timer` 拉 IPC；无 1 Hz 以上的状态查询；动画只改局部属性 | 代码审查：唯一的 1 Hz `Timer` 只推进会话时钟与 `--demo` 合成遥测 |
| 关窗即退 | `WM_CLOSE` → 退出事件循环 → 进程结束；不驻留托盘、不保留 DWM 句柄 | `Get-Process msilt-ui` 在 800 ms 内消失；`tools/capture.ps1` 自动断言 0 残留 |

### 1.2 设计原则

1. **状态先于控制**：先告诉用户「现在是什么」，再给「可以改成什么」。所有控件都带当前值/当前状态。
2. **能力驱动 UI**：`capabilities ∩ writable` 是 UI 的立法机关。不可写的能力不渲染可点击控件，而是渲染「为什么不可用 + 解锁条件」。
3. **危险操作可理解、可反悔**：停用官软、MUX、EC 写入必须逐项列出「对象 / 当前 / 目标 / 影响面」，并提供同页还原；禁止一个「优化」大按钮。
4. **本地预览、一次提交**：拖动风扇曲线、刷 24 区颜色只改本地模型；点「应用」才发一次 IPC；收到 `Applied` 快照后才把观测值更新为回读值。
5. **克制的 Fluent 2**：Mica 是背景不是主角；内容层用低对比卡片承载信息，强调色只给「当前选择 / 主操作 / 危险」。
6. **中文优先的排版**：中文正文 13 px、行高 20 px；数字用等宽字体对齐；英文专有名词保留原文（EC / MUX / ICC / RPM），首次出现给中文解释。
7. **可访问性不是附加项**：键盘可达、焦点可见、对比度达标、200% DPI 不裁切、减少动效可关闭。

### 1.3 明确反模式（禁止出现）

- ❌ 单个「一键优化 / 一键加速」大按钮，背后做多件不可见的事。
- ❌ 把 `None` 显示成 `0°C` / `0%` / `0 RPM`。
- ❌ 请求发出后立刻把开关 / 曲线改成「已生效」样式。
- ❌ 在 UI 进程里建第二个 OSD / 准星窗口、注入游戏、轮询前台进程。
- ❌ **用 emoji 当图标**（本机实测 `⚡❄▤▣` 渲染为豆腐块）。
- ❌ **紫蓝赛博渐变、装饰性光晕、霓虹描边、营销式大标语、空洞文案**。
- ❌ 全屏高斯模糊 / 大阴影 / 大面积透明叠透明（软件渲染下显著增加重绘成本）。
- ❌ 把 24 区键盘做成 24 份独立布局树（应使用 `for` + 单网格）。
- ❌ 用 `Path` 画图表却不设 `fit: fill`（默认 `contain` 会 letterbox，实测踩坑）。
- ❌ 页面底部留 100 px 以上的空白带（§2.4 已用显式高度消除）。

---

## 2. 信息架构（IA）

### 2.1 全局壳层（紧凑版）

```
┌────────────────────────────────────────────────────────────────────────────────────┐
│ 顶栏 40px：☰ │ 产品名 │ 模式胶囊 │ AC/DC │ CPU │ GPU │ 风扇 │ 刷新率 │……│ 连接胶囊 │
├──────────────┬─────────────────────────────────────────────────────────────────────┤
│ 左导航 168px │ 页面头 26px：标题 20/600 + 一句话说明（12/400 tertiary）           │
│ （8 项，32px）│ 内容区 padding 12px，卡片间距 8px                                    │
│              │ ┌───────────────┐ ┌───────────────┐                                 │
│  总览         │ │ CompactCard   │ │ CompactCard   │  ← 58–66px 高，满铺栅格          │
│  性能与风扇   │ └───────────────┘ └───────────────┘                                 │
│  …           │ ┌───────────────────────────────────────┐ ┌──────────────────┐        │
│  底部：版本   │ │ 可伸缩行（显式 height = body-h − 固定行）│ │ 右侧事实栏        │        │
│              │ └───────────────────────────────────────┘ └──────────────────┘        │
├──────────────┴─────────────────────────────────────────────────────────────────────┤
│ 页脚 22px：连接点 │ 连接状态 · IPC v1 · 快照 rN · 遥测 1 Hz │ 关窗即退出提示        │
└────────────────────────────────────────────────────────────────────────────────────┘
```

- 窗口默认 **1080×700 逻辑像素**，最小 **960×620**；150% DPI 下实测窗口 1642×1106 物理像素。
- 顶栏 40 px；导航 168 px（紧凑 46 px，`Ctrl+B` 或顶栏 `‹` 切换）；内容区 padding 12 px；卡片间距 8 px；页面头 26 px。
- 页面切换不做整页滑动，只换内容；导航顺序固定（不可由用户重排）。
- 快捷键：`Ctrl+1..8` 直达页面；`Ctrl+B` 折叠导航；`Esc` 关闭对话框/退出编辑模式。

### 2.2 顶栏全局状态胶囊

只显示**有值**的字段；无值显示 `—`（胶囊本身不隐藏，避免布局跳动）。胶囊不可编辑，点击跳到对应页面。

| 胶囊 | 数据来源 | 颜色规则 | 备注 |
|---|---|---|---|
| 当前模式 | `hardware.performance_mode` | 有值 accent 描边；无值 warn | 4 档：超级省电 / 静音 / 均衡 / 狂暴 |
| AC/DC | `hardware.power_source` | AC=ok，电池=warn，未知=muted | 插拔由 daemon 推送 |
| CPU 温度 | `telemetry.cpu_temperature_c` | <70 muted，70–89 warn，≥90 danger | 无值 `—` |
| GPU 温度 | 同上 | 同上 | NVML 未订阅时显示 `—`，不显示 0 |
| 风扇 | `cpu_fan_rpm` / `gpu_fan_rpm` | muted | 形如 `2180 / 2274` |
| 刷新率 | `hardware.display_refresh_hz` | 240=accent，60=muted | `240 Hz` |
| 连接 | `status` + 管道状态 | live=ok，demo=warn，断连=danger | 断连时顶部 Banner 同时出现 |

**胶囊规格**：高 **22 px**、圆角 11 px、左右 padding 7/8 px、图标 14 px（2 px 线宽）、标签 10 px、值 11 px/600 等宽；不使用彩色实底，只用 14% 语义底 + 1 px 32% 语义描边 + 语义色文字。

### 2.3 八大页面信息架构

| # | 页面 | 用户要回答的问题 | 核心内容 | 主要 IPC |
|---|---|---|---|---|
| 1 | 总览 Dashboard | 「我现在是什么状态？最常用的开关在哪？」 | 硬件概览条（5 瓦片）+ 2–3 列卡片栅格 + 遥测趋势 + 本机状态 | `GetSnapshot`、`Control`、`SetSubscriptions` |
| 2 | 性能与风扇 | 「为什么风扇这么吵/这么热？曲线怎么调？」 | 模式分段 + Cooler Boost + **双画布**（CPU/GPU 6 点曲线）+ 6 组数值微调 + RPM/占空比读数 | `Control{setPerformanceMode/setFanCurve/setCoolerBoost}` |
| 3 | 显卡与显示 | 「现在是独显直连吗？刷新率/亮度/色彩怎么调？」 | MUX 三态时间线、刷新率、亮度、L1 Windows 色彩配置、L2 未验证、面板事实 | `Control{setDisplayRefreshRate/setDisplayBrightness/setColorProfile}` |
| 4 | 电池与电源 | 「电池健康如何？充电上限安全吗？」 | 电量大字 + 20 段条、充电上限离散档、健康度（`—`）、电源计划深链 | `Control{setChargeLimit}` |
| 5 | 键盘灯效 | 「每个键区现在是什么颜色？怎么画？」 | 6×4 逻辑矩阵（可拖选）、亮度 0–3、预设、设备/提交状态、冲突提示 | `Control{setKeyboardLighting}` |
| 6 | 自动化 | 「为什么规则没生效？怎么试跑？」 | 规则卡片流 + 三列诊断 + DryRun + 模板入口 | 待扩展 `automation.*` |
| 7 | 监控与 OSD | 「温度/风扇趋势？OSD 怎么配？」 | 大趋势图、最近采样表、OSD 配置、GPU 遥测开关、准星只读参数 | `SetSubscriptions{telemetry/osd}` |
| 8 | 系统与工具 | 「daemon 健康吗？官软冲突怎么处理？」 | 版本/身份/EC 状态、诊断包、逐项停用器 + 全部还原 | `GetConfig`/`SetConfig` + 待扩展 `disabler.*` |

### 2.4 页面高度契约（消除大空洞的强制公式）

```
chrome-h = topbar(40) + 2×divider(1) + footer(22) + (banner-visible ? 40 : 0)   // = 64 / 104
body-h   = window.height − chrome-h − 2×page-pad(12) − page-header(26) − gap(8)
body-w   = window.width  − nav-width(168 | 46) − divider(1) − 2×page-pad(12)
```

- 每个页面实例被显式赋予 `width: body-w; height: body-h`（`ui/app.slint`），因此页面内部布局**始终有确定高度**。
- 页面内「填满剩余」的行必须用显式高度：
  `fill-h = root.height − Σ(固定行高度) − n×gap`。
  **不要**依赖 `vertical-stretch` 单独完成填充——Slint 1.19 在部分嵌套下会退化为按 preferred 高度排版，实测会出现 200 px 空白带。
- `--check-layout` 会打印全部尺寸常量，便于机械核对。

---

## 3. Design Tokens

> 所有颜色以 **sRGB** 表示；透明度用 8 位 alpha 前缀 `#AARRGGBB`。Mica 是动态基底，**不要**把基底色写死到卡片上。

### 3.1 颜色令牌（深色主题）

| Token | 值 | 用途 |
|---|---|---|
| `bg` | `transparent`（Mica 可用）／`#1C1C1C`（回退） | 窗口基底 |
| `topbar` | `#D91A1A1A` ／ `#1A1A1A` | 顶栏 |
| `nav` | `#EB191919` ／ `#191919` | 左导航 |
| `content` | `#F7202020` ／ `#202020` | 内容区 |
| `card` | `#262626` | 卡片（不透明，保证任意壁纸下可读） |
| `card-hover` / `card-pressed` | `#2D2D2D` / `#323232` | 悬停 / 按下 |
| `inset` | `#1D1D1D` | 画布、指标瓦片内底 |
| `stroke` / `stroke-strong` | `#343434` / `#4A4A4A` | 1 px 描边 |
| `divider` / `grid` | `#2C2C2C` / `#2F2F2F` | 分隔线 / 网格线 |
| `text` / `text-2` / `text-3` / `text-disabled` | `#F2F2F2` / `#ABABAB` / `#7C7C7C` / `#5C5C5C` | 文字层级 |
| `accent` / `accent-hover` / `accent-pressed` | `#4CC2FF` / `#6FD0FF` / `#3AA9E0` | 主强调 |
| `on-accent` | `#07161E` | 强调上的文字 |
| `accent-surface` | `#4CC2FF24` | 选中底 |
| `ok` / `warn` / `danger` / `info` | `#6CCB5F` / `#FFC83D` / `#FF6B6B` / `#4CC2FF` | 语义色 |
| `ok-surface` 等 | 14% 语义色 | 语义底 |
| `muted-surface` | `#FFFFFF0A` | 中性底 |

### 3.2 颜色令牌（浅色主题）

| Token | 值 |
|---|---|
| `bg` | `transparent` ／ `#F3F3F3` |
| `topbar` / `nav` / `content` | `#F2F7F7F7` / `#EBEFEFEF` / `#F7F7F7F7` |
| `card` / `card-hover` / `card-pressed` | `#FFFFFF` / `#F6F6F6` / `#EFEFEF` |
| `inset` | `#F2F2F2` |
| `stroke` / `stroke-strong` / `divider` / `grid` | `#E2E2E2` / `#C6C6C6` / `#EAEAEA` / `#E6E6E6` |
| `text` / `text-2` / `text-3` / `text-disabled` | `#1A1A1A` / `#5A5A5A` / `#8A8A8A` / `#AFAFAF` |
| `accent` / `accent-hover` / `accent-pressed` | `#0F6CBD` / `#1A7FD4` / `#0B5A9E` |
| `on-accent` | `#FFFFFF` |
| `ok` / `warn` / `danger` / `info` | `#107C10` / `#9D5D00` / `#C42B1C` / `#0F6CBD` |

**对比度**（深色）：`#F2F2F2` on `#262626` ≈ 12.6:1；`text-2` ≈ 6.4:1；`text-3` ≈ 3.4:1（仅 ≥10 px 非关键信息）；`#4CC2FF` on `#262626` ≈ 8.0:1。浅色：`#1A1A1A` on `#FFFFFF` ≈ 16.1:1；`#0F6CBD` on `#FFFFFF` ≈ 5.6:1。

### 3.3 Mica 分层规则

1. **基底**：DWM 绘制（Mica / Mica Alt），UI 不绘制任何不透明全屏背景。
2. **叠层：只有一层**。顶栏与内容区**不透明**（`#1A1A1A` / `#202020`，浅色 `#F7F7F7`），只有导航轨以 92% alpha 叠在 Mica 上（与 Windows 11 设置一致）。实测：低 alpha 叠层会把整窗染成壁纸色（青绿），既不原生也不专业；多层透明叠透明禁止。
3. **卡片**：完全不透明，保证任意壁纸下可读；卡片内不再套半透明卡片超过 2 层。
4. **边框**：1 px 低对比描边保证卡片在任意壁纸上可辨；**禁止用阴影代替边框**。
5. **Mica 不可用时**：`Theme.mica = false` → 顶栏/导航/内容区全部转为不透明实底（`#1A1A1A`/`#191919`/`#202020` 或浅色对应值），绝不留下一个半透明的空窗。

### 3.4 字体与排版

| 角色 | 字号/行高 | 字重 | 字体 | 用途 |
|---|---|---|---|---|
| PageTitle | 20/26 | 600 | Segoe UI Variable Display | 页面标题 |
| H2 | 16/22 | 600 | Segoe UI Variable Text | 卡片组标题 |
| Body | 13/20 | 400 | Segoe UI Variable Text | 正文 / 控件标题 |
| BodyStrong | 13/20 | 600 | Segoe UI Variable Text | 卡片主标题 |
| Caption | 11/16 | 400 | Segoe UI Variable Text | 副标题 / 说明 |
| CaptionStrong | 11/16 | 600 | Segoe UI Variable Text | 胶囊 / 表头 |
| Micro | 10/14 | 400 | Segoe UI Variable Text | 时间戳 / 角标 / 轴标签（轴标签可 9 px） |
| Metric | 22/26 | 600 | Cascadia Mono | 指标瓦片数值 |
| NumericSmall | 11/16 | 600 | Cascadia Mono | 表格数字 / 曲线读数 |

- 中文回退：`Microsoft YaHei UI`（Slint 1.19 / fontique 实测中文正常）。
- Slint 的 `font-family` 只接受单一族名（实测逗号列表未生效）；中文回退交给 fontique。
- 数字列右对齐 + 等宽；单位与数值分离（数值 22/600，单位 10/400 tertiary）。
- 卡片副标题**单行省略**（`overflow: elide`，不换行），避免 66 px 卡片内出现半截第二行。

### 3.5 紧凑布局尺寸表（**normative**）

| 项 | 值 | Token | 说明 |
|---|---|---|---|
| 窗口默认 | 1080×700 | — | `AppWindow.width/height` |
| 窗口最小 | 960×620 | `MIN_WINDOW` | Win32 双重钳制，实测生效 |
| 顶栏高 | 40 | `topbar-h` | |
| 状态胶囊高 | 22 | `pill-h` | 圆角 11 |
| 左导航宽 | 168 / 46 | `nav-w` / `nav-w-collapsed` | 紧凑 46 = 图标 18 + padding 7×2 + 圆角 |
| 导航项高 | 32 | `nav-item-h` | |
| 内容区边距 | 12 | `page-pad` | 四边 |
| 卡片间距 | 8 | `gap` | 栅格列间距与行间距 |
| 紧凑间距 | 4 | `gap-sm` | 控件内间距 |
| 紧凑卡片高 | 58 / 66 | `card-min-h` / `card-max-h` | 紧凑 / 标准 |
| 控件高 | 32 | `control-h` | Fluent `std-widgets` 的最小高度 |
| 行高（KV） | 19 | `row-h` | |
| 卡片圆角 | 6 | `radius-card` | 控件 5，小元素 3，对话框 10 |
| 卡片内边距 | 8×10 | — | 上下 8 / 左右 10 |
| 页脚高 | 22 | — | |
| Banner 高 | 40 | — | 含 4 px 上下留白 |

### 3.6 动效

| 场景 | 时长 | 缓动 | 说明 |
|---|---|---|---|
| 悬停 / 焦点 | 80 ms | ease-out | 背景 / 描边 |
| 开关 / 分段 | 120 ms | ease-out | 位移 + 颜色 |
| 卡片展开 | 160 ms | ease-out | 高度 / 透明度 |

- **减少动效**：读取 `SPI_GETCLIENTAREAANIMATION`（`SystemParametersInfoW`）；关闭时 `Theme.reduced-motion = true`，所有 `duration` 归 0。
- 禁止：整页视差、持续发光、无限旋转、大面积 backdrop blur、任何装饰性脉冲。

### 3.7 图标规范（**normative**）

| 项 | 值 |
|---|---|
| 字形来源 | 手写 Fluent System Icons 风格描边路径（不复制任何 GPL 资源） |
| viewbox | 20 × 20 |
| 线宽 | 16 px 图标 → **2 px**；20 px 图标 → **1.5 px**；14 px 胶囊图标 → 2 px |
| 端点 / 连接 | `stroke-line-cap: round`、`stroke-line-join: round` |
| 填充 | `fill: transparent`（纯描边） |
| 颜色 | 继承 `Theme`（`text-2` 默认，选中 `accent`，语义 `ok/warn/danger`） |
| 数量 | 33 个字形（dashboard / gauge / display / battery / keyboard / lighting / bolt / pulse / sliders / chevron-×4 / check / close / warning / info / refresh / power / shield / plus / minus / play / eye / lock / chip / clock / thermometer / layers / menu / sidebar / dot / external / trash / restore） |
| 实现 | 单一 `Path` + `Glyphs.commands(name)` 纯函数，**不**为每个图标实例化一个 Path |

---

## 4. 组件库（Fluent 2 / Slint）

### 4.1 标准组件复用表（**normative：禁止重造**）

| 需求 | 复用组件 | 来自 | 用法约束 |
|---|---|---|---|
| 主/次按钮 | `Button` | `std-widgets.slint` | `primary: true` 仅用于「应用/提交」；`checkable: true` 用于分段 |
| 分段选择 | `SegmentBar`（薄封装 4 个 `Button{checkable}`） | `components/primitives.slint` | 受控：`current` 是 `in` 属性，避免内部状态与快照打架 |
| 开关 | `Switch` | `std-widgets.slint` | 用于布尔型能力（Cooler Boost / OSD / 引擎总开关） |
| 离散多选 | `ComboBox` | `std-widgets.slint` | 充电上限（60/80/100）、Windows 色彩配置 |
| 连续值 | `Slider` | `std-widgets.slint` | 亮度；`released` 才提交 |
| 数值微调 | `SpinBox` | `std-widgets.slint` | OSD 时长、规则优先级 |
| 文本输入 | `LineEdit` | `std-widgets.slint` | 规则名称 |
| 长列表 | `ScrollView` | `std-widgets.slint` | 规则列表、采样表、停用器 |
| 进度 | `ProgressIndicator` | `std-widgets.slint` | 预留（批量应用时用） |
| 复选框 | `CheckBox` | `std-widgets.slint` | 停用器逐项选择（Phase 2） |
| 配色 / 字号 | `Palette` / `StyleMetrics` | `std-widgets.slint` | `Palette.color-scheme` 由 `Theme.apply()` 写入 |

> 结论：**没有一行自研的基础控件代码**（开关、滑块、下拉、数字框、输入框、滚动、按钮全部来自官方 Fluent 样式）。

### 4.2 领域专属组件（标准库无等价物，才自研）

| 组件 | 文件 | 尺寸 | 职责 |
|---|---|---|---|
| `CompactCard` | `components/compact_card.slint` | 58/66 × 自适应 | 图标盘 + 标题 + 副标题 + `@children` 控件槽；`activatable` 时整卡可点 |
| `StatusPill` | `components/status_pill.slint` | 22 高 | 顶栏只读胶囊，5 级语义色 |
| `Banner` | `components/banner.slint` | 34 高 | 页面级持久状态（断连/只读/冲突/恢复必需/待重启），带 3 px 语义竖条 |
| `FanCurveEditor` | `components/fan_curve.slint` | 自适应（min 250 高） | 6 点曲线画布 + 红线安全区 + 出厂虚线 + 工作点 + 数值微调 |
| `ZoneMatrix` | `components/zone_matrix.slint` | 6×4 | 24 区矩阵 + 选中态 + 全选/清空 |
| `RuleCard` | `components/rule_card.slint` | 自适应 | 规则卡 + 三列诊断 + 启停 |
| `Sparkline` | `components/sparkline.slint` | 自适应 | 遥测趋势（Rust 生成 polyline，两条 `Path`） |
| `primitives.slint` | 同上 | — | `SectionLabel` / `MetricTile` / `KeyValue` / `SegmentBar` / `NoteRow` / `IconButton` / `Rule` / `Chip` |

### 4.3 `CompactCard` 结构

```
┌──────────────────────────────────────────────────────────────┐
│ [26×26 图标盘]  主标题 13/600                  [页面控件…]    │
│                 副标题 11/400 单行省略                        │
└──────────────────────────────────────────────────────────────┘
```

- 结构：`Rectangle(card)` → 先声明 `TouchArea`（保证控件在其上层）→ `HorizontalLayout(padding 8×10, spacing 9)` → 图标盘 → `VerticalLayout(标题/副标题)` → `@children`。
- 悬停：`card-hover` + `stroke`；`level` 决定描边（1=warn、2=danger）。
- `pending`：标题右侧出现 12 px「正在应用…」，禁止重复点击。
- `disabled`：整体降饱和，副标题替换为禁用原因。

### 4.4 `Banner` 文案契约

必须包含「发生了什么 + 影响 + 下一步」，例如：

- 「EC 写入已锁定：当前固件 `17S3EMS1.107` 的映射尚未通过写入验收。曲线可预览，暂不能应用。」
- 「检测到官软占用：`LEDKeeper2` 可能占用键盘 HID。灯效预览可用，实际写入需先停用官软。」
- 「daemon 未连接：正在重连 `\\.\pipe\MSIToolkit-IPC-v1`；控件已禁用，最后一份快照保留但不再刷新。」+ 动作「立即重连」

### 4.5 导航、页头、页脚

- 导航项：32 px 高、左 padding 9 / 右 6、图标 18 + 文字 12；选中 = `accent-surface` + 左侧 3×16 accent 条；悬停 = `card-hover`。
- 页头：标题 20/600 + 一行说明 12/400 `text-3`；右侧显示「有未提交修改」与「只读演示快照」角标。
- 页脚：10 px `text-3`，显示连接点 + 连接状态 + `IPC v1 · 快照 rN · 遥测 1 Hz` + 「关窗即退出进程 · 无托盘 · 无后台线程」。

---

### 4.6 能力门与 v1 缺口标注（**normative**）

1. **能力门是逐域的**。可用性 = `capabilities ∩ writable` 的对应位，逐项计算：
   `cap-performance / cap-fan / cap-cooler / cap-charge / cap-refresh / cap-brightness / cap-color / cap-keyboard / cap-mux`。
   `ecWriteStatus` **只**解释 EC 域：它只能门住风扇曲线的 `应用`（`can-apply`），**不得**连带禁用刷新率、亮度、色彩配置或灯效。
2. **`ecWriteStatus != enabled` 时曲线仍可本地编辑**：画布为「仅预览」，`应用` 禁用，顶部 Banner 给原因。这是「本地预览、一次提交」的必然结果。
3. **v1 没有的命令不得出现在 UI**：MUX 写、`Cancel`、`StartDaemon`、`automation.*`、`disabler.*`、`OsdPreview`、主动 NVML 订阅。对应位置只能渲染**只读事实**或带「待端点」Chip 的本地草稿。
4. **缺口必须可见**：任何依赖未冻结端点的区域，标题旁必须有 `Chip { text: "… 待 … 端点"; level: 1; }`，而不是只在点击后弹一个提示。
5. **恢复出厂只重置本地草稿**：不发送任何 IPC，也不等价于 `floor`。只有已冻结的、独立命名的预设才能生成草稿。

---

## 5. 页面级设计

### 5.1 总览 Dashboard

**目标**：5 秒内回答「现在什么状态、最常用开关在哪、有没有异常」。

- 顶部硬件概览条：5 个 `MetricTile`（CPU 温度 / GPU 温度 / CPU 风扇 / GPU 风扇 / GPU 功耗）；宽窗（≥1200 px）显示 5 个，窄窗隐藏第 5 个。
- 卡片栅格：2 列（默认）/ 3 列（`columns = 3` 时追加只读事实列）。
  - 列 1：性能模式（`SegmentBar` 4 档）、电池养护（`ComboBox` 离散档）、刷新率（`SegmentBar`）、屏幕亮度（`Slider`）。
  - 列 2：Cooler Boost（`Switch`）、显卡模式 MUX（**只读** + 「v1 无切换命令」）、Windows 色彩配置（`ComboBox`）、键盘灯效（可点跳转）。
  - 列 3（宽窗）：硬件身份、写入安全、自动化摘要、会话。
- 底部行（`fill-h`）：遥测趋势（2/3 宽）+ 本机状态（1/3 宽，8 行 `KeyValue`）。
- 空状态：daemon 未连接 → Banner + 全灰控件，**不**显示 0。

### 5.2 性能与风扇

- 顶部：模式 `SegmentBar`（3 份宽度）+ Cooler Boost `Switch`（1 份宽度）。
- 预设行 32 px：`出厂` / `静音` / `性能` + 「有未提交的本地修改」提示。
- 主体：**双画布并排**（CPU / GPU），每块包含
  - 头部 32 px：标题 + 「可编辑/只读」Chip + 「58 °C → 44% → 2180 RPM」+ `恢复` / `应用`；
  - 画布：6 个固定温度节点、Y 轴 0–150%、出厂曲线虚线（`stroke-dash-array`）、floor 以下红色安全区、accent 预览曲线、当前温度竖线 + 交点圆点；
  - 数值微调行：6 组 `− 占空比 +`（首点锁定禁用）。
- 底部：6 个 `MetricTile`（CPU/GPU 转速、CPU/GPU EC 占空比、CPU/GPU 温度）+ 一行口径说明。
- 状态：`ec_write_status != Enabled` → 画布仍可编辑预览，但 `应用` 禁用并给出原因；`pending` → 锁定 + 「应用中…」；`RecoveryRequired` → 只读 + danger Banner。

### 5.3 显卡与显示

- MUX 卡（1/2 宽）：三枚 Chip（当前 / 已请求 / 待重启）+ 说明 + 「只读 · v1 无写命令」Chip。**v1 没有 MUX 写命令，任何 `GPU_MUX` 能力位都不得解锁写入入口。**
- 刷新率 + 亮度卡（1/2 宽）：`SegmentBar` 60/240 + `Slider` 0–100 + 「WMI 亮度不可用时控件禁用，不显示 0」。
- 底部（`fill-h`）：L1/L2 色彩配置卡（`ComboBox` + 「未验证」Chip + 两条口径说明）+ 面板与外接屏事实卡。

### 5.4 电池与电源

- 电量卡（2/3 宽）：42 px 等宽大字 + AC/DC Chip + 充电上限 + 20 段电池条。
- 充电上限卡（1/3 宽）：`ComboBox`（充满 100% / 80% / 60%）+ 说明。
- 指标行：设计容量 / 满充容量 / 循环次数 / 电池健康（缺失全部 `—`）+ GPU 功耗。
- 底部（`fill-h`）：电池明细（含 3 条口径 `NoteRow`）+ 电源计划深链。

### 5.5 键盘灯效

- 左（2 份宽）：`ZoneMatrix`（6×4，单元格 34 px 起，可拉伸填满）+ 「布局待验证」Chip + 全选/清空 + 「N / 24 选中」+ 软件亮度说明。
- 右（1 份宽）：亮度 `SegmentBar`（关/低/中/高，76 px 卡）、预设 4 个 `Button`（76 px 卡）、设备与提交卡（`fill-h`：设备/布局/效果/提交状态 + 「应用 24 区颜色」+ 冲突提示）。
- 冲突：`LEDKeeper2` / `Mystic_Light_Service` 占用时整卡只读 + danger 描边 + 跳转停用器。

### 5.6 自动化

- 头部卡 66 px：引擎名 + 摘要 + `Switch` + 「试跑 (DryRun)」（固定 132 px 宽）。
- 条件式 DryRun 结果条 30 px。
- 左（2 份宽，`fill-h`）：规则 `ScrollView`，每张 `RuleCard` 显示名称/优先级/结果胶囊/启停/Trigger/Actions/**三列诊断**/最近触发。
- 右（1 份宽）：诊断口径卡（三条口径 + 「字段枚举来自 daemon schema」）+ 规则模板卡（`LineEdit` 名称 + `SpinBox` 优先级 + 模板提示 + 「新建规则」）。

### 5.7 监控与 OSD

- 顶部：采样卡（窗口 `SegmentBar` 1/5/15 分钟 + 暂停）+ GPU 遥测卡（`Switch`）。
- 趋势卡 190 px：`Sparkline`（两条 `Path`，缺值断线）+ CPU/GPU Chip。
- 指标行 5 个 `MetricTile`。
- 底部（`fill-h`）：最近采样表（`ScrollView`，6 列，40 行上限）+ OSD 卡（`Switch` + `SpinBox` 时长 + 准星只读参数 2 行）。

### 5.8 系统与工具

- 头部卡 66 px：产品名 + daemon/UI/IPC/快照版本行 + 4 枚 Chip（EC 写入、渲染后端、首帧 ms、Private KiB）+ 重新连接 / 导出诊断。
- 身份行 132 px：硬件身份、运行状态、诊断包说明（含 DWM 属性摘要，脱敏）。
- 停用器（`fill-h`）：表格列 `对象 / 类型 / 当前 / 目标 / 影响面 / 状态` + 「全部还原」+ 「`msiapcfg.dll` / `msiltcfg.dll` / `WmiAcpi` 明确保留」警示。

---

## 6. 高难度交互控件深度规范

### 6.1 风扇曲线画布（`FanCurveEditor`）

**数据契约**（来自 `FanCurve` / `FanCurveLimits`）：

- 6 个点；`temperature_c` 严格递增；`duty_percent` 非递减；首点固定（`fixed_anchor` 时不可改）；`max_duty_percent` 可能为 150；`floor` 是每个温度点的最低占空比。
- **温度节点不可拖**（HAL 明确「温度固定、首点不可拖、其余点不低于出厂占比且非递减」）。
- 本地拖动只预览；`应用` 才提交一次 `setFanCurve`；收到 `Applied` 快照后才替换预览。

**几何**：

```
X = pad-l(26) + (temp / 100) * plot-w
Y = pad-t(8)  + (1 − duty / maxDuty) * plot-h
plot-w = canvas.width  − 26 − 12
plot-h = canvas.height − 8  − 16
```

- 网格：水平 5 条（0/25/50/75/100 占 1 px `grid` 色）；垂直 6 条按温度节点；Y 轴标签 9 px 等宽右对齐。
- 安全区：floor 以下填充 `danger@10%`；floor 与预览曲线之间填充 `accent@10%`。
- 出厂曲线：`stroke-dash-array: [4px, 3px]`，`danger@55%`，1 px。
- 预览曲线：2 px accent（只读时 `text-3`），round cap/join。
- 工作点：当前温度竖线（`text@22%`）+ 8 px 交点圆点。
- 控制点：8 px 圆（anchor 为 2 px 方角），2 px accent 描边。
- 命中：**整画布单一 `TouchArea`**，按下时用 `nearest(mouse-x)` 选中最近节点，拖动时用 `mouse-y` 换算占空比。
  > 关键：不要给每个节点各自一个 `TouchArea`——节点会随拖动移动，`mouse-y` 变成相对移动目标的坐标，产生 runaway。

**校验与错误**（全部在 Rust `CurveEditor` 内，可单测）：

| 条件 | UI 反馈 |
|---|---|
| 低于 floor | 钳到 floor，InlineError「点 N 不能低于 X%（出厂安全下限）」 |
| 递减 / 越界 | 钳到相邻节点或上限，InlineError 说明原因 |
| anchor 首点 | 拒绝编辑，InlineError「首点是已验证锚点（0 °C / 0 %）」 |
| `ec_write_status != Enabled` | 画布可编辑预览，`应用` 禁用 + Banner 原因 |
| `pending` | 锁定 + 「应用中…」 |
| `RecoveryRequired` | 只读 + danger Banner，禁止「重试」 |

### 6.2 24 区键盘矩阵（`ZoneMatrix`）

- 布局：4 行 × 6 列 = 24 个逻辑区；单元格最小 34 px 高、间距 4、圆角 5；行内 `vertical-stretch` 让矩阵填满卡片。
- 数据：`[Rgb8; 24]`，索引 0–23；HAL 未确认物理行列映射，界面显示「布局待验证」Chip。
- 选择：单击独占选中；`Shift`/`Ctrl`（`PointerEvent.modifiers`）追加；「全选 / 清空」按钮；选中格右上角 6 px accent 圆点 + 2 px accent 描边。
- 亮度 0–3：**在 Rust 里做软件缩放**（`rgb × level / 3`），Slint 只索引已缩放的模型。
  > 关键：不要在 Slint 的 `pure function` 里读模型——Slint 1.19 不做依赖跟踪，返回值会静默变成 `transparent`（实测整片矩阵全白）。
- 提交：`应用` → 一次 `setKeyboardLighting`；状态文案「已提交（设备不回读）」，**不写「已生效」**。
- 冲突：官软占用时整卡只读 + Banner + 跳转停用器，不自动杀进程。

### 6.3 自动化规则卡片（`RuleCard`）

- 结构：名称 + 优先级 Chip + 结果 Chip + 启停 `Switch`；Trigger 行；Actions 行；**三列诊断行**；最近触发时间。
- 三列诊断是自动化页的核心：`未命中：` / `被抑制：` / `能力缺失：`，空值显示 `—`，颜色按严重度。
- 字段枚举必须来自 daemon schema，UI 不硬编码 `FieldId`。

### 6.4 Dashboard 栅格策略

- 列数由 `AppState.columns`（Rust 按窗口宽度写入 2 或 3）驱动；第三列为只读事实列。
- 卡片高度固定 58/66 px，不允许被拉伸——密度由列数而不是卡片高度调节。
- 遥测趋势与事实栏位于底部 `fill-h` 行，吸收全部剩余高度。

---

## 7. Slint 工程结构（已交付）

### 7.1 文件布局（`crates/msilt-ui`）

```
crates/msilt-ui/
├─ build.rs                     # fluent-dark 样式 + ui/app.slint 编译
├─ Cargo.toml                   # slint(software+winit) / windows(DWM) / msilt-core
├─ ui/
│  ├─ app.slint                 # 40px 顶栏 + 168px 导航 + 页面容器 + 22px 页脚
│  ├─ theme.slint               # Theme 全局（颜色/尺寸/字体/动效 + apply()）
│  ├─ icons.slint               # Glyphs 纯函数 + Icon 组件（33 个字形）
│  ├─ state.slint               # AppState / Fmt / 记录类型 / command 契约
│  ├─ components/
│  │  ├─ primitives.slint       # SectionLabel/MetricTile/KeyValue/SegmentBar/…
│  │  ├─ compact_card.slint
│  │  ├─ status_pill.slint
│  │  ├─ banner.slint
│  │  ├─ fan_curve.slint
│  │  ├─ zone_matrix.slint
│  │  ├─ rule_card.slint
│  │  └─ sparkline.slint
│  └─ pages/
│     ├─ dashboard.slint  performance.slint  display.slint  battery.slint
│     └─ lighting.slint   automation.slint   monitor.slint  system.slint
├─ src/
│  ├─ main.rs                   # 事件循环 / 回调 / 重连 / --demo / --check-layout
│  ├─ dwm.rs                    # Win11 DWM + 最小尺寸 + Private 内存读取
│  ├─ ipc_client.rs             # 命名管道客户端（读线程 + 唤醒，无轮询）
│  ├─ editor.rs                 # CurveEditor / ZoneEditor（纯逻辑，10 个单测）
│  ├─ view.rs                   # 快照 → AppState 投影 + 趋势路径 + Banner 文案
│  └─ demo.rs                   # MS-17S3 出厂参数快照 + 合成遥测
└─ tools/
   ├─ capture.ps1               # 启动 → 截图 → WM_CLOSE → 断言 0 残留
   └─ check-min-size.ps1        # 断言 960×620 最小尺寸被钳制
```

### 7.2 全局单例（Slint globals）

| Global | 内容 | 写入方 |
|---|---|---|
| `Theme` | 颜色、字号、尺寸、圆角、动效、`dark`/`mica`/`reduced-motion` | Rust（系统主题 / DWM 回读） |
| `AppState` | 连接、能力门、硬件值、曲线/矩阵/规则模型、Banner、页面 | Rust（IPC）+ Slint（`page`/`nav-collapsed`） |
| `Fmt` | `n/pct/celsius/rpm/hz/watts/temp-level/mode-name/...` | 编译期纯函数 |
| `Glyphs` | `commands(name)` 图标表 | 编译期纯函数 |
| `Palette`（std） | Fluent 控件配色 | `Theme.apply()` |

- 颜色只在 `Theme` 定义；组件禁止裸 hex（除调试）。
- 回调统一为 4 个入口：`command(kind,a,b)` / `command-text(kind,string)` / `curve-edit(channel,index,duty)` / `zone-edit(index,additive)`，命令码集中在 `src/main.rs::cmd`。
- `Fmt` 是「零假值」的唯一实现点：`v < 0 → "—"`。

### 7.3 模型与更新策略

- `SystemSnapshot` → `view::apply_snapshot` → `AppState` 标量/模型。只应用 `revision >= current` 的快照。
- 遥测：`VecDeque<Option<i16>>` 40 点；**趋势折线在 Rust 里生成**（0..1000 viewbox 的 `M/L` 串），Slint 侧只有两个 `Path`。缺值开始新的 `M`（断线），不插值。
- 24 区：`VecModel<Color>` 24 项，亮度在 Rust 缩放；选中是 `VecModel<bool>` 24 项。
- 曲线：`VecModel<CurvePoint>` 6 项，拖动时整表替换（6 项，代价可忽略）。
- 图标：单一 `Path` + 字符串 `commands`，不为每个字形实例化元素。

### 7.4 Slint 语言与布局陷阱（本机实测，全部已规避）

| 陷阱 | 现象 | 正确做法 |
|---|---|---|
| `Path` 默认 `fit: contain` | 图表被 letterbox | `fit: fill` + `viewbox-width/height` |
| `length` 拼进字符串 | `Cannot convert length to string` | 先 `/ 1px` 转成普通数 |
| `viewbox-width: canvas.width` | `Cannot convert length to float` | `viewbox-width: root.n(canvas.width)` |
| 布局双值 `padding: 8px 10px` | 语法错误 | 拆成 `padding-left/right/top/bottom` |
| `mouse-cursor: pointer` | 解析错误 | `MouseCursor.pointer` |
| 位运算 `& \| <<` | 不支持 | 用 Rust 位掩码 + 模型 |
| `%` 取模 | 不支持 | `Math.mod(x, y)` |
| `color(r,g,b)` 构造 | 未知标识符 | `rgb(r:i32, g:i32, b:i32, a:float)` |
| `pure function` 读模型 | 静默返回 `transparent` | 索引写在元素绑定里，或改在 Rust 计算 |
| 元素属性与函数同名 | 编译失败 | 函数改名（`zone-fill` / `zone-is-selected`） |
| 单行元素最后属性漏 `;` | 解析错误 | 每个属性都以 `;` 结尾 |
| 只靠 `vertical-stretch` 填满 | 页面底部留 200 px 空白 | 显式 `height: fill-h` |
| `width` + `min-width` 同时写在 `Window` | 编译错误 | 最小尺寸走 Win32 钳制 |
| 拖动时用会移动的元素做坐标基准 | 拖拽 runaway | 用固定画布 `TouchArea` + `pressed-x` |

### 7.5 线程模型

- UI 主线程：Slint 事件循环 + 渲染；禁止阻塞 >16 ms。
- IPC 读线程：阻塞读管道；`decode_frame`（共享 codec）→ 入队 → `invoke_from_event_loop` 唤醒主线程。**没有轮询**。
- 连接/重连：`Timer::single_shot` 退避 0/200/400/800/1600/5000 ms，最多 6 次。
- 写请求：UI 主线程串行发送，`id` 原子自增；`Control` 携带 `expected_revision`。
- 退出：`on_close_requested` → 关闭 IPC → `quit_event_loop` → 进程退出（读线程随进程结束，不 join，不阻塞）。

---

## 8. Windows 11 DWM Mica / 圆角 / 最小尺寸

### 8.1 实测结论（Windows 11 26200，150% DPI）

- Slint 的 winit 后端在 Windows 上**总是**以 `with_transparent(true)` 创建窗口；winit 用 `DwmEnableBlurBehindWindow`（空 region）实现透明，Slint 随后子类化 `WM_ERASEBKGND`，在渲染器可能使用透明时填黑，避免白色擦除。
- 客户区能否显示 Mica 取决于渲染表面是否保留 alpha：Software 与 FemtoVG 都保留，实测浅色客户区 `#F7F2EF`（Mica 成功）。
- 窗口扩展样式实测 `0x00040110`：**无** `WS_EX_LAYERED`、**无** `WS_EX_NOREDIRECTIONBITMAP`；不要自行添加。
- 属性顺序（全部 `hr=0`）：
  1. `DWMWA_USE_IMMERSIVE_DARK_MODE (20) = 1/0`
  2. `DWMWA_WINDOW_CORNER_PREFERENCE (33) = 2 (ROUND)`
  3. `DWMWA_BORDER_COLOR (34) = 0x00332A26`（深色）/ `0xFFFFFFFF`（浅色）
  4. `DWMWA_SYSTEMBACKDROP_TYPE (38) = 2 (Mica)`；`DwmGetWindowAttribute(38)` 回读 2 才算成功
  5. `DwmExtendFrameIntoClientArea(-1,-1,-1,-1)`
- 回读失败 → `Theme.mica = false` → 全不透明实底（干净回退，不留半透明空窗）。

### 8.2 无白屏闪烁协议

1. `AppWindow::new()` → 设置 `Theme.dark` → `Theme.apply()`；
2. `ui.show()`；
3. `Timer::single_shot(0ms)` → 取 `HWND`（`raw-window-handle 0.6`）→ 应用 §8.1 属性；HWND 尚未就绪时按 16 ms 重试，最多 8 次；
4. 客户区由 Slint 的 `WM_ERASEBKGND` 黑填充保护，不会白屏；残余风险是标题栏在 0–60 ms 内可能先按系统主题绘制（系统深色时不可见）。

**禁止**：`WS_EX_LAYERED` 透明、自己调 `DwmEnableBlurBehindWindow`、在 `show()` 之前假设 `window_handle()` 可用、把 Mica 失败当致命错误。

### 8.3 最小窗口尺寸（实测生效）

Slint 拒绝在根 `Window` 上同时写 `width` 与 `min-width`，因此最小尺寸由 Win32 承担：

- `SetWindowLongPtrW(GWLP_WNDPROC)` 子类化，`ORIGINAL_PROC` 存于 `AtomicIsize`；
- 处理 `WM_GETMINMAXINFO`（交互式拖拽）→ 写 `ptMinTrackSize`；
- 处理 `WM_WINDOWPOSCHANGING`（`MoveWindow` / `SetWindowPos`）→ 钳 `WINDOWPOS.cx/cy`；
- 物理值 = 逻辑值 × `scale_factor`。

实测：请求 500×360 → 实际 1440×930（= 960×620 @150%）。

### 8.4 主题跟随与回退

- 主题来源：`HKCU\...\Themes\Personalize\AppsUseLightTheme`（`RegGetValueW`，0 = 深色）；读取失败回退深色。
- 变更后：更新 `Theme` 全局 + `DwmSetWindowAttribute(20)` + `DWMWA_BORDER_COLOR`。
- 高对比度：禁用 Mica/透明，使用系统色，边框加粗到 2 px。

---

## 9. 性能预算与实测方法

### 9.1 预算

| 指标 | 目标 | 实测（release） | 结论 |
|---|---|---|---|
| 首帧（进程启动→窗口可见） | <200 ms | **101–118 ms** | 达标 |
| 开窗 Working Set | ≤45 MiB | **33.0 MiB** | 达标 |
| 开窗 Private | ≤45 MiB | **11.7–11.8 MiB** | 达标 |
| EXE | ≤12 MB | **8.75 MiB** | 达标 |
| GPU 初始化 | 0（不唤醒 dGPU） | **0** | 达标 |
| 关闭后残留 | 0 进程 / 0 显存 | **0** | 达标 |
| 冷编译 | ≤2 min | 61–88 s（release + fat LTO） | 达标 |

### 9.2 测量方法（可复跑）

1. **首帧**：进程启动时记录 `PROCESS_START`，在 DWM 回调（`show()` 之后的 0 ms 定时器）记录差值，显示在「系统与工具」页的 Chip 上。
2. **内存**：同一页面显示 `GetProcessMemoryInfo().PagefileUsage / 1024`（Private KiB）。
3. **截图 + 0 残留**：`pwsh -File crates/msilt-ui/tools/capture.ps1 -Exe target\release\msilt-ui.exe -AppArgs "--demo --page 1"`。
4. **最小尺寸**：`pwsh -File crates/msilt-ui/tools/check-min-size.ps1`。
5. **Mica 像素测试**：铺全屏纯白 WinForms 作背景，浅色模式截取客户区；亮色即合成成功。

### 9.3 渲染后端实测明细（本机）

测试环境：Windows 11 家庭版中文 10.0.26200，Core Ultra 9 275HX，RTX 5090 Laptop（32.0.16.1687），2560×1600@240 Hz，150% DPI，Rust 1.99.1，Slint 1.19.1。

| 后端 | 特征 | EXE | 首帧 | WS | Private | 动画 CPU | DWM notifier | Mica |
|---|---|---|---|---|---|---|---|---|
| `renderer-software`（精简） | winit + softbuffer | **8.75 MiB** | **101–118 ms** | **33.0 MiB** | **11.7–11.8 MiB** | 2.1% | `Err(Unsupported)` | ✅ |
| `renderer-software`（默认特性） | 含 accessibility 等 | 10.2 MB | 132 ms | 36.7 MiB | 13.1 MiB | — | `Err` | ✅ |
| `renderer-femtovg` | winit + glutin/OpenGL | 10.2 MB | 262 ms（冷 882 ms） | 94.6–157 MiB | 110–165 MiB | 4.9% | `Ok` + `RenderingSetup` | ✅ |
| `renderer-skia` | prebuilt | 16.3 MB | ~840 ms | 76–110 MiB | 114–156 MiB | — | `Ok` 但未回调 | 未验证 |
| `renderer-femtovg-wgpu` | DX12 | 13.4 MB | 1237 ms | 197–203 MiB | 198–205 MiB | — | `Ok` | ✅ |

关键观察：

- **FemtoVG 的 60–120 MiB 私有内存主要来自 OpenGL 驱动**（本机显示由 NVIDIA 直驱、无活动 iGPU）；软件渲染无此开销。
- **WGPU 最差**：DX12 设备/管线初始化 1.24 s，且会占用独显。
- **软件渲染唯一同时满足首屏 <200 ms 与 ≤45 MiB**，且 Mica 可用；代价是 `RenderingSetup` 不可用（DWM 延后 ~60 ms）与极复杂矢量场景的 CPU 成本（本 UI 的 Path 数量可控：单画布 ≤8 个 Path）。

**最终特性选择**：`default-features = false` + `std, compat-1-18, backend-winit, renderer-software, software-renderer-systemfonts, software-renderer-path, raw-window-handle-06`。不启用 `accessibility` / `system-tray`（UI 不需要托盘；无障碍待 Phase 2 单独评估）。

---

## 10. 可访问性、国际化、DPI

- 键盘：所有可交互元素可 Tab 到达；`Ctrl+1..8` 切页、`Ctrl+B` 折叠导航；`Enter/Space` 激活。
- 对比度：正文 ≥4.5:1；禁用态不依赖颜色单独表达（加文案）。
- 减少动效：`SPI_GETCLIENTAREAANIMATION=0` → `Theme.reduced-motion` → 所有 duration=0。
- DPI：Slint 逻辑像素自动缩放；150% 下 1080×700 → 1642×1106 物理；图标为矢量 `Path`。
- 国际化：Phase 1 仅 zh-CN；文案集中在各组件内联（Phase 2 抽到 `strings.slint`）；数字/单位不拼接。
- 中文断行：标题不换行（省略）；卡片副标题单行省略；按钮文字不换行。

---

## 11. 与 IPC / daemon 的契约映射

### 11.1 连接与订阅（已落地 v1）

```
连接 \\.\pipe\MSIToolkit-IPC-v1（退避 0/200/400/800/1600/5000 ms）
  → Hello{version:1, client:ui}            → 首帧快照
  → SetSubscriptions{state,telemetry,osd}  → 快照 + 推送
  → Control{expectedRevision, change}      → Applied{snapshot}
```

- 请求串行（至多 1 条在途），`id` 从 1 递增，`ttlMs` 默认 5000。
- 快照应用：只接受 `revision >= current`；`TelemetryTick` 用 `sampledAtUnixMs` 计算「N s 前」。
- 错误码 → 中文文案在 Rust 侧（`error_code_text`），Slint 只显示字符串。

### 11.2 UI 需要的 ViewModel 字段（现状 + 缺口）

| 区域 | 需要字段 | 现状 | 缺口/诉求 |
|---|---|---|---|
| 顶栏 | 模式、AC/DC、CPU/GPU 温度、风扇、刷新率、连接 | 已有 | 整机/电池功率（`TelemetrySample` 扩展） |
| 风扇 | 6 点曲线、limits、RPM、EC duty、当前温度 | `FanState`/`FanCurveLimits`/Telemetry 有 | 无（注意 RPM ≠ duty） |
| 键盘 | 24 区颜色、亮度 0–3、效果、冲突 | 仅 `keyboard_zones` | `KeyboardLightingState{brightness,effect,speed,deviceStatus,conflict}` |
| 显示 | 刷新率、支持列表、亮度、色彩配置、MUX | 有 | L2 面板色域能力（明确 Unsupported） |
| 电池 | 电量、上限、支持档、健康/循环 | 电量/上限/档有 | 健康度/循环/功率 |
| 自动化 | 规则、诊断、schema、dryrun、手动覆盖 | 仅 `AppConfig.automation` 开关 | `automation.list/schema/dryrun/diagnostics` |
| 系统 | 版本、EC 状态、停用器 | 状态有 | `disabler.list/apply/restore` + `ObservedState` |
| OSD | 配置、预览 | `AppConfig.osd` | 预览/位置/屏幕选择命令 |

### 11.3 控制意图映射

| UI 动作 | IPC | 备注 |
|---|---|---|
| 切模式 | `Control{... setPerformanceMode{mode}}` | 4 档，来自 `supported_performance_modes` |
| 应用风扇曲线 | `Control{... setFanCurve{channel,curve}}` | 一次提交；本地预览；回读后更新 |
| Cooler Boost | `Control{... setCoolerBoost{enabled}}` | 危险提示（噪音/寿命） |
| 充电上限 | `Control{... setChargeLimit{percent}}` | 只允许 `supported_charge_limits` |
| 刷新率 | `Control{... setDisplayRefreshRate{hz}}` | 只允许 `supported_refresh_rates` |
| 亮度 | `Control{... setDisplayBrightness{percent}}` | 0–100；回读 |
| 色彩 | `Control{... setColorProfile{profileId}}` | id 不是路径 |
| 键盘 | `Control{... setKeyboardLighting{zones}}` | 24×RGB；亮度软件缩放 |
| 手动覆盖释放 | `ReleaseManualOverride{expectedRevision,target}` | Phase 2 接线 |

### 11.4 UI 本地配置（`ui.json`）

- 路径：`%LOCALAPPDATA%\MSI-Toolkit\ui.json`（UI 独占，daemon 不解析）。
- 内容：`schemaVersion`、`cards[{id,visible,compact}]`、`columns`、`lastPage`、`navCollapsed`、`window{width,height}`、`reducedMotionOverride`；≤64 KiB；原子写。
- Phase 1 未启用持久化（`navCollapsed` 仅存内存）。

---

## 12. 验收清单

### P0（Phase 1 合并门禁）

- [x] 软件渲染默认构建：首帧 <200 ms（实测 101–118 ms），WS ≤45 MiB（33.0），Private ≤45 MiB（11.7–11.8），EXE 8.75 MiB。
- [x] 关闭窗口 800 ms 内进程消失；无托盘；无后台线程残留（`tools/capture.ps1` 断言）。
- [x] DWM 属性全部 `hr=0`；`DWMWA_SYSTEMBACKDROP_TYPE` 回读 2；Mica 不可用时回退实底。
- [x] 最小窗口 960×620 由 Win32 钳制（实测 500×360 → 1440×930）。
- [x] 全部通用控件来自 `std-widgets.slint`（§4.1 复用表）。
- [x] 图标全部为单色 `Path`，零 emoji（`ui/icons.slint` 33 个字形）。
- [x] `None` 一律渲染为 `—`（`Fmt` 唯一实现点，含单测）。
- [x] 风扇曲线：温度固定、首点锁定、非递减、≥floor、≤max；本地预览；一次提交；`pending/error` 状态正确（8 个单测）。
- [x] 24 区：6×4 矩阵、独占/追加选择、软件亮度、提交状态区分；官软冲突提示。
- [x] 自动化：三列诊断、DryRun 入口、schema 提示、无 OSD 窗口。
- [x] 停用器：逐项表格 + 全部还原 + 保留项标注；无单一「优化」按钮。
- [x] 8 个页面均无底部空白带（显式 `fill-h`）。
- [x] `cargo check` 0 error / 0 warning；`cargo test` 19 passed；`--check-layout` 0 项失败。

### P1（Phase 2）

- [ ] FemtoVG 可选模式（`--features femtovg`）。
- [ ] 自绘 Fluent 标题栏（`no-frame`）消除标题栏主题跳变。
- [ ] 动态灯效（仅设备端已验证效果）；UI 不推帧。
- [ ] 命令面板 `Ctrl+K`；en-US。
- [ ] `ui.json` 持久化 + Dashboard 自定义编辑模式。
- [ ] 无障碍特性开启与屏幕阅读器全流程。

### P2

- [ ] 高对比度主题完整适配。
- [ ] 主题自定义强调色（跟随系统 accent）。
- [ ] 多窗口？**不做**：单窗口 + 页面导航，避免内存翻倍。

---

## 13. 风险与开放问题

| # | 风险/问题 | 影响 | 缓解/待决 |
|---|---|---|---|
| R1 | 软件渲染无 `RenderingSetup`，DWM 在首帧后 ~60 ms 应用 | 标题栏可能短暂按系统主题绘制 | 系统深色时不可见；Phase 2 自绘标题栏 |
| R2 | 本机无活动 iGPU，任何 GL/Skia/WGPU 后端都会加载 NVIDIA 驱动并多 60–120 MiB | 违反 45 MiB 目标 | 默认软件渲染 |
| R3 | 24 区物理行列映射未验证 | UI 预览可能与实际键盘不一致 | 显示「布局待验证」；逐区截图确认后更新 |
| R4 | 键盘亮度/效果/速度无契约字段 | 无法实现完整灯效页 | 软件亮度 + 静态逐区颜色；请求 Core 扩展 |
| R5 | 自动化 IPC 未落地 | 自动化页只能展示静态规则与 DryRun 入口 | 与 Pi-Auto/Core 对齐 |
| R6 | 风扇曲线温度不可编辑，与「6 点可拖拽」直觉不同 | 用户可能尝试横拖 | 只允许 Y 拖动 + 首点锁定 + InlineError |
| R7 | 功率/电池健康字段缺失 | 电池页信息不完整 | 显示 `—`；扩展 TelemetrySample |
| R8 | Slint `font-family` 单族限制 | 中文/等宽字体无法同时指定 | 单族 + fontique 回退 |
| R9 | 高饱和壁纸下低 alpha 叠层会把整窗染色 | 观感不原生 | 内容区 alpha 提升到 97%；卡片不透明 |
| R10 | `pure function` 读模型静默返回 `transparent` | 曾导致整片矩阵空白 | 已改为 Rust 计算 + 元素内联索引（§7.4） |
| R11 | `vertical-stretch` 在嵌套下不稳定 | 曾出现 200 px 空白带 | 已改为显式 `fill-h`（§2.4） |

**待 Core/HAL/Auto 回答**：

1. `TelemetrySample` 功率/电池健康字段是否纳入 Phase 1？
2. 键盘效果/亮度/速度是否有设备端协议，还是只支持静态逐区 RGB？
3. 自动化 `schema/dryrun/diagnostics` 的命令名与载荷。
4. 停用器 IPC 的 `list/apply/restore` 与 `ObservedState` 形状。
5. `ui.json` 由 UI 独占是否可接受？
6. `ControlTarget::FanCurve` 的 `limits.floor` 是否始终给出 6 个固定温度？`fixed_anchor` 是否总为首点？
7. OSD/准星预览：daemon 是否提供预览命令，还是 UI 只读参数？
8. 系统主题变更事件：Slint/winit 是否已暴露，还是需要 UI 自建 message-only 窗口？

---

## 附录 A：实测原始数据与复跑路径

```powershell
# 构建（release，fat LTO）
cargo build --release -p msilt-ui          # 61-88 s

# 布局与交互自检（无窗口，退出码 0/1）
cargo run -p msilt-ui -- --check-layout

# 单元测试（纯逻辑，无窗口）
cargo test -p msilt-ui                     # 19 passed

# 截图 + 关窗即退验证（任意页面 0..7）
pwsh -File crates/msilt-ui/tools/capture.ps1 `
  -Exe target\release\msilt-ui.exe -AppArgs "--demo --page 1" -Out "$env:TEMP\p1.png"

# 最小尺寸验证
pwsh -File crates/msilt-ui/tools/check-min-size.ps1
```

实测输出（release，2026-11-27）：

```
exe         8.75 MiB
首帧        101-118 ms
Private     12004-12036 KiB (11.7-11.8 MiB)
WorkingSet  33.0 MiB
Threads     8
Handles     228
close       0 residual
min-size    500x360 → 1440x930 (960x620 @150%)
```

## 附录 B：DWM 实测日志摘录

```
dwm: hwnd=0x130D5A style=0x06CB0000 exstyle=0x00040110 (WS_EX_LAYERED=false WS_EX_NOREDIRECTIONBITMAP=false)
dwm: DWMWA_USE_IMMERSIVE_DARK_MODE(20)=1 -> hr=0x00000000
dwm: DWMWA_WINDOW_CORNER_PREFERENCE(33)=ROUND -> hr=0x00000000
dwm: DWMWA_BORDER_COLOR(34)=0x00332A26 -> hr=0x00000000
dwm: DWMWA_SYSTEMBACKDROP_TYPE(38)=2 -> hr=0x00000000; readback=2 (hr=0x00000000)
dwm: DwmExtendFrameIntoClientArea(-1,-1,-1,-1) -> hr=0x00000000
```

实现内诊断串（系统与工具页「诊断包」卡，脱敏）：

```
dwm: dark=false immersive=true rounded=true mica=true frame=true min-size=true animations=true
```

浅色 Mica 像素（白色全屏背景）：客户区 `(896,44)=#F9F2E8`、`(1752,598)=#858580`、`(896,1136)=#F7F2EF`；深色同位置 `#202020/#242422`。

## 附录 C：文件所有权

本规范与 `docs/collab/03-pi-ui.md` 由 `@Pi-UI` 独占写入；`crates/msilt-ui/**`（含 `ui/`、`src/`、`build.rs`、`Cargo.toml`、`tools/`）同属 `@Pi-UI`。根 `Cargo.toml`、`crates/msilt-core/*`、`crates/msilt-hal/*`、`crates/msilt-automation/*`、`docs/CONTRACTS.md`、`docs/HARDWARE_PROTOCOLS.md`、`docs/AUTOMATION_SPEC.md` 均由对应 owner 维护，本规范只读引用，不修改。

## 附录 D：实现与验证记录（2026-11-27）

| 项 | 结果 |
|---|---|
| `cargo check -p msilt-ui` | 0 error / 0 warning |
| `cargo test -p msilt-ui` | 19 passed / 0 failed |
| `cargo run -p msilt-ui -- --check-layout` | 通过（0 项失败） |
| `cargo build --release -p msilt-ui` | 61–88 s，EXE 8.75 MiB |
| 8 个页面渲染 | 全部通过（截图逐页核对，无空白带、无裁切） |
| 关窗即退 | 7 次连续截图运行均 0 残留 |
| 最小尺寸 | 钳制生效 |
| DWM | 6 项属性全部接受，Mica 回读 2 |

已修复的实测缺陷（保留记录，避免回归）：

1. `pure function` 读 `[color]` 模型 → 24 区矩阵整片空白（改为 Rust 缩放 + 元素内联索引）。
2. `vertical-stretch` 填充失败 → 显示/电池/自动化/监控/系统页底部 200 px 空白（改为显式 `fill-h`）。
3. 低 alpha Mica 叠层 → 高饱和壁纸把整窗染成青绿（内容区提升到 97%）。
4. 卡片副标题换行 → 66 px 卡片内出现半截第二行（改为单行省略）。
5. 画布头部 24 px 装 32 px 按钮 → 按钮溢出（头部改 32 px）。
6. 伸缩卡内 `Button`/`ComboBox`/`LineEdit` 被拉高 → 固定 `height: control-h`。

---

*本规范的所有数字均来自本机实测；`—` 表示未测/不适用，不表示 0。*
