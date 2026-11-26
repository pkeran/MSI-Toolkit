# Magpie 设计语言参考与对标改造蓝图（MAGPIE_STYLE_REFERENCE）

> 文档所有者：`@Pi-UI`　|　状态：v1.0（只读研究 + 可执行蓝图，未改动任何代码）　|　采集日期：2026-11-27
> 上游契约：`docs/UI_DESIGN_SPEC.md`（v1.1，三条用户硬标准）、`docs/ARCHITECTURE.md`、`docs/CONTRACTS.md`
> 下游实现：`crates/msilt-ui`（Slint 1.19.1 + `fluent` 样式 + `renderer-software` + `backend-winit` + Win11 DWM Mica）
> 参考对象：`https://github.com/Blinue/Magpie`（GPL-3.0，main 分支，`Microsoft.UI.Xaml 2.8.7` + XAML Islands + C++/WinRT）
> 许可证红线：**Magpie 为 GPL-3.0。本文只提取设计语言、数值参数、颜色 token 与交互范式（事实性数据），不复制任何源代码、XAML 模板、资源或图标。** 全部 Slint 实现独立编写。见附录 B。

---

## 0. 摘要

### 0.1 十条精髓（一句话一条）

1. **两级纵向节奏**：卡片之间 **2 px**，组与组之间 **32 px**（1:16）。Magpie 的"高级感"来自这个比值，不来自阴影或圆角。
2. **内容层不是卡片，是"层"**：`LayerFillColorDefault` 浮在 Mica 上（深色 `rgba(58,58,58,0.30)` → 合成后 `#282828`），与 Mica 导航栏只差 **8 个亮度级**，靠一条 **1 px 极细缝**分开。
3. **卡片是"三点式"横排**：图标 20 → 标题/副标题 → 右侧控件垂直居中，**恒 68 px 高**。纵排堆叠（我们的 `CompactCard`）是"松散感"的根源。
4. **圆角只有一个值**：`ControlCornerRadius = 4`。卡片 4、输入框 4、导航项 4、Expander 4；大浮层 8。没有 6/10 的中间档。
5. **描边是"内凹"的**：深色下卡片描边是 **黑色 10%**（`#19000000`），比卡片本身更暗 —— 卡片像被"压"进内容层，而不是浮起来。
6. **状态色替换，不是叠加**：hover/pressed 用 `ControlFillColor*` **替换**卡片底色（半透明刷子合成在内容层上），所以深色下 pressed（`#2F2F2F`）比静息卡片（`#333333`）**更暗**。
7. **动效三档，全部 ≤ 333 ms**：83 ms（按钮底色 `BrushTransition`，已核实）/ 167–333 ms（展开收起，已核实）/ 150 ms（导航选中条位移，**我们按 WinUI 通用曲线自定**，见 §5.1 第 3 行）。
8. **展开是"位移 + 淡出"两段式**：展开 `TranslateY −h → 0`，333 ms `cubic-bezier(0,0,0,1)`；收起先 48 ms 淡出再 167 ms 上移 `cubic-bezier(1,1,0,1)`。没有高度插值的抖动。
9. **导航即列表**：Magpie 的"左列列表 + 右列详情"是 `NavigationView` 的 pane（配置档案）+ 整页详情，**页内没有第二套双栏**。页内永远是单列 1000 px 版心。
10. **Mica 是背景，不是主角**：全窗只有 DWM 画的一层 Mica + 一层 `LayerFillColorDefault` + 卡片。禁止第二层透明叠透明。

### 0.2 勘误：七个流传但不准确的说法

| 流传说法 | Magpie 实测 | 证据 |
|---|---|---|
| 内容区是"左上 8 px 圆角 + 1 px 描边的浮层卡片" | **被显式覆盖为 0 圆角**，只保留 **1 px 左边缝** | `RootPage.xaml:57-58` |
| 卡片之间 3–4 px | **2 px**（`SettingsCardSpacing`） | `App.xaml:84` |
| `SettingsCard` 悬停 83 ms 过渡 | 83 ms 只存在于 `SubtleButtonStyle`；**卡片状态是瞬切**（`DiscreteObjectKeyFrame`，无 `BrushTransition`） | `App.xaml:141` vs `SettingsCard.Resource.xaml:236-260` |
| 缩放模式/配置页是"左列列表 + 右列详情" | 左栏导航即列表，右栏是**整页**详情；页内单列 1000 版心 | `ScalingModesPage.xaml:97`、`ProfilePage.xaml:135` |
| 强调色 `#60CDFF` 是硬编码常量 | 是 `SystemAccentColorLight2` 的**派生值**；系统默认蓝 `#0078D4` 时正好等于 `#60CDFF` | `Common_themeresources_any.xaml:125` |
| 卡片图标 16–20 px | 头部图标**上限 20**（`Viewbox` 缩放），行内标准图标 **14** | `SettingsCard.Resource.xaml:71`、`App.xaml:87` |
| 组标题 14 px、卡片标题 14 px | 组标题 = `BodyStrong`（14/600）；**卡片标题继承 `ControlContentThemeFontSize` = 14/400**，不是 SemiBold | `SettingsGroup.Resource.xaml:32`、`SettingsCard.Resource.xaml:102` |

### 0.3 最高杠杆改造（按 ROI 排序）

| # | 改造 | 触及文件 | 收益 | 风险 |
|---|---|---|---|---|
| 1 | 颜色阶梯换成 WinUI 原生（Mica → Layer → Card → hover/press） | `ui/theme.slint` | 全窗质感一次性提升；卡片与内容层出现真实层次 | 低（纯 token） |
| 2 | 两级节奏：组内 2 px / 组间 24 px + 组标题 14/600 | 全部 `pages/*.slint` | 直接消灭"散乱感、多余大留白" | 低（纯布局） |
| 3 | 新组件 `SettingsCard`（三点式横排 68 px）替代纵排 `CompactCard` | 新增 1 文件 + 8 页 | 单卡高 104 → 68（−35%），信息量不变，一屏多放 1–2 张卡 | 中（结构改动） |
| 4 | `SettingsExpander` 承载高级配置（规则明细、曲线预设、停用器） | 新增 1 文件 + `automation`/`performance` | 折叠态一屏看 8 条规则，展开看诊断 | 中 |
| 5 | 导航轨：3×16 accent 选中条 + 150 ms 位移 + 2 px 间距 | `ui/app.slint` + 新增 `nav_item` | 与 Win11 设置一致的"原生手感" | 低 |
| 6 | 语义色换成 WinUI `SystemFillColor*Background`（`#442726`/`#433519`/`#393D1B`） | `ui/theme.slint` | 状态条不再"荧光"，与系统一致 | 低 |

### 0.4 明确不照搬的四处

| 不照搬 | 原因 |
|---|---|
| 320 px 导航栏 | Magpie 放的是任意长度的配置文件名称；我们是 8 个 4 字中文标签，168 px 已足够。照搬会吃掉 152 px 内容宽。 |
| 40 px 页面左右边距 | Magpie 默认窗 980 宽、版心 1000、单列；我们 1080 宽且要放 2–3 列栅格。12 px 边距 + 组内边距是正确换算（见 §2.7）。 |
| 28 px 页面标题 | 那是"单页设置应用"的标题；我们 8 页、页头还要放说明与角标。保留 20/600。 |
| 页内双栏 Master-Detail | Magpie 没有；我们的自动化/灯效页需要，用 2:1 栅格 + `SettingsGroup` 实现（§3.6）。 |

---

## 1. 证据与采集

### 1.1 采集对象与版本

| 项 | 值 |
|---|---|
| 仓库 | `Blinue/Magpie`，`main` 分支，`codeload.github.com/Blinue/Magpie/zip/refs/heads/main`（8.24 MiB，解包后内存/临时目录读取，**未写入工作区**） |
| UI 框架 | `Microsoft.UI.Xaml 2.8.7`（WinUI 2.8，`XamlControlsResources ControlsResourcesVersion="Version2"`），C++/WinRT 3.0，XAML Islands（`DesktopWindowXamlSource`） |
| 主题来源 | `App.xaml` + `SettingsCard/SettingsGroup/SettingsExpander.Resource.xaml`（移植自 MIT 许可的 PowerToys / CommunityToolkit）+ WinUI 2.8 默认资源 |
| 窗口材质 | `DwmSetWindowAttribute(DWMWA_SYSTEMBACKDROP_TYPE, DWMSBT_MAINWINDOW)` = Mica；Win11 22H2+ 追加 `WS_EX_NOREDIRECTIONBITMAP`；XAML Island 背景置透明（`MainWindow.cpp:648-650`、`:568`、`:85`） |
| 默认窗口 | **980 × 690** dip，最小 500 × 300 dip（`MainWindow.cpp:602`、`:285-286`） |
| 颜色基准 | `microsoft-ui-xaml` `controls/dev/CommonStyles/Common_themeresources_any.xaml`（MIT），与 WinUI 2.8.7 同源 |

### 1.2 证据等级

| 等级 | 含义 | 标注方式 |
|---|---|---|
| **A** | Magpie 自身 XAML/CPP 可直接查证 | 行号 |
| **B** | Magpie 未覆盖、继承 WinUI 2.8.7 默认资源 | `WinUI 默认` |
| **C** | 由 A/B 计算得出的合成色、对比度、推导值 | 公式 |

本文所有"实测"字样仅用于 A/B 级；C 级一律标"合成/推导"。

### 1.3 复现

```powershell
$tmp = Join-Path $env:TEMP 'magpie-research'; New-Item -ItemType Directory -Force $tmp | Out-Null
$wc = New-Object Net.WebClient; $wc.Headers.Add('User-Agent','msilt-research')
[IO.File]::WriteAllBytes("$tmp\m.zip", $wc.DownloadData('https://codeload.github.com/Blinue/Magpie/zip/refs/heads/main'))
Add-Type -AssemblyName System.IO.Compression.FileSystem
[IO.Compression.ZipFile]::ExtractToDirectory("$tmp\m.zip", "$tmp\x")
# 关键文件
#   x/Magpie-main/src/Magpie/{RootPage,PageFrame,TitleBarControl,CaptionButtonsControl}.xaml
#   x/Magpie-main/src/Magpie/Settings{Card,Group,Expander}.Resource.xaml
#   x/Magpie-main/src/Magpie/{App.xaml,KeyVisual.Resource.xaml,ShortcutControl.xaml,BlueInfoBarStyle.xaml}
```

### 1.4 为什么它长得像"Windows 11 设置"

Magpie 没有自绘任何 Fluent 视觉：它用 WinUI 2.8 的 `NavigationView` / `SettingsCard` 家族 + DWM Mica，只做了 **三类微调**：

1. 把 `NavigationViewContentGridCornerRadius` 从 `8,0,0,0` 改成 `0`，边框从 `1,1,0,0` 改成 `1,0,0,0`（`RootPage.xaml:57-58`）→ 内容层从"圆角浮卡"变成"与窗口齐平的一层"。
2. 把 `SettingsCardSpacing` 从默认 4 改成 **2**（`App.xaml:84`）→ 组内更紧。
3. 收紧 `InfoBar` 内边距（`App.xaml:90-91`）→ 状态条不再"胖"。

**结论：Magpie 的"高级感"几乎全部来自 WinUI 原生 token 的正确使用与两级节奏，而不是自定义视觉。** 这对我们是好消息：不需要发明视觉，只需要把 token 换成正确的值。

---

## 2. 壳层与空间节奏

### 2.1 壳层结构（含实测尺寸）

```
┌────────────────────────────────────────────────────────────────────────────────┐
│ TitleBarControl  h=40   （XAML Island 覆盖在 NavigationView 之上，非系统标题栏） │
│ 起点 16,10：[16×16 logo] 8px [12px "Magpie"]        右端 3 × (46×32) caption    │
├──────────────────────────────┬─────────────────────────────────────────────────┤
│ NavigationView.Pane          │ NavigationView.ContentGrid                      │
│ 展开 320 / 紧凑 48（WinUI）   │ bg = LayerFillColorDefault  （Mica 之上 30% 灰） │
│ bg = Mica（DWM 绘制，不绘制）  │ radius 0（默认 8,0,0,0 → 被覆盖）                │
│                              │ border 1px 左边（默认 1,1,0,0 → 被覆盖）         │
│  ┌ item 36px, margin 4,2 ┐   │ ┌ PageFrame ──────────────────────────────────┐ │
│  │ icon16  text 12/14     │   │ │ header  Margin 40,54,40,16  title 28/600    │ │
│  │ ▎3×16 accent 选中条    │   │ │         （54 = 40 标题栏 + 14 视觉间隙）     │ │
│  └────────────────────────┘   │ │ body    ScrollViewer Padding 40,0,40,0      │ │
│  ── 组标题（14/600）──        │ │         stack Spacing 32（SettingsGroupSpacing）│
│  …                            │ │         ┌ SettingsGroup ───────────────────┐  │ │
│  Footer：关于                 │ │         │ 标题 14/600 + 说明 12/400        │  │ │
│                              │ │         │ 8px                              │  │ │
│                              │ │         │ card 68 · gap 2 · radius 4       │  │ │
└──────────────────────────────┴─┴─────────┴──────────────────────────────────┴──┘
```

### 2.2 标题栏（`TitleBarControl.xaml`）

| 项 | 值 | 证据 |
|---|---|---|
| 高度 | **40** | `:8` |
| 内容内边距 | `16,10,0,0` | `:14` |
| Logo | 16 × 16 | `:17` |
| 产品名 | **12 px**，`Margin 0,0,0,2`，与 logo 间距 **8** | `:19-21` |
| 窗口失活 | 文字变 `#8E8E8E` | `:96` |
| 紧凑态位移 | 标题栈 `TranslateX 0 ↔ 46`，**220 ms**，`ExponentialEase EaseOut Exponent=7` | `:57-79` |
| Caption 按钮 | **46 × 32**，图标 `Viewbox 10×10`，`BorderThickness 0`，不可 Tab | `CaptionButtonsControl.xaml:66-67,96` |
| Caption 悬停 / 按下 | 前景色 **6%** / **4%** 叠加；按下前景 70%；失活 35%（深）/ 38%（浅） | `:24-46` |
| 导航可滚动区 | 展开时 `Margin.Top = TitleBar.ActualHeight`（防止盖住标题栏） | `RootPage.cpp:246` |

> 我们的顶栏已是 40 px（`Theme.topbar-h`），**高度无需改**。要补的是：产品名 12 px（现为默认 13）、caption 类按钮 46×32 的命中区、失活降透明。

### 2.3 NavigationView 参数表

| 参数 | 值 | 等级 | 说明 |
|---|---|---|---|
| `CompactModeThresholdWidth` | **0** | A `:48` | 永远不进入"最小模式"（无汉堡菜单浮层） |
| `ExpandedModeThresholdWidth` | **920** | A `:50` | < 920 → 自动收成 48 px 紧凑轨；≥ 920 → 强制展开 |
| `IsBackButtonVisible` | `Collapsed` | A `:51` | 没有返回按钮 |
| `IsPaneToggleButtonVisible` | `!isExpanded` | A `RootPage.cpp:240` | **展开态没有汉堡按钮** |
| Pane 展开宽 / 紧凑宽 | 320（代码默认，**未在 XAML 定义，未逐字核实**）/ **48** | B | `NavigationViewCompactPaneLength` 已核实；320 为 WinUI 代码默认 |
| 导航项最小高 | **36** | B | `NavigationViewItemOnLeftMinHeight` |
| 导航项外边距 | **4,2** | B | `NavigationViewItemButtonMargin` → 左右 4、**上下 2** |
| 导航项圆角 | **4** | B | `ControlCornerRadius` |
| 导航项图标盒 | 16 × 16（居中） | B | `NavigationViewItemOnLeftIconBoxHeight` |
| 选中条 | **3 × 16，圆角 2**，`AccentFillColorDefault`，左对齐垂直居中 | B | `NavigationViewSelectionIndicator*` |
| 项内右边距 | 14 | B | `ContentGrid Margin 0,0,14,0` |
| 静息 / 悬停 / 按下底 | 透明 / `SubtleFillColorSecondary` / `SubtleFillColorTertiary` | B | 见 §4 |
| 选中底 | `SubtleFillColorSecondary`（与悬停同色） | B | 选中靠**竖条 + 文字主色**区分，不靠底色 |
| 选中 + 悬停 | `SubtleFillColorTertiary` | B | |
| 组标题项 | `NavigationViewItemHeader`（14/600，无图标，可折叠） | A `:80` | |
| 页脚项 | `FooterMenuItems`（"关于"） | A `:192-199` | |
| 默认展开 | 第一个项 `IsSelected="True"` | A `:63` | |

**导航项在深色下的合成色**（Mica `#202020` 为底）：

| 状态 | 刷子 | 合成结果 |
|---|---|---|
| 静息 | `transparent` | `#202020` |
| 悬停 / 选中 | `#0FFFFFFF`（5.9% 白） | `#2D2D2D` |
| 按下 | `#0AFFFFFF`（3.9% 白） | `#292929` |

> 注意：**选中与悬停同色**。这是 Win11 的原生行为——选中由左侧 3 px 竖条与文字提亮承担，底色只表示"指针在附近"。我们的 `app.slint` 现在用 `Button { checkable }`，选中是 Slint fluent 的 accent 底，比 Magpie 重得多。

### 2.4 内容层：0 圆角 + 1 px 左缝（关键决策）

**Magpie 实测**（`RootPage.xaml:57-58`）：

```xml
<CornerRadius x:Key="NavigationViewContentGridCornerRadius">0</CornerRadius>
<Thickness   x:Key="NavigationViewContentGridBorderThickness">1,0,0,0</Thickness>
```

**WinUI 默认**（被覆盖掉的原始值）：

```xml
<CornerRadius x:Key="NavigationViewContentGridCornerRadius">8,0,0,0</CornerRadius>
<Thickness   x:Key="NavigationViewContentGridBorderThickness">1,1,0,0</Thickness>
<StaticResource x:Key="NavigationViewContentBackground"    ResourceKey="LayerFillColorDefaultBrush" />
<StaticResource x:Key="NavigationViewContentGridBorderBrush" ResourceKey="CardStrokeColorDefaultBrush" />
```

**两者都"对"，但语义不同**：

| 方案 | 视觉 | 适用 | 我们能否实现 |
|---|---|---|---|
| A. 0 圆角 + 1 px 左缝（Magpie 选它） | 内容层与窗口齐平，靠 1 px 缝 + 8 个亮度级分层；与 DWM 的 8 px 窗口圆角不冲突 | 全屏出血式壳层（导航+内容都在窗口边缘） | **能**：`Rectangle { border-radius: 0; border-width: 1px; }` |
| B. 左上 8 px 圆角（WinUI 默认） | 内容层像一张浮卡压在 Mica 上；需要内容层四边都与窗口有间隙才自然 | 悬浮面板式 | **不能直接做**：Slint 1.19 的 `border-radius` 只有单值，无 `border-radius-top-left` |

**决策：采用 A（Magpie 实测值）。** 理由：

1. 我们的壳层是全屏出血式（顶栏 40 + 导航 168 都贴窗口边），B 的左上圆角会与 DWM 窗口圆角形成"双圆角"，在 150% DPI 下会出现 1.5 px 的锯齿缝。
2. A 少一个 Slint 不支持的特性，零实现成本。
3. A 的分层完全靠**亮度阶梯 + 1 px 缝**，这正是本文 §4.4 要建立的阶梯。

> 若将来要做 B：用 8×8 的 `Path` 画 1/4 圆弧补角（`M 0 8 A 8 8 0 0 1 8 0 L 8 8 Z` 填内容层色，再叠一条 1 px 描边弧），代价是每个角 2 个 `Path`，软件渲染可接受但没必要。

### 2.5 PageFrame 版心（`PageFrame.xaml`）

| 项 | 值 | 证据 |
|---|---|---|
| 版心最大宽 | **1000** | `:12` |
| 页头外边距 | **40, 54, 40, 16** | `:26` |
| 页头图标 | `Margin 0,5,16,0`（5 px 下移对齐首行基线，右侧 16 间距） | `:34` |
| 页头标题 | **28 px / SemiBold**，超长省略 | `:39-40` |
| 页头右侧动作 | 36×36 Subtle 按钮，图标 18 | `:48-58` |
| 正文滚动区 | `Padding 40,0,40,0`，可 Tab 聚焦（键盘可滚动），上下键 ±100 px | `:68-72`、`PageFrame.cpp:70-80` |
| 正文底边距 | 20 | `:76` |
| 响应式边距 | 宽 > 590 → 40；≤ 590 → **25** | `PageFrame.cpp:31` |
| 内容栈 | `Padding 0,4,0,0`，`Spacing = 32` | `HomePage.xaml:12-14` |

> 关键：`MaxWidth` + `Margin`（不是 `Padding`）+ `HorizontalAlignment=Stretch` → 版心在窗口内**居中**，超宽时左右留白增大，内容不拉伸。这是我们目前没有的行为（我们的 `body-w` 是硬算的）。

### 2.6 两级纵向节奏（本文最重要的一节）

```
                    ↑ 组间 32（SettingsGroupSpacing）
  [组标题] 14 / 600                        ← 紧贴内容，Margin 1,0,0,0
  [组说明] 12 / 400 TextFillColorSecondary ← 可选
  ↕ 8（ItemsPresenter Margin 0,8,0,0）
  ┌──────────────────────────────┐  68
  └──────────────────────────────┘
  ↕ 2（SettingsCardSpacing）
  ┌──────────────────────────────┐  68
  └──────────────────────────────┘
  ↕ 2
  ┌──────────────────────────────┐  68
  └──────────────────────────────┘
                    ↓ 组间 32
```

**这就是"不散乱"的全部秘密**：1:16 的组内/组间比。我们现在的 `Theme.gap = 8` 对**所有**间距一视同仁，于是每张卡片与其它卡片的关系强度相同 → 视觉上"平均、松散、没有段落"。

换算到我们 1080×700 的窗口（详见 §2.7）：

| 层级 | Magpie | 我们建议 | Token |
|---|---|---|---|
| 组内卡片间距 | 2 | **2** | `gap-card: 2px` |
| 组标题 → 首卡 | 8 | **8** | `gap-group-head: 8px` |
| 组间 | 32 | **24**（1:12） | `gap-group: 24px` |
| 页面栈顶内边距 | 4 | **4** | `pad-page-top: 4px` |

> 为什么组间取 24 而不是 32：Magpie 的 690 px 高窗口里只有 3 个组（首页），而我们有 8 个页面、每页 3–5 个组，且顶部还有 40 px 顶栏 + 32 px 页头。32 px 的组间会让 700 px 高的窗口一屏只能放 2 个组；24 px 在保持强对比（1:12）的同时多放出 1 个组。若某页只有 2 个组，可以用 32 px 回到 Magpie 原值。

### 2.7 与 v1.1 的差异与换算表

| 项 | v1.1 现状 | Magpie | 换算原则 | 建议 |
|---|---|---|---|---|
| 窗口 | 1080×700 / min 960×620 | 980×690 / min 500×300 | 保持我们的 | 不改 |
| 顶栏 | 40 | 40 | 相同 | 不改 |
| 导航宽 | 168 / 46 | 320 / 48 | 按标签长度 | 168 / 48（紧凑改 48 与 WinUI 一致） |
| 导航项高 | 32（代码 36） | 36 | 取大者 | **36** |
| 导航项间距 | 4 | 2 | 直接取 | **2** |
| 导航项圆角 | 5 | 4 | 直接取 | **4** |
| 导航内边距 | 6 | 4 | 直接取 | **4** |
| 页头 | 32 高，标题 20/600 | 54 上边距，28/600 | 我们页头在内容区内 | 保持 20/600，页头下间距 16 |
| 内容边距 | 12 | 40 | 我们是宽栅格 | 保持 12，但**卡片内边距 10 → 16** |
| 卡片高 | 58 / 66 / 104 | 68 | `16 + max(内容,36) + 16` | **68**（设置型）/ 58（纯信息行） |
| 卡片圆角 | 6 | 4 | 直接取 | **4** |
| 卡片间距 | 8 | 2 | 直接取 | **2**（组内） |
| 组间 | 无（8 统一） | 32 | 1:12 | **24** |
| 卡片内边距 | 8×10 | 16 四边 | 直接取 | **16**（左右）/ 16（上下） |
| 卡片图标 | 20 | ≤20 | 相同 | 20 |
| 图标 → 文字 | 7 | 20 | 直接取 | **20** |
| 文字 → 控件 | 8 | 24 | 直接取 | **24** |
| 控件最小宽 | — | 200 | 直接取 | **200** |
| 分组标题 | 12/600 | 14/600 | 直接取 | **14/600** |
| 动效 | 80/120/160 | 83/150/167–333 | 见 §5 | 83/150/167 |

**唯一需要"算"的一条**：卡片最小高。

```
h_card = 16 + max(36, h_content) + 16   ≥ 68
```

`h_content = 36` 来自 WinUI 控件行高（我们 `control-h = 32`，加 4 px 呼吸）。所以我们的 `SettingsCard` 恒 68，除非内容更高（如曲线画布）。

---

## 3. 组件规范

### 3.1 `SettingsCard`（核心组件）

**结构**（4 列网格，`SettingsCard.Resource.xaml:150-235`）：

```
┌───────────────────────────────────────────────────────────────────────────┐
│ 16 │ [icon ≤20] │20│ 标题 14/400 ────────────────────── 24 │ 控件 │14│ ◇13│ 16 │
│    │  margin    │  │ 副标题 12/400 TextFillColorSecondary     │右对齐│  │动作│    │
│    │  2,0,20,0  │  │                                          │垂直居中│ │图标│    │
└───────────────────────────────────────────────────────────────────────────┘
     └─ 第 1 列 Auto（左缩进，默认 0）  └─ 第 2 列 *（标题区）  └ 第 3 列 Auto（内容）└ 第 4 列（动作图标）
```

| 参数 | 值 | 证据 |
|---|---|---|
| 最小高 | **68** | `:70` |
| 最小宽 | 148 | `:69` |
| 内边距 | **16,16,16,16** | `:68` |
| 边框 | **1 px**，`CardStrokeColorDefaultBrush` | `:67,28-30` |
| 圆角 | **4**（`ControlCornerRadius`） | `:91` |
| 背景 | `CardBackgroundFillColorDefaultBrush` | `:10-12` |
| 头部图标 | `Viewbox` 上限 **20×20**，`Margin 2,0,20,0` | `:71,74,167-169` |
| 标题 | 继承 `ControlContentThemeFontSize` = **14 / 400**，`TextWrapping=Wrap` | `:102,180-182` |
| 副标题 | **12 px**，`TextFillColorSecondaryBrush`，与标题 **0 间距**（同栈） | `:186-187,174` |
| 标题区右边距 | **24** | `:174` |
| 内容列 | `HorizontalAlignment=Right`，`VerticalAlignment=Center` | `:206-208` |
| 内容最小宽 | **200**（Slider/ComboBox/TextBox/NumberBox 统一） | `:73` |
| 动作图标 | 上限 **13**，`Margin 14,0,0,0`，默认折叠 | `:76,75,225-227` |
| 垂直堆叠间距（wrap 后） | 8 | `:77` |
| 响应式换行阈值 | **476**（有图标）/ **286**（无图标） | `:78-79` |
| `IsTabStop` | false（卡片本身不可聚焦，内部控件可） | `:96` |

**状态矩阵**（`:236-320`，全部瞬切）：

| 状态 | 背景 | 边框 | 前景 |
|---|---|---|---|
| 静息 | `CardBackgroundFillColorDefault` | `CardStrokeColorDefault` | `TextFillColorPrimary` |
| 悬停 | `ControlFillColorSecondary` | `ControlElevationBorderBrush` | `TextFillColorPrimary` |
| 按下 | `ControlFillColorTertiary` | `ControlStrokeColorDefault` | `TextFillColorSecondary` |
| 禁用 | `ControlFillColorDisabled` | `ControlStrokeColorDefault` | `TextFillColorDisabled` |

> `ControlElevationBorderBrush` 是**纵向 3 px 渐变描边**（上 `ControlStrokeColorSecondary` = 白 9.4% → 下 `ControlStrokeColorDefault` = 白 7.1%，浅色主题反向），即"顶部受光"。Slint 的 `border-color` 只有单值 → 我们取 `ControlStrokeColorSecondary` 作为悬停描边，视觉差 1 个亮度级，可接受（§6.3 有说明）。

**响应式换行**（`:336-360`）：可用宽 < 476 时，控件从第 3 列移到第 2 行，行间距 8。这解释了我们 2–3 列栅格里卡片为什么会"变高"——**Magpie 也会**，只是它单列所以几乎不触发。

### 3.2 `SettingsGroup`

| 参数 | 值 | 证据 |
|---|---|---|
| 组标题 | `BodyStrongTextBlockStyle` = **14 / 600**，`Margin 1,0,0,0`，`HeadingLevel=2`（无障碍） | `:32-37` |
| 组说明 | `Caption` = **12 / 400**，`TextFillColorSecondaryBrush`，`WrapWholeWords` | `:39-45` |
| 标题 → 首卡 | **8** | `:54` |
| 卡片间距 | `SettingsCardSpacing` = **2** | `App.xaml:84` |
| 组间距 | `SettingsGroupSpacing` = **32** | `App.xaml:85` |
| 重排动画 | `RepositionThemeTransition IsStaggeringEnabled=False`（列表变化时卡片平滑重排，**不**逐个错峰） | `App.xaml:80-82` |
| 禁用态 | 标题与说明转 `TextFillColorDisabled` | `:57-65` |

> `HeadingLevel=2` 是必须抄的可访问性细节：屏幕阅读器把组标题读成二级标题，8 个页面立刻具备结构。

### 3.3 `SettingsExpander`（最适合我们的高级配置）

**Magpie 的做法**：`Expander` 的 Header 直接塞一个 `SettingsCard`（背景透明、无边框、`Padding 16,16,4,16`），Content 是子项容器（`SettingsExpander.Resource.xaml:75-100`）。

| 参数 | 值 | 证据 |
|---|---|---|
| 头部内边距 | **16,16,4,16**（右侧 4 给 chevron 让位） | `:10` |
| 子行内边距 | **58,8,44,8**（可点击行 58,8,16,8） | `:11,13` |
| 子行最小高 | **52** | `:25` |
| 子行圆角 | **0** | `:27` |
| 子行分隔线 | **0,1,0,0**（仅上边 1 px） | `:12` |
| 子行缩进 58 的推导 | `16(内边距) + 2(图标左 margin) + 20(图标) + 20(图标右 margin)` = **58** → 子行文字与头部标题**严格左对齐** | C 级推导，与 `:11` 吻合 |
| 右侧 44 的推导 | `32(chevron) + 8(右 margin) + 4(头部内边距)` = 44 | C 级 |
| Chevron 按钮 | **32 × 32**，`Margin 0,0,8,0`，圆角 4，图标 16×16，`AnimatedIcon`（上/下 180° 翻转） | `:15-16,261-288` |
| 展开体最小高 | 16 | `:14` |
| 展开体上内边距修正 | 内容 `Margin 0,-2,0,0`（抵消头部 2 px 光学间隙） | `:160` |
| 圆角处理 | 展开时头部变"上圆角"、内容变"下圆角"（`SettingsExpanderCornerRadiusConverter`） | `:89,158,175` |

**动效**（`:176-217`，这是全套 UI 里最精致的一处）：

| 阶段 | 属性 | 时长 | 曲线 |
|---|---|---|---|
| 展开 | `TranslateY: −contentHeight → 0` | **333 ms** | `cubic-bezier(0.0, 0.0, 0.0, 1.0)`（Fast Out Slow In） |
| 展开 | 可见性 | 0 ms | 立即 Visible |
| 收起 | `Opacity: 1 → 0` | **48 ms** | 线性 |
| 收起 | `TranslateY: 0 → −contentHeight` | **167 ms** | `cubic-bezier(1.0, 1.0, 0.0, 1.0)`（Fast In Slow Out） |
| 收起 | 可见性 | 48 ms 时 | 折叠 |

> **没有高度插值**。WinUI 用"整块内容位移 + 裁剪"实现展开，因为高度插值会触发逐帧布局。我们在 Slint 里没有 `TranslateY` + `clip` 的等价物（`clip` 只裁子元素），只能用 `height` 动画——代价可接受（见 §5.3），但**必须** `clip: true`，否则内容溢出。

**给我们的三个用法**（这是 Magpie 体系里最被低估的组件）：

| 场景 | 折叠态（68 px） | 展开态 |
|---|---|---|
| 自动化规则 | 名称 + 优先级 Chip + 结果 Chip + 开关 | 三列诊断 + 最近触发 + 触发/动作明细 |
| 风扇曲线预设 | 预设名 + 节点数 + "应用" | 6 个节点数值微调行 |
| 停用器（服务/进程） | 服务名 + 状态 + 开关 | 路径、启动类型、冲突说明、恢复按钮 |

### 3.4 `InfoBar`（内联状态条）

Magpie **不用全局 banner**，而是把 `InfoBar` 当作 `SettingsGroup` 里的一个**同级项**，出现在它解释的那张卡片正下方（`HomePage.xaml:150-171,206-218`）。

| 参数 | 值 | 证据 |
|---|---|---|
| 图标外边距 | **0,16,22,16** | `App.xaml:90` |
| 内容左边距 | **20,0,0,0** | `App.xaml:91` |
| 信息级底色（深） | **`#FF34424D`**（蓝灰，不是纯 accent） | `BlueInfoBarStyle.xaml:8` |
| 信息级底色（浅） | **`#FFD3E7F7`** | `:12` |
| 信息级图标底 | `#FF5FB2F2` / `#FF0063B1` | `:9,13` |
| 关闭按钮 | 按需 `IsClosable`；解释性提示多为 `False` | 用法 |
| 位置 | 紧跟被解释卡片之后，间距同卡片（2 px） | 用法 |

> 我们的 `Banner` 是**全局**的（在顶栏下、占 40 px 高、挤压所有页面）。建议分工：
> **全局断连 / 只读 / 恢复必需 → 保留 `Banner`**（它必须常驻且跨页）；
> **局部原因（"此控件被官软占用"、"需重启生效"）→ 用内联 `InfoBar` 形态**，跟在卡片下方，不再挤占全局高度。

### 3.5 `KeyVisual` / `ShortcutControl`（快捷键徽章）

| 参数 | 小键帽 | 大键帽 | 证据 |
|---|---|---|---|
| 最小宽 × 高 | **40 × 36** | 56 × 48 | `KeyVisual.Resource.xaml:82-84,20-21` |
| 圆角 | **4**（`RadiusX/Y`） | 4 | `:36-37` |
| 字号 / 字重 | **14 / SemiBold** | 18 / SemiBold | `:88-89` |
| 内边距 | 12,0,12,2（2 px 下移补偿大写字母视觉重心） | 16,8,16,8 | `:87,28` |
| 底色 / 前景 | `AccentButtonBackground` / `AccentButtonForeground` | 同 | `:22-25` |
| 描边 | `AccentButtonBorderBrush` 1 px | 同 | `:26` |
| 错误态 | 底色转 `InfoBarErrorSeverityBackgroundBrush`，描边加粗到 **2 px** | 同 | `:60-66` |
| 键与键间距 | **4** | 8 | `ShortcutControl.xaml:42,62` |
| 徽章组 → 编辑图标 | 16 | — | `:40-41` |
| 外层按钮 | 透明、无边框、`Margin 0,-6,0,-6`（垂直吃掉 12 px 让徽章贴紧卡片） | — | `:11-16` |
| 内层留白 | `Margin 6,0,6,0` | — | `:37` |

> 这套徽章是我们**最该抄的组件**：`automation` 页的快捷键、`monitor` 页的 OSD 热键、`system` 页的全局热键，目前都只是纯文本。40×36 的 accent 键帽是 Win11 设置里"快捷键"的原生表达。

### 3.6 "双栏 Master-Detail"的真实形态

必须澄清：Magpie **没有页内双栏**。它的"列表-详情"是：

```
NavigationView.Pane（列表）              ContentFrame（详情页）
├ 主页                                  ├ PageFrame: 标题 28/600
├ 缩放模式                              └ SettingsGroup 栈（单列，版心 ≤1000）
├ ── 配置文件（组标题）──
├ 默认（详情页 = ProfilePage）
├ 游戏 A（详情页 = ProfilePage）
├ 游戏 B
├ + 新建配置文件
└ 关于（Footer）
```

证据：`ScalingModesPage.xaml:97`（单个 `ListView` 铺满版心）、`ProfilePage.xaml:135`（`SettingsGroup` 栈，无第二列）。

**页内唯一出现的"两列"**是 Expander 内部的参数行（`ScalingModesPage.xaml:41-54`）：`Grid ColumnSpacing=4`，左列 `*`（参数名），右列 **60**（数值）。

**给我们的结论**：

| 我们的页面 | 该不该做页内双栏 | 做法 |
|---|---|---|
| 自动化 | 要（规则列表 + 诊断栏） | `2 : 1` 栅格，左 `SettingsExpander` 栈，右 `SettingsGroup`（口径 + 模板） |
| 键盘灯效 | 要（矩阵 + 属性） | `2 : 1`，右列 `SettingsGroup` |
| 监控与 OSD | 不要 | 单列 `SettingsGroup`（Magpie 式） |
| 系统与工具 | 不要 | 单列 `SettingsGroup` |

### 3.7 按钮三态与语义色

| 变体 | 用途 | 关键参数 | 证据 |
|---|---|---|---|
| `SubtleButtonStyle` | 页头动作、图标按钮 | 底 `SubtleFillColorTransparent/Secondary/Tertiary`；**`BrushTransition 0.083s`** | `App.xaml:106-141` |
| `AccentButtonStyle`（WinUI） | 唯一主操作 | `AccentFillColorDefault`，深色下文字**黑色** | `Common_themeresources:125` |
| `CriticalButtonStyle` | 危险（删配置） | 底 `Firebrick`，悬停 **0.9**、按下 **0.8** 不透明度，文字 `#FFFFFF` | `CriticalButtonStyle.xaml:4-10` |

> 危险按钮不是"红色描边"，是**实心 Firebrick + 不透明度递减**。这条比我们的 `danger` 描边卡片更"敢"——删除类操作应当一眼可辨。

---

## 4. 颜色

### 4.1 合成规则（先读这一节再看表）

WinUI 的填充刷子大量是**半透明**的，它们的最终颜色取决于**叠在谁身上**：

```
result = a · src + (1 − a) · base        // 每通道，sRGB 空间直接插值
```

三条铁律：

1. **卡片状态色是"替换"不是"叠加"**。悬停时卡片背景从 `CardBackgroundFillColorDefault`（5.1% 白）**换成** `ControlFillColorSecondary`（8.2% 白），基准是**内容层**而不是卡片本身。所以深色下 pressed（`#2F2F2F`）比静息卡片（`#333333`）**更暗**。
2. **深色下描边比卡片更暗**。`CardStrokeColorDefault` 在深色主题是**黑 10%**（`#19000000`），合成后 `#242424`，比卡片 `#333333` 暗 → 卡片边缘是"内凹刻线"，不是"高光边"。
3. **浅色下 hover 与 pressed 的填充差值几乎为 0**（都合成到 `#F9F9F9`），状态由**描边**承担。这解释了为什么 WinUI 浅色主题的悬停反馈"很淡"——它是设计如此，不是缺陷。

### 4.2 深色 token 表

| WinUI token | ARGB | rgba | 合成基准 | 合成后实色 | 我们对应 | 用途 |
|---|---|---|---|---|---|---|
| `SolidBackgroundFillColorBase` | `#202020` | — | — | `#202020` | `bg` 回退 | 窗口基底（Mica 不可用时） |
| `SolidBackgroundFillColorSecondary` | `#1C1C1C` | — | — | `#1C1C1C` | `inset` | 凹槽、画布底、进度条槽 |
| `SolidBackgroundFillColorTertiary` | `#282828` | — | — | `#282828` | `flyout` | 弹出层、对话框 |
| `SolidBackgroundFillColorQuarternary` | `#2C2C2C` | — | — | `#2C2C2C` | — | 二级浮层 |
| `LayerFillColorDefault` | `#4C3A3A3A` | `rgba(58,58,58,0.30)` | Mica `#202020` | **`#282828`** | `content` | **内容层**（关键改动） |
| `CardBackgroundFillColorDefault` | `#0DFFFFFF` | `rgba(255,255,255,0.051)` | 内容层 | **`#333333`** | `card` | 卡片 |
| `CardBackgroundFillColorSecondary` | `#08FFFFFF` | `rgba(255,255,255,0.031)` | 内容层 | **`#2F2F2F`** | `card-2` | 二级卡片、内嵌行 |
| `CardBackgroundFillColorTertiary` | `#12FFFFFF` | `rgba(255,255,255,0.071)` | 内容层 | `#373737` | — | 三级卡片 |
| `ControlFillColorSecondary` | `#15FFFFFF` | `rgba(255,255,255,0.082)` | 内容层 | **`#3A3A3A`** | `card-hover` | 卡片悬停 |
| `ControlFillColorTertiary` | `#08FFFFFF` | `rgba(255,255,255,0.031)` | 内容层 | **`#2F2F2F`** | `card-pressed` | 卡片按下 |
| `ControlFillColorDisabled` | `#0BFFFFFF` | `rgba(255,255,255,0.043)` | 内容层 | `#313131` | — | 禁用底 |
| `SubtleFillColorSecondary` | `#0FFFFFFF` | `rgba(255,255,255,0.059)` | Mica | **`#2D2D2D`** | `nav-hover`/`nav-selected` | 导航悬停/选中 |
| `SubtleFillColorTertiary` | `#0AFFFFFF` | `rgba(255,255,255,0.039)` | Mica | `#292929` | `nav-pressed` | 导航按下 |
| `CardStrokeColorDefault` | `#19000000` | `rgba(0,0,0,0.098)` | 内容层 | **`#242424`** | `card-stroke` | 卡片 1 px 描边 |
| `ControlStrokeColorDefault` | `#12FFFFFF` | `rgba(255,255,255,0.071)` | 卡片 | `#414141` | `stroke-strong` | 输入框描边、按下描边 |
| `ControlStrokeColorSecondary` | `#18FFFFFF` | `rgba(255,255,255,0.094)` | 卡片 | **`#464646`** | `card-stroke-hover` | 悬停描边（顶部） |
| `DividerStrokeColorDefault` | `#15FFFFFF` | `rgba(255,255,255,0.082)` | 内容层 | `#3A3A3A` | `divider` | 分隔线 |
| `TextFillColorPrimary` | `#FFFFFF` | `rgba(255,255,255,1.0)` | — | `#FFFFFF` | `text` | 主文字 |
| `TextFillColorSecondary` | `#C5FFFFFF` | `rgba(255,255,255,0.773)` | — | — | `text-2` | 副标题（**0.773 而非 0.786**） |
| `TextFillColorTertiary` | `#87FFFFFF` | `rgba(255,255,255,0.529)` | — | — | `text-3` | 说明、轴标签 |
| `TextFillColorDisabled` | `#5DFFFFFF` | `rgba(255,255,255,0.365)` | — | — | `text-disabled` | 禁用 |
| `AccentFillColorDefault` | = `SystemAccentColorLight2` | — | — | `#60CDFF`（默认蓝） | `accent` | 主强调 |
| `TextOnAccentFillColorPrimary` | `#000000` | — | — | — | `on-accent` | **深色下强调上的文字是黑色** |
| `SystemFillColorSuccess` | `#6CCB5F` | — | — | — | `ok` | 成功 |
| `SystemFillColorCaution` | `#FCE100` | — | — | — | `warn` | 警告（纯黄） |
| `SystemFillColorCritical` | `#FF99A4` | — | — | — | `danger` | 危险（柔粉红） |
| `SystemFillColorSuccessBackground` | `#393D1B` | — | — | — | `ok-surface` | 成功底 |
| `SystemFillColorCautionBackground` | `#433519` | — | — | — | `warn-surface` | 警告底 |
| `SystemFillColorCriticalBackground` | `#442726` | — | — | — | `danger-surface` | 危险底 |
| `SystemFillColorAttentionBackground` | `#08FFFFFF` | `rgba(255,255,255,0.031)` | 内容层 | `#2F2F2F` | `info-surface` | 信息底 |

### 4.3 浅色 token 表

| WinUI token | ARGB | rgba | 合成基准 | 合成后实色 | 用途 |
|---|---|---|---|---|---|
| `SolidBackgroundFillColorBase` | `#F3F3F3` | — | — | `#F3F3F3` | 基底 |
| `SolidBackgroundFillColorSecondary` | `#EEEEEE` | — | — | `#EEEEEE` | 凹槽 |
| `SolidBackgroundFillColorTertiary` | `#F9F9F9` | — | — | `#F9F9F9` | 浮层 |
| `LayerFillColorDefault` | `#80FFFFFF` | `rgba(255,255,255,0.50)` | Mica `#F3F3F3` | **`#F9F9F9`** | 内容层 |
| `CardBackgroundFillColorDefault` | `#B3FFFFFF` | `rgba(255,255,255,0.70)` | 内容层 | **`#FDFDFD`** | 卡片 |
| `CardBackgroundFillColorSecondary` | `#80F6F6F6` | `rgba(246,246,246,0.50)` | 内容层 | **`#F7F7F7`** | 二级卡片 |
| `ControlFillColorSecondary` | `#80F9F9F9` | `rgba(249,249,249,0.50)` | 内容层 | `#F9F9F9` | 悬停 |
| `ControlFillColorTertiary` | `#4DF9F9F9` | `rgba(249,249,249,0.30)` | 内容层 | `#F9F9F9` | 按下（**与悬停同色**） |
| `SubtleFillColorSecondary` | `#09000000` | `rgba(0,0,0,0.035)` | Mica | `#EAEAEA` | 导航选中/悬停 |
| `SubtleFillColorTertiary` | `#06000000` | `rgba(0,0,0,0.024)` | Mica | `#EDEDED` | 导航按下 |
| `CardStrokeColorDefault` | `#0F000000` | `rgba(0,0,0,0.059)` | 内容层 | **`#EAEAEA`** | 卡片描边 |
| `ControlStrokeColorDefault` | `#0F000000` | `rgba(0,0,0,0.059)` | 卡片 | `#EDEDED` | 控件描边 |
| `ControlStrokeColorSecondary` | `#29000000` | `rgba(0,0,0,0.161)` | 卡片 | `#D4D4D4` | 悬停描边 |
| `DividerStrokeColorDefault` | `#0F000000` | `rgba(0,0,0,0.059)` | 内容层 | `#EAEAEA` | 分隔线 |
| `TextFillColorPrimary` | `#E4000000` | `rgba(0,0,0,0.894)` | 卡片 `#FDFDFD` | `#1B1B1B`（≈`#1A1A1A`） | 主文字 |
| `TextFillColorSecondary` | `#9E000000` | `rgba(0,0,0,0.620)` | 卡片 `#FDFDFD` | `#606060` | 副标题 |
| `TextFillColorTertiary` | `#72000000` | `rgba(0,0,0,0.447)` | 卡片 `#FDFDFD` | `#8C8C8C` | 说明 |
| `TextFillColorDisabled` | `#5C000000` | `rgba(0,0,0,0.361)` | 卡片 `#FDFDFD` | `#A2A2A2` | 禁用 |
| `AccentFillColorDefault` | = `SystemAccentColorDark1` | — | — | `#005FB8` | 主强调 |
| `TextOnAccentFillColorPrimary` | `#FFFFFF` | — | — | — | 强调上的文字（白） |
| `SystemFillColorSuccess` | `#0F7B0F` | — | — | — | 成功 |
| `SystemFillColorCaution` | `#9D5D00` | — | — | — | 警告 |
| `SystemFillColorCritical` | `#C42B1C` | — | — | — | 危险 |
| `SystemFillColorSuccessBackground` | `#DFF6DD` | — | — | — | 成功底 |
| `SystemFillColorCautionBackground` | `#FFF4CE` | — | — | — | 警告底 |
| `SystemFillColorCriticalBackground` | `#FDE7E9` | — | — | — | 危险底 |

### 4.4 亮度阶梯与对比度验算

**深色阶梯**（假设 Mica 解析为纯 `#202020`，无壁纸染色）：

| 层 | 颜色 | 相对上一层 ΔL* | 对比度 |
|---|---|---|---|
| L0 Mica | `#202020` | — | — |
| L1 内容层 | `#282828` | +8 | 1.11:1 |
| L2 卡片 | `#333333` | +11 | 1.17:1 |
| L2' 二级卡片 | `#2F2F2F` | +7 | 1.09:1 |
| L3 卡片悬停 | `#3A3A3A` | +7 | 1.10:1（vs 卡片） |
| L3' 卡片按下 | `#2F2F2F` | −4（**更暗**） | 0.91:1 |
| 卡片描边 | `#242424` | −15（**比卡片暗**） | 1.20:1 |

**浅色阶梯**：

| 层 | 颜色 | 对比度 |
|---|---|---|
| L0 Mica | `#F3F3F3` | — |
| L1 内容层 | `#F9F9F9` | 1.05:1 |
| L2 卡片 | `#FDFDFD` | 1.04:1 |
| L2' 二级卡片 | `#F7F7F7` | 0.98:1 |
| 卡片描边 | `#EAEAEA` | 1.08:1（vs 卡片） |

**文字对比度**（在卡片上，WCAG 2.1）：

| 组合 | 深色 | 浅色 | 判定 |
|---|---|---|---|
| 主文字 / 卡片 | **12.63:1** | **16.93:1** | AAA |
| 副文字 / 卡片 | **8.27:1** | **6.18:1** | AAA / AA |
| 三级文字 / 卡片 | **4.77:1** | 3.31:1 | 深色 AA；**浅色仅适合 ≥14 px 或非关键信息** |
| accent / 卡片 | **7.02:1** | **6.20:1** | AA+ |
| on-accent / accent | 11.67:1（黑） | 6.31:1（白） | AAA / AA |

> 三条可执行结论：
> 1. 深色的 `text-3`（`#87FFFFFF`）在卡片上是 4.77:1，**可以**用于 12 px 说明文字；我们现在的 `#B0B0B0`（≈3.4:1）**不够**，这就是"灰得看不清"的量化原因。
> 2. 浅色 `text-3` 只有 3.31:1，**不得**用于承载信息，只能用于时间戳/轴标签这类可推断内容。我们的 `#606060` 反而比 WinUI 更保守（好）。
> 3. 强调色在两种主题下都 ≥6:1，说明 `#60CDFF` 不是"刺眼霓虹"——它的对比度低于主文字（12.6:1），所以视觉上永远从属于内容。

### 4.5 Accent 派生规则

Magpie 不硬编码强调色，它用 WinUI 的系统派生：

| 主题 | 刷子 | 派生源 |
|---|---|---|
| 深色 | `AccentFillColorDefault` = **`SystemAccentColorLight2`** | 系统强调色提亮 2 档 |
| 深色 | `AccentTextFillColorPrimary` = `SystemAccentColorLight3` | 提亮 3 档（文字用更亮） |
| 浅色 | `AccentFillColorDefault` = **`SystemAccentColorDark1`** | 压暗 1 档 |
| 浅色 | `AccentTextFillColorPrimary` = `SystemAccentColorDark2` | 压暗 2 档 |

系统默认蓝 `#0078D4` 时：`Light2 = #60CDFF`、`Dark1 = #005FB8`、`Light3 = #99EBFF`、`Dark2 = #003E92`。

**我们的落地策略**：

| 阶段 | 做法 |
|---|---|
| v1（现在） | 保留常量 `#4CC2FF` / `#0F6CBD`。它们与 `#60CDFF` / `#005FB8` 同族（同色相、略低饱和），视觉无差；且已有实测对比度记录（§3.1 of UI_DESIGN_SPEC）。 |
| P1 | 读 `HKCU\Software\Microsoft\Windows\DWM\AccentColor`（DWORD，`0xAABBGGRR`）→ 得到 Base；再读 `HKCU\...\Explorer\Accent\AccentPalette`（32 字节 = 8 组 RGB）→ 得到 Light1..3 / Base / Dark1..3。**校验断言**：默认蓝下 `Light2` 必须读出 `60 CD FF`、`Dark1` 必须读出 `00 5F B8`，否则回退常量。 |
| P2 | 用户在 `ui.json` 可覆盖 accent（写死 base），派生仍走 Windows 公式。 |

> 不要自己实现"提亮 2 档"的算法：Windows 的 Light/Dark 变体在 HSL 空间里对饱和度和明度做了非线性处理，复刻必然有色差。读 `AccentPalette` 是唯一正确做法。

### 4.6 与现有 `ui/theme.slint` 的逐 token diff

| 现 token | 现值（深/浅） | 建议 | 动作 |
|---|---|---|---|
| `bg` | `transparent` / `#1C1C1C` | 不变 | 保持 |
| `topbar` | `#1A1A1A` / `#F7F7F7` | `SolidBackgroundFillColorBase` 的近似，**保留不透明**（顶栏不参与 Mica 分层） | 保持 |
| `nav` | `#EB191919` / `#EBEFEFEF` | 改为 **`transparent`**（让 Mica 直接透出，与 Magpie 一致）；仅在 `mica == false` 时用 `#191919` | **改** |
| `content` | `#202020` / `#F3F3F3`（不透明） | **`#282828`（= LayerFillColorDefault 合成值）/ `#F9F9F9`**；`mica == false` 时回退 `#202020` / `#F3F3F3` | **改（最高优先级）** |
| `card` | `#262626` / `#FFFFFF` | **`#333333` / `#FDFDFD`** | **改** |
| `card-hover` | `#2D2D2D` / `#F6F6F6` | **`#3A3A3A` / `#F9F9F9`** | **改** |
| `card-pressed` | `#323232` / `#EFEFEF` | **`#2F2F2F` / `#F9F9F9`**（深色下比卡片暗） | **改** |
| `inset` | `#1D1D1D` / `#F2F2F2` | `#1C1C1C` / `#EEEEEE` | 微调 |
| `flyout` | `#2B2B2B` / `#FFFFFF` | `#282828` / `#F9F9F9` | 微调 |
| `stroke` | `#343434` / `#E2E2E2` | **`#242424` / `#EAEAEA`**（深色是"内凹刻线"） | **改** |
| `stroke-strong` | `#4A4A4A` / `#C6C6C6` | `#414141` / `#D4D4D4` | 改 |
| `divider` | `#2C2C2C` / `#EAEAEA` | `#3A3A3A` / `#EAEAEA` | 改（深色分隔线要更亮） |
| `text` | `#F2F2F2` / `#1A1A1A` | `#FFFFFF` / `#1A1A1A` | 改 |
| `text-2` | `#C9C9C9` / `#4A4A4A` | `rgba(255,255,255,0.773)` / `rgba(0,0,0,0.620)` | 改 |
| `text-3` | `#B0B0B0` / `#606060` | `rgba(255,255,255,0.529)` / `rgba(0,0,0,0.447)` | 改 |
| `accent` | `#4CC2FF` / `#0F6CBD` | 保持（P1 接系统） | 保持 |
| `on-accent` | `#07161E` / `#FFFFFF` | `#000000` / `#FFFFFF` | 微调 |
| `ok` / `warn` / `danger` | `#6CCB5F` / `#FFC83D` / `#FF6B6B` | `#6CCB5F` / **`#FCE100`** / **`#FF99A4`** | 改（warn/danger） |
| `ok/warn/danger-surface` | `#293529` / `#383226` / `#3A2929` | **`#393D1B` / `#433519` / `#442726`** | **改** |
| `muted-surface` | `#303030` / `#F0F0F0` | `#2F2F2F` / `#F7F7F7` | 微调 |
| `radius` | 5 | 4（控件） | 改 |
| `radius-card` | 6 | **4** | **改** |
| `gap` | 8 | 8（栅格列间距保留） | 保持 |
| — | 缺 | `gap-card: 2` / `gap-group: 24` / `gap-group-head: 8` | **新增** |
| `card-min-h` / `card-max-h` | 58 / 66 | `card-h: 68` / `card-h-compact: 58` | 改 |
| `dur-fast` / `dur` / `dur-slow` | 80 / 120 / 160 | `dur-hover: 83` / `dur-nav: 150` / `dur-expand: 333` / `dur-collapse: 167` | 改 |

> 关于 `warn`：`#FCE100` 在深色卡片上的对比度是 12.4:1，视觉上"很亮"。如果我们觉得过于刺眼，可以只在**图标**上用 `#FCE100`，文字仍用 `#FFC83D`（Magpie 自己就是这么做的：`InfoBar` 的图标用 `SystemFillColorCautionBrush`，正文用 `TextFillColorPrimary`）。

---

## 5. 动效

### 5.1 Magpie 动效清单（已核实 5 处 + 1 处 WinUI 内部）

| # | 位置 | 属性 | 时长 | 曲线 | 证据 |
|---|---|---|---|---|---|
| 1 | Subtle 按钮底色 | `BackgroundTransition` | **83 ms** | 线性 | `App.xaml:141` |
| 2 | 标题栏紧凑切换 | `TranslateX 0↔46` | **220 ms** | `ExponentialEase EaseOut, Exponent=7` | `TitleBarControl.xaml:57-79` |
| 3 | 导航选中条 | 偏移（WinUI Composition，代码实现） | **未在 XAML 定义** | 未核实 | 说明：`NavigationView_themeresources.xaml` 内**无** `Duration`/`KeySpline`，该动画在 WinUI C++ 代码中。我们在 §5.5a 取 **150 ms + `cubic-bezier(0,0,0,1)`**，属于按 WinUI 通用曲线**自定**，不是抄来的数值 |
| 4 | Expander 展开 | `TranslateY −h→0` | **333 ms** | `cubic-bezier(0.0,0.0,0.0,1.0)` | `SettingsExpander:184-191` |
| 5 | Expander 收起 | `Opacity 1→0` + `TranslateY 0→−h` | **48 ms** + **167 ms** | 线性 / `cubic-bezier(1.0,1.0,0.0,1.0)` | `:196-217` |
| 6 | 列表重排 | `RepositionThemeTransition`（`IsStaggeringEnabled=False`） | WinUI 内部 ≈ 300 ms | 位移 + 淡入 | `App.xaml:80-82` |

**已核实的 5 处全部 ≤ 333 ms，没有一处缩放，没有一处循环。** 这是"克制"的可量化定义。

我们**明确不抄**的两处：标题栏 220 ms 位移（我们顶栏不参与收展）、列表重排（软件渲染下整列重排会全屏重绘）。

### 5.2 曲线速查

| 语义 | WinUI 名 | cubic-bezier | 用途 |
|---|---|---|---|
| Fast Out Slow In | `KeySpline 0,0,0,1` | `cubic-bezier(0, 0, 0, 1)` | 进入、展开、位移 |
| Fast In Slow Out | `KeySpline 1,1,0,1` | `cubic-bezier(1, 1, 0, 1)` | 退出、收起 |
| 标准 | `ease-out`（Slint 内置 = `0,0,0.58,1`） | `cubic-bezier(0, 0, 0.58, 1)` | 颜色、透明度 |
| 对称 | `ease-in-out`（Slint = `0.42,0,0.58,1`） | — | 位置来回（导航条） |

> **陷阱**：Slint 的 `ease-out` 是 `cubic-bezier(0,0,0.58,1)`，**不是** WinUI 的 `cubic-bezier(0,0,0,1)`。要 WinUI 手感必须显式写 `cubic-bezier(0, 0, 0, 1)`。Slint 1.19 支持 `cubic-bezier(a,b,c,d)` 与 `spring(...)`（`i-slint-compiler/lookup.rs:993`）。

### 5.3 软件渲染帧预算与降级

实测基线（`UI_DESIGN_SPEC §9.3`）：`renderer-software`，1080×700@150% → 1620×1050 物理像素，CPU 空闲 ≈ 0–1%。

| 动效 | 每帧重绘面积 | 判定 | 依据 |
|---|---|---|---|
| 颜色过渡（83 ms） | 卡片局部（Slint 有 damage 跟踪） | **可做** | 只改 brush，不触发重排 |
| 导航条位移（150 ms） | 3×16 + 旧位置 ≈ 100 px² | **可做** | 绝对定位，不触发布局 |
| 卡片高度展开（167/333 ms） | 整页（父布局逐帧重排） | **可做，但限 1 个** | 每次展开 ≈ 20 帧 × 1 页布局；单页 ≤ 1 个展开体同时动画 |
| 整页淡入（150 ms） | 全窗 | **可做** | 只改 opacity，无重排 |
| 整页位移（8 px 上滑） | 全窗 + 重排 | **谨慎** | 只在页面切换时，且与淡入合并（不叠加缩放） |
| 模糊 / 阴影 | — | **禁止** | `BoxShadow` 每帧要重绘阴影位图；`BackdropBlur` 直接不可用（软件渲染） |
| 持续脉冲 / 无限旋转 | — | **禁止** | 会让 CPU 恒定占用，违反 ≤3% |

**降级策略**（按 `reduced-motion` 与性能预算）：

1. `reduced-motion == true` → 所有 `duration = 0ms`（现有机制，保持）。
2. 同一时刻只允许 **1 个** 高度动画（展开另一个时，前一个瞬切完成）。
3. 页面切换动画只在 `reduced-motion == false` 且窗口 `width * height < 1.6 Mpx`（1080×700@100% = 0.76 Mpx；@150% = 1.7 Mpx → 150% DPI 下关闭整页位移动画，只保留淡入）。

### 5.4 reduced-motion 与系统参数

现有实现已正确：`src/dwm.rs:128-140` 读 `SPI_GETCLIENTAREAANIMATION`，`Theme.reduced-motion` 驱动所有 `duration` 归 0。

要补一条 Magpie 的做法：**启动时跳过动画**。Magpie 在 `RootPage_Loaded` 里遍历视觉树，把每个 `ToggleSwitch` 的 `Storyboard.SkipToFill()`（`RootPage.cpp:74-101`）——避免首帧看到 30 个开关同时从"关"滑到"开"。我们对应：**首帧不播放任何 `animate`**（Slint 的 `animate` 只在属性**变化**时触发，初始赋值不触发，所以天然满足；但用 `states` 初始态时要注意不要在 `init` 后立刻改状态）。

### 5.5 Slint 实现片段

**（a）导航选中条位移**（单元素 + 绝对定位，不要每个 item 各画一条）：

```slint
// ui/app.slint —— 替换现有 for Button 段
// 注意：nav-rail 内 root 指向 AppWindow，所以行高要用 nav-rail.row，不能用 root.row
nav-rail := Rectangle {
    width: root.nav-width;
    background: Theme.nav;                  // mica 时 transparent

    property <length> row: 38px;            // 36 高 + 2 间距
    property <length> inset: 4px;           // WinUI: NavigationViewItemButtonMargin = 4,2

    for i in 8 : NavItem {
        x: nav-rail.inset;
        y: nav-rail.inset + nav-rail.row * i;
        width: nav-rail.width - 2 * nav-rail.inset;
        height: 36px;
        label: root.nav-labels[i];
        glyph: root.nav-glyphs[i];
        selected: AppState.page == i;
        activated => { AppState.command(10, i, 0); }
    }

    // 选中条：单个元素，只动 y
    Rectangle {
        x: nav-rail.inset;  width: 3px;  height: 16px;  border-radius: 2px;
        background: Theme.accent;
        y: nav-rail.inset + nav-rail.row * AppState.page + 10px;   // (36 - 16) / 2
        animate y { duration: Theme.dur-nav; easing: cubic-bezier(0, 0, 0, 1); }
    }
}
```

**（b）卡片悬停/按下**（颜色替换，83 ms）：

```slint
export component SettingsCard inherits Rectangle {
    in property <bool> interactive: true;
    height: 68px;
    border-radius: Theme.radius-card;
    border-width: 1px;

    ta := TouchArea {
        enabled: root.interactive;
        mouse-cursor: pointer;
    }

    background: !root.enabled ? Theme.card-disabled
              : ta.pressed   ? Theme.card-pressed
              : ta.has-hover ? Theme.card-hover
              :                Theme.card;
    border-color: ta.pressed   ? Theme.stroke-strong
                : ta.has-hover ? Theme.card-stroke-hover
                :                Theme.card-stroke;

    animate background   { duration: Theme.dur-hover; easing: ease-out; }
    animate border-color { duration: Theme.dur-hover; easing: ease-out; }
}
```

**（c）展开/收起（非对称时长必须用 `states` + `transitions`）**：

```slint
body := Rectangle {
    clip: true;
    states [
        collapsed when !root.expanded : { height: 0px; }
        expanded  when  root.expanded : { height: root.body-height; }
    ]
    transitions [
        collapsed -> expanded : { animate height { duration: 333ms; easing: cubic-bezier(0, 0, 0, 1); } }
        expanded -> collapsed : { animate height { duration: 167ms; easing: cubic-bezier(1, 1, 0, 1); } }
    ]
}
```

> **为什么用 `states` 而不是 `animate height { duration: a ? 333ms : 167ms; }`**：`duration` 接受任意表达式（可以三元），但 `easing` 不是运行期值——`cubic-bezier()` 在编译期解析为常量，三元表达式里的两个不同曲线无法通过类型检查。`states`/`transitions` 是唯一可靠的写法。
>
> **chevron 旋转**：`Path { transform-rotation: root.expanded ? 180deg : 0deg; animate transform-rotation { duration: 167ms; easing: ease-out; } }`（`transform-rotation` 是 1.19 的正式名，`rotation-angle` 是废弃别名，见 `i-slint-compiler/builtin_elements.rs:898`）。若旋转原点不在中心，显式写 `transform-origin-x/y`。**首次实现必须实测编译**；不行就退化为两个字形交叉淡入。

---

## 6. 给 `crates/msilt-ui` 的逐项改造清单

### 6.1 总策略：区分"设置型页面"与"数据型页面"

Magpie 是一个 100% 的设置应用。我们不是。**照搬全套会把数据页变成一列 68 px 卡片，信息密度反而下降。**

| 页面类型 | 页面 | 借什么 |
|---|---|---|
| **设置型**（绝大多数控件是"一个开关/一个下拉"） | 系统与工具、显卡与显示、电池与电源、监控与 OSD | **全套**：`SettingsGroup` + `SettingsCard` + `SettingsExpander` + `InfoBar` |
| **编辑器型**（大面积自绘画布 + 数值微调） | 性能与风扇、键盘灯效 | 只借 **token + 两级节奏 + Expander**；画布保持自研 |
| **数据型**（栅格指标 + 趋势） | 总览 | 借 **token + 68 px 三点式卡片**；保留栅格与趋势图 |
| **列表型**（重复条目 + 详情） | 自动化 | 借 **`SettingsExpander`**（规则即 Expander）+ 双栏 |

### 6.2 文件级改造矩阵

| 文件 | 动作 | 说明 |
|---|---|---|
| `ui/theme.slint` | **改** | §4.6 的 token 替换 + 新增 `gap-card`/`gap-group`/`gap-group-head`/`card-h`/`dur-*` |
| `ui/app.slint` | **改** | 导航轨换 `NavItem`；内容层改 `LayerFillColorDefault`；页头下间距 16；页面栈 `spacing: Theme.gap-group` |
| `ui/components/settings_card.slint` | **新增** | 三点式横排卡片（§6.3） |
| `ui/components/settings_group.slint` | **新增** | 组标题 + 2 px 栈（§6.4） |
| `ui/components/settings_expander.slint` | **新增** | 可折叠（§6.5） |
| `ui/components/nav_item.slint` | **新增** | 导航项 + 3×16 选中条（§5.5a） |
| `ui/components/key_chip.slint` | **新增** | 40×36 快捷键徽章（§3.5） |
| `ui/components/compact_card.slint` | **改** | 保留给"指标瓦片"，高度 104 → 68，改为三点式 |
| `ui/components/banner.slint` | **改** | 语义色换 `SystemFillColor*Background`；高 34 → 40 |
| `ui/components/rule_card.slint` | **改** | 改造成 `SettingsExpander` 形态（折叠 68 / 展开诊断） |
| `ui/components/primitives.slint` | **改** | `SectionLabel` 12/600 → **14/600**；`Chip` 圆角 9 → 4（Win11 的 Chip 是 4 px 圆角小方块，不是药丸） |
| `ui/components/status_pill.slint` | **改** | 保持药丸（顶栏状态是例外，Win11 顶栏也用胶囊） |
| `ui/pages/*.slint` | **改** | 见 §6.9 映射表 |

> `SectionLabel` 的 12 → 14 是关键：Magpie 的组标题是 14/600，我们的是 12/600。**组标题比卡片标题（14/400）更重**，这是层级的来源；我们现在组标题比卡片标题还小，所以"没有段落感"。

### 6.3 `settings_card.slint` 目标骨架

```slint
// 三点式横排卡片：图标 ≤20 → 标题/副标题 → 右侧控件（垂直居中）。
// 尺寸来自 Magpie SettingsCard：MinHeight 68、Padding 16、圆角 4、1px 描边、
// 图标右间距 20、标题区右间距 24、内容最小宽 200。
import { Theme } from "../theme.slint";
import { Icon } from "../icons.slint";

export component SettingsCard inherits Rectangle {
    in property <string> glyph: "";
    in property <string> title: "";
    in property <string> subtitle: "";
    in property <bool> enabled: true;
    in property <bool> interactive: false;   // 整卡可点击时置 true
    /// 副标题用三级文字
    in property <bool> muted: false;
    callback activated();

    height: Theme.card-h;                 // 68
    horizontal-stretch: 1;
    background: !root.enabled ? Theme.card-disabled
              : ta.pressed    ? Theme.card-pressed
              : ta.has-hover  ? Theme.card-hover
              :                 Theme.card;
    border-radius: Theme.radius-card;     // 4
    border-width: 1px;
    border-color: ta.pressed   ? Theme.stroke-strong
                : ta.has-hover ? Theme.card-stroke-hover
                :                Theme.card-stroke;
    animate background   { duration: Theme.dur-hover; easing: ease-out; }
    animate border-color { duration: Theme.dur-hover; easing: ease-out; }

    // enabled: false 时 has-hover 恒为 false，无需额外门控
    ta := TouchArea {
        enabled: root.interactive && root.enabled;
        mouse-cursor: pointer;
        clicked => { root.activated(); }
    }

    HorizontalLayout {
        padding-left: 16px;  padding-right: 16px;
        padding-top: 16px;   padding-bottom: 16px;
        spacing: 0px;
        alignment: center;                   // 交叉轴（垂直）居中，代替逐子项 vertical-alignment

        if root.glyph != "" : Icon {
            glyph: root.glyph;
            size: 20px;
            weight: 1.5px;
            tint: root.enabled ? Theme.text-2 : Theme.text-disabled;
        }
        if root.glyph != "" : Rectangle { width: 20px; background: transparent; }

        VerticalLayout {
            horizontal-stretch: 1;           // 吸收剩余宽度，把控件推到右侧
            spacing: 0px;
            Text {
                text: root.title;
                font-size: 14px;                 // ControlContentThemeFontSize
                font-weight: 400;                // 注意：Magpie 的卡片标题不是 SemiBold
                color: root.enabled ? Theme.text : Theme.text-disabled;
                overflow: elide;
            }
            Text {
                text: root.subtitle;
                font-size: 12px;
                color: !root.enabled ? Theme.text-disabled
                     : root.muted    ? Theme.text-3
                     :                 Theme.text-2;
                overflow: elide;
                visible: root.subtitle != "";
            }
        }
        Rectangle { width: 24px; background: transparent; }   // 标题区右边距

        HorizontalLayout {
            alignment: center;
            @children                        // 控件槽（右对齐）
        }
    }
}
```

**为什么标题是 400 而不是 600**：Magpie 的卡片标题继承 `ControlContentThemeFontSize` 且 `FontWeight=Normal`（`SettingsCard.Resource.xaml:100-102`）。层级由**位置 + 副标题 + 控件**表达，不由字重堆叠。我们现在的 `CompactCard` 用 600 标题 + 600 组标题，字重互相竞争，反而没有层级。

### 6.4 `settings_group.slint` 目标骨架

```slint
import { Theme } from "../theme.slint";

/// 组：标题 14/600 + 可选说明 12/400 + 8px + 卡片栈（间距 2）。
/// 组与组之间由父级 spacing: Theme.gap-group (24) 负责。
/// 注意：Slint 布局会接管子项位置，不能用 y:/x: 做偏移；
/// Magpie 的 Margin 1,0,0,0 用 padding-left: 1px 表达，8px 用外层 spacing 表达。
export component SettingsGroup inherits Rectangle {
    in property <string> title: "";
    in property <string> description: "";
    in property <bool> enabled: true;

    background: transparent;
    horizontal-stretch: 1;

    VerticalLayout {
        spacing: 8px;                        // ItemsPresenter Margin 0,8,0,0

        // 标题 + 说明（两者之间 0 间距，Magpie 同栈）
        VerticalLayout {
            spacing: 0px;
            padding-left: 1px;               // Magpie: Margin 1,0,0,0
            Text {
                text: root.title;
                font-size: 14px;             // BodyStrongTextBlockStyle
                font-weight: 600;
                color: root.enabled ? Theme.text : Theme.text-disabled;
                overflow: elide;
            }
            Text {
                text: root.description;
                font-size: 12px;             // CaptionTextBlockStyle
                color: root.enabled ? Theme.text-2 : Theme.text-disabled;
                wrap: word-wrap;
                visible: root.description != "";
            }
        }

        // 卡片栈：组内 2px
        VerticalLayout {
            spacing: Theme.gap-card;
            @children
        }
    }
}
```

### 6.5 `settings_expander.slint` 目标骨架

```slint
import { Theme } from "../theme.slint";

/// 折叠态 = 一个 68px SettingsCard；展开态 = 同一张卡片下方追加子行。
/// 关键：外层 clip: true + 单一半径 4 → 展开体的下圆角由父级裁剪自动成立，
/// 不需要 WinUI 的 CornerRadiusConverter。
export component SettingsExpander inherits Rectangle {
    in property <string> glyph: "";
    in property <string> title: "";
    in property <string> subtitle: "";
    in property <bool> expanded: false;
    in property <length> body-height: 0px;
    callback toggled();
    in-out property <bool> interactive: true;

    background: Theme.card;
    border-radius: Theme.radius-card;   // 4
    border-width: 1px;
    border-color: Theme.card-stroke;
    clip: true;
    horizontal-stretch: 1;

    VerticalLayout {
        spacing: 0px;

        // ── 头部：复用 SettingsCard 的三点式布局，右侧内边距 4 给 chevron
        header := Rectangle {
            height: 68px;
            background: header-ta.pressed ? Theme.card-pressed
                      : header-ta.has-hover ? Theme.card-hover
                      : Theme.card;
            animate background { duration: Theme.dur-hover; easing: ease-out; }

            // TouchArea 必须在布局**之前**声明（即位于底层），
            // 这样右侧控件（Switch/ComboBox）能先拿到点击，空白处才触发展开。
            header-ta := TouchArea {
                mouse-cursor: pointer;
                clicked => { root.toggled(); }
            }

            HorizontalLayout {
                padding-left: 16px;  padding-right: 4px;
                padding-top: 16px;   padding-bottom: 16px;
                alignment: center;               // 交叉轴居中
                // ... 图标 / 标题 / 副标题 / @children（右侧控件）
                Rectangle { width: 8px; background: transparent; }
                chevron := Rectangle {
                    width: 32px;  height: 32px;
                    border-radius: 4px;
                    background: header-ta.has-hover ? Theme.control-hover : transparent;
                    Path {
                        // chevron-down 字形；展开时旋转 180°
                        // （width/height/commands 由调用方给出，与 icons.slint 一致）
                        transform-rotation: root.expanded ? 180deg : 0deg;
                        animate transform-rotation { duration: 167ms; easing: ease-out; }
                    }
                }
            }
        }

        // ── 展开体
        body := Rectangle {
            clip: true;
            states [
                collapsed when !root.expanded : { height: 0px; }
                expanded  when  root.expanded : { height: root.body-height; }
            ]
            transitions [
                collapsed -> expanded : { animate height { duration: 333ms; easing: cubic-bezier(0, 0, 0, 1); } }
                expanded -> collapsed : { animate height { duration: 167ms; easing: cubic-bezier(1, 1, 0, 1); } }
            ]
            VerticalLayout {
                spacing: 0px;
                @children               // 子行由调用方给出（见下）
            }
        }
    }
}

/// 子行：52 高、左缩进 58（与头部标题严格对齐）、上 1px 分隔线。
/// Slint 不支持四值 border-width，分隔线用独立 1px Rectangle。
export component SettingsExpanderItem inherits Rectangle {
    height: 52px;
    background: transparent;

    VerticalLayout {
        spacing: 0px;
        Rectangle { height: 1px; background: Theme.divider; }
        HorizontalLayout {
            padding-left: 58px;  padding-right: 44px;
            padding-top: 8px;    padding-bottom: 8px;
            // ... 子行内容
        }
    }
}
```

> `body-height` 由调用方按子行数算出：`52px * n`。不要用 `preferred-height`（Slint 的布局反馈会形成循环依赖）。

### 6.6 `app.slint` 壳层改造（具体 diff 建议）

| 位置 | 现状 | 改为 |
|---|---|---|
| `Window.background` | `Theme.bg` | 不变（`transparent`，让 DWM 画 Mica） |
| 顶栏 | 40 高、`Theme.topbar` 不透明 | 不变；产品名 13 → **12** |
| 导航 `Rectangle.background` | `Theme.nav`（92% 黑） | `mica ? transparent : #191919` |
| 导航 `padding` | 6 | **4** |
| 导航 `spacing` | 4 | **2** |
| 导航项 | `Button { checkable }` | **`NavItem`**（36 高、4 圆角、3×16 选中条） |
| 导航分隔线 | `Rectangle { width: 1px; background: Theme.divider }` | 删除（Magpie 无导航/内容分隔线，靠亮度阶梯） |
| 内容 `Rectangle.background` | `Theme.content`（`#202020`） | **`Theme.content`（`#282828`）** |
| 内容 `padding` | `Theme.page-pad`（12） | 12 左右 / **8 上** / 12 下 |
| 页头 | 32 高，标题 20/600 + 说明 12/400 | 保持；**下间距 16** |
| 页面栈 `spacing` | `Theme.gap`（8） | **`Theme.gap-group`（24）** |
| `body-h` 公式 | `height - chrome - 2*pad - 32 - gap` | 重算：`height - chrome - 2*pad - 32 - 16` |

> 删除"导航/内容之间的 1 px 分隔线"是本次改造里最容易被忽略、但视觉收益最大的一步：Magpie 用 `NavigationViewContentGridBorderThickness=1,0,0,0` 保留了这条线，但它的颜色是 `CardStrokeColorDefault`（深色 `#242424`），**比内容层 `#282828` 更暗**，所以看起来是"内容层压在导航上"而不是"两个区域被一条线切开"。我们现在的 `#2C2C2C` 比内容 `#202020` **更亮**，方向反了。

### 6.7 `InfoBar` 与 `Banner` 的分工

| 场景 | 组件 | 位置 | 高度 |
|---|---|---|---|
| daemon 断连、只读固件、恢复必需、待重启 | `Banner`（保留） | 顶栏下，全局 | 40 |
| "此控件被官软占用" | `InfoBar`（新增） | 紧跟该卡片下方，间距 2 | 自适应（≈48） |
| "修改将在重启后生效" | `InfoBar` | 紧跟该卡片下方 | ≈48 |
| "v1 无此端点" | 不要 InfoBar | 用卡片标题旁的 `Chip` | — |

理由：全局 banner 挤压所有页面（`chrome-h` 里 +40），只应承载**跨页**事实；局部原因必须**贴着**它解释的控件，否则用户要来回找。

### 6.8 八页映射表

| 页面 | 现状 | 改造 | 优先级 |
|---|---|---|---|
| **总览** `dashboard` | 6 张 104 高纵排卡 + 趋势 + KV 行 | 卡片改 **68 高三点式**（`SettingsCard`），2 列栅格；趋势卡保留（圆角 4、描边 `#242424`）；KV 行改 19 高 | P0 |
| **性能与风扇** `performance` | 模式/强冷卡 + 预设行 + 双画布 + 指标 | 顶部两卡 → `SettingsCard` 68；预设行 → `SettingsGroup`（标题"曲线预设"）+ 3 个 `SettingsExpanderItem`；画布卡圆角 6→4、描边换 token | P0 |
| **显卡与显示** `display` | 卡片 + 分段 + 说明 | **全改** `SettingsGroup` + `SettingsCard`；MUX 只读卡 → `SettingsCard` + `Chip`；警告 → 内联 `InfoBar` | P0 |
| **电池与电源** `battery` | 大字卡 + 指标 + 明细 | 电量卡保留（自研）；充电上限/电源计划 → `SettingsCard`；明细 → `SettingsGroup` | P1 |
| **键盘灯效** `lighting` | 矩阵 + 右栏 3 卡 | 矩阵保留；右栏 → `SettingsGroup`（亮度/预设/设备）；冲突 → danger 描边 + 内联 `InfoBar` | P1 |
| **自动化** `automation` | 头部卡 + `RuleCard` 列表 + 右栏 | **`RuleCard` → `SettingsExpander`**：折叠 68（名称 + 优先级 Chip + 结果 Chip + 开关 + 一行诊断摘要），展开显示三列诊断 + 触发/动作 + 最近触发；右栏 → `SettingsGroup` | **P0（收益最大）** |
| **监控与 OSD** `monitor` | 卡片 | **全改** `SettingsGroup` + `SettingsCard`；OSD 热键用 `KeyChip` 徽章 | P1 |
| **系统与工具** `system` | 卡片 | **全改**（最接近 Magpie 设置页）；版本/诊断 → `SettingsExpander` | P0 |

**自动化页的改造前后对比**（密度收益量化）：

| | 现状 | 改造后 |
|---|---|---|
| 单条规则折叠高 | ≈ 104（名称 24 + 触发 18 + 动作 18 + 诊断 18 + 最近触发 16 + 间距） | **68** |
| 一屏可见（body-h ≈ 560） | 5 条 | **8 条** |
| 诊断可见性 | 常驻（占 18 px） | 折叠时 1 行摘要，展开看全量 |

### 6.9 分阶段落地

| 阶段 | 内容 | 验收 |
|---|---|---|
| **P0-1** | `theme.slint` token 替换（§4.6）+ 新增间距/时长 token | 全部页面**布局不变**，仅颜色变化；`cargo check` 0 warning |
| **P0-2** | `settings_card` / `settings_group` 两个新组件 + 3 个页面迁移（`system`/`display`/`monitor`） | 三页视觉对齐 Magpie；一屏信息量不下降 |
| **P0-3** | `nav_item` + 导航轨改造（含选中条动效） | 选中条 150 ms 位移；`reduced-motion` 下瞬切 |
| **P1-1** | `settings_expander` + `automation` 页 `RuleCard` 迁移 | 折叠 68 / 展开诊断；一屏 8 条 |
| **P1-2** | `key_chip` 徽章 + `monitor`/`system` 热键展示 | 40×36 键帽，间距 4 |
| **P1-3** | 内联 `InfoBar` 形态 + 5 处局部原因迁移 | 局部原因不再走全局 Banner |
| **P2** | 系统 Accent 读取（`AccentPalette`）+ 页面切换淡入 | 默认蓝下 `Light2 == #60CDFF` 断言通过 |

---

## 7. 验收清单（可机械检查）

| # | 检查项 | 判定 | 方法 |
|---|---|---|---|
| 1 | 内容层与卡片颜色差 ≥ 8 亮度级 | 深色 `#282828` vs `#333333` | 截图像素采样 |
| 2 | 卡片描边比卡片更暗（深色） | `#242424` < `#333333` | 采样描边像素 |
| 3 | 组内卡片间距 = 2 | 相邻卡片边缘像素距离 | 截图量距 |
| 4 | 组间 ≥ 10× 组内间距 | 24 / 2 = 12 | 同上 |
| 5 | 组标题字号 > 卡片标题字号 | 14/600 vs 14/400（字重严格大于） | 代码审查 |
| 6 | 卡片高度 = 68（设置型） | 逐页量 | 截图 |
| 7 | 卡片圆角 = 4 | — | 代码审查（无 5/6/10） |
| 8 | 子行缩进 = 58，与头部标题左边缘像素对齐 | 截图对齐 | 截图 |
| 9 | 导航选中条 3×16、圆角 2、accent | — | 采样 |
| 10 | 所有动效时长 ∈ {83, 150, 167, 333} ms | 代码审查 | `grep duration` |
| 11 | `reduced-motion` 下全部 0 ms | 开关系统"动画效果"后无过渡 | 手工 |
| 12 | 无 `BoxShadow` / `BackdropBlur` / `@linear-gradient` | `grep` 为 0 | `grep` |
| 13 | 文字对比度：主 ≥ 12:1、副 ≥ 7:1、三级 ≥ 4.5:1（深色） | 见 §4.4 | 计算 |
| 14 | 浅色三级文字不承载信息 | 人工审查 | — |
| 15 | 空闲 CPU ≤ 3%、展开动画期间 ≤ 8% | 性能计数器 | `tools/capture.ps1` |

---

## 8. 风险与开放问题

| # | 风险 | 影响 | 缓解 |
|---|---|---|---|
| 1 | 内容层改半透明后，饱和壁纸会染色（v1.1 §3.3 已实测"青绿染色"） | 卡片可读性下降 | `LayerFillColorDefault` 是 30% 灰而非 30% 白，染色幅度小于我们 v1.1 试过的方案；若实测仍偏色，退化为 `content: mica ? #282828 : #202020`（用合成后的**实色**，仍保留阶梯但不透壁纸）。**这是唯一必须真机验证的改动。** |
| 2 | Slint 无四值 `border-width` / 单值 `border-radius` | 无法 1:1 复刻 Expander 的"上圆角/下圆角"与"仅上边分隔线" | 外层 `clip: true` 自动裁剪下圆角；分隔线用 1 px `Rectangle`。已在 §6.5 给出定稿 |
| 3 | `transform-rotation` 在 `Path` 上的旋转原点 | chevron 旋转可能绕左上角 | 首次实现必须实测；不行则退化为两字形淡入 |
| 4 | 高度动画触发逐帧布局 | 展开时 CPU 峰值 | 单页同时只允许 1 个展开动画；`reduced-motion` 下瞬切 |
| 5 | `SectionLabel` 12 → 14 会让所有页面的纵向高度 +2~4 px | 可能触发 `--check-layout` 的高度断言失败 | 与 `page-min-h` 同步调整；`tools/check-min-size.ps1` 复跑 |
| 6 | `SettingsCard` 标题改 400 字重后，弱视用户可能觉得"层级不清" | 可访问性 | 副标题保持 `text-2`（8.27:1）；组标题 600 提供段落锚点；不靠字重区分单卡 |
| 7 | 系统 Accent 读取（P2）在 `AccentPalette` 下标顺序上存在机型差异 | 强调色错位 | 用默认蓝做断言（`Light2 == 60CDFF`），不通过则回退常量 |

**开放问题（需要用户裁决）**：

1. 内容层是否接受壁纸染色（风险 1）？选项 A：接受（Magpie 原生）；选项 B：用合成实色 `#282828`（阶梯在、不透明）。
2. 是否保留 `CompactCard` 的纵排形态用于"指标瓦片"（大数字 + 单位），还是统一成 68 三点式？建议保留（指标需要大字）。
3. `automation` 页的 `RuleCard` 折叠后是否保留"一行诊断摘要"？保留则 68 px 内要放 4 个元素，需要压缩优先级 Chip 的宽度。

---

## 附录 A：源码索引（结论 → 证据）

| 结论 | 文件:行 |
|---|---|
| 内容层 0 圆角 + 1 px 左缝 | `src/Magpie/RootPage.xaml:57-58` |
| 导航展开阈值 920、紧凑阈值 0、无返回键 | `RootPage.xaml:48-51` |
| 导航组标题（`NavigationViewItemHeader`） | `RootPage.xaml:80` |
| 页脚"关于"项 | `RootPage.xaml:192-199` |
| 展开态隐藏汉堡按钮、导航滚动区避让标题栏 | `RootPage.cpp:236-247` |
| 启动跳过 ToggleSwitch 动画 | `RootPage.cpp:74-101` |
| 版心 1000 / 页头 40,54,40,16 / 标题 28 | `PageFrame.xaml:12,26,39` |
| 正文 `Padding 40,0,40,0` / 底边距 20 | `PageFrame.xaml:68,76` |
| 响应式边距 590 → 25 | `PageFrame.cpp:31` |
| 标题栏 40 高 / 16,10 / 12 px / 220 ms 位移 | `TitleBarControl.xaml:8,14,21,57-79` |
| Caption 按钮 46×32 / 6% / 4% / 35% | `CaptionButtonsControl.xaml:66-67,24-46` |
| 卡片最小高 68 / 内边距 16 / 图标 20 / 内容最小宽 200 | `SettingsCard.Resource.xaml:68-79` |
| 卡片四态刷子 | `SettingsCard.Resource.xaml:10-34` |
| 卡片状态瞬切（无 BrushTransition） | `SettingsCard.Resource.xaml:236-320` |
| 卡片换行阈值 476 / 286 | `SettingsCard.Resource.xaml:78-79` |
| 组标题 14/600 + 说明 12/400 + 8 px | `SettingsGroup.Resource.xaml:32-54` |
| 组内 2 px / 组间 32 px | `App.xaml:84-85` |
| 列表重排动画不交错 | `App.xaml:80-82` |
| 行内标准图标 14 | `App.xaml:87` |
| `InfoBar` 图标 0,16,22,16 / 内容 20,0,0,0 | `App.xaml:90-91` |
| Subtle 按钮 83 ms `BrushTransition` | `App.xaml:141` |
| Expander 头部 16,16,4,16 / 子行 58,8,44,8 / 分隔线 0,1,0,0 | `SettingsExpander.Resource.xaml:10-16` |
| Expander 子行 52 高 / 圆角 0 | `SettingsExpander.Resource.xaml:25-27` |
| Expander 展开 333 ms / 收起 48+167 ms | `SettingsExpander.Resource.xaml:176-217` |
| 键帽 40×36 / 14 SemiBold / 圆角 4 | `KeyVisual.Resource.xaml:82-89,36-37` |
| 快捷键内边距 6,0,6,0 / 间距 16 / 键间距 4 | `ShortcutControl.xaml:37,39,42` |
| InfoBar 信息级蓝底 `#34424D` / `#D3E7F7` | `BlueInfoBarStyle.xaml:8,12` |
| 危险按钮 Firebrick + 不透明度 0.9/0.8 | `CriticalButtonStyle.xaml:4-10` |
| Mica = `DWMSBT_MAINWINDOW` + `WS_EX_NOREDIRECTIONBITMAP` | `MainWindow.cpp:648-650,568` |
| 默认窗口 980×690 / 最小 500×300 | `MainWindow.cpp:602,285-286` |
| `LayerFillColorDefault` = `#4C3A3A3A`（深）/ `#80FFFFFF`（浅） | `Common_themeresources_any.xaml:60,264` |
| 卡片/描边/文字/语义色全套 ARGB | `Common_themeresources_any.xaml:5-87,209-300` |
| accent = `SystemAccentColorLight2`（深）/ `Dark1`（浅） | `Common_themeresources_any.xaml:125,329` |
| 导航内容层默认 `8,0,0,0` + `1,1,0,0` | `NavigationView_themeresources.xaml:234,263` |
| 导航项 36 高 / margin 4,2 / 选中条 3×16 r2 | `NavigationView_themeresources.xaml:217,228,220-222,602` |

## 附录 B：许可证与红线

| 项 | 结论 |
|---|---|
| Magpie 许可证 | **GPL-3.0**。本文**不复制**其任何源代码、XAML 模板、资源字典、图标或字符串。 |
| 本文引用的"事实" | 数值（68、2、4、`#4C3A3A3A`…）、资源名（`CardBackgroundFillColorDefault`）、参数名（`SettingsCardSpacing`）属于**事实性数据**，不受版权保护；引用时只保留参数与数值，不保留结构。 |
| 上游 MIT 来源 | Magpie 的 `Settings*` 模板本身移植自 PowerToys / CommunityToolkit（MIT），颜色来自 `microsoft-ui-xaml`（MIT）。本文的颜色与尺寸数值实际上溯到这两个 MIT 项目，法律风险更低；但仍以"独立实现"为准。 |
| 我们的实现 | 全部 Slint 代码独立编写；图标继续使用自绘 Fluent 风格 `Path`（`ui/icons.slint`），**不**引入 Segoe Fluent Icons 字体（避免字体授权与豆腐块风险）。 |
| 文档中的引用 | 只引用**行号 + 单行属性值**作为证据，不整段转载 XAML。 |

## 附录 C：本文与 `UI_DESIGN_SPEC.md` 的关系

| 项 | `UI_DESIGN_SPEC` v1.1 | 本文 | 处理 |
|---|---|---|---|
| 三条硬标准（零 emoji / 复用组件 / 零大空洞） | 裁决依据 | 完全遵守 | 本文所有建议均不引入 emoji、不重造基础控件（`SettingsCard` 是布局容器，内部控件仍来自 `std-widgets`）、不增加大留白 |
| §3.3 "顶栏与内容区不透明" | 实测结论 | 建议改为 `LayerFillColorDefault` | **需用户裁决**（§8 风险 1）；本文提供两种落地 |
| §3.5 卡片圆角 6 | normative | 建议 4 | 与 Magpie/WinUI 一致；需同步修订 §3.5 |
| §3.5 导航项 32 | normative | 建议 36 | 与 WinUI 一致 |
| §3.6 动效 80/120/160 | normative | 建议 83/150/167/333 | 与 Magpie/WinUI 一致 |
| §4.5 "导航项左侧 3×16 accent 条" | 已写明 | 代码未实现 | 本文 §5.5a 给出实现 |
| §4.2 `CompactCard` 纵排 | 已交付 | 设置型页面改横排 | `CompactCard` 保留给指标瓦片 |
