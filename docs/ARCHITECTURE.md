# MSI-Toolkit 总架构蓝图（Architecture Master Spec）

> **目标机型（Phase 0/1 首发验证机）**：MSI Vector 17 HX AI A2XWJG（主板 `MS-17S3 REV:1.0`、BIOS `E17S3IMS.112`、EC `17S3EMS1.107`、Core Ultra 9 275HX、RTX 5090 Laptop、内屏 `AUO84A6` 2560×1600 60/240Hz、24 区键盘灯 `HID\VID_1462&PID_1603`）。
> **定位**：仿照并超越 [LenovoLegionToolkit](https://github.com/LenovoLegionToolkit-Team/LenovoLegionToolkit)（LLT）的 UI 质感、信息架构与自动化能力，打造一款**零第三方内核驱动、全周期极致低占用、独立开源干净实现**的 MSI 笔记本原生控制中心。

---

## 1. 不可妥协的架构铁律（Hard Constraints）

1. **零第三方内核驱动（HVCI / Secure Boot / 反作弊 100% 安全）**
   - 严禁引入 `WinRing0`、严禁依赖易入黑名单的 `MsIo64.sys`/`NTIOLib`。
   - EC 通信唯一主通道：Windows 原生 `wmiacpi.sys` 承载的 `root\WMI:MSI_ACPI`（由本机已注册的 MSI 签名 `msiapcfg.dll` 提供 MOF Schema，不二次分发任何 MSI 签名二进制）。
   - 24 区键盘 RGB：纯用户态 Win32 `HidUsb`（`VID_1462&PID_1603`，Usage Page `0x00FF`，64 字节 Feature/Output 报告）。
   - 显卡控制与监控：动态加载本机已有 `nvml.dll` / `nvapi64.dll`。
   - 显示与色彩：Win32 CCD (`QueryDisplayConfig` / `ChangeDisplaySettingsExW`) + `WmiMonitorBrightness` + Windows WCS/ICM 切换官方出厂 `.icm` 校色文件。
   - 电池与电源：Win32 `IOCTL_BATTERY_QUERY_INFORMATION` + Power Management API + `RegisterPowerSettingNotification`。
   - OSD：纯 CPU GDI+ 渲染预乘 ARGB32 软件位图，经 `UpdateLayeredWindow(ULW_ALPHA)` 提交到非注入分层窗（`WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_NOACTIVATE`）。渲染路径零 Direct3D、零 Direct2D、零显存分配；绘制后立即释放位图，不在后台预热 GDI+。

2. **极致低资源双体进程架构（突破单进程 UI 内存与独显唤醒天花板）**
   - **`msilt-daemon.exe`（常驻守护与硬件服务体）**：
     - **纯 Rust + 原生 Win32 API，严禁链接 Slint / Skia / OpenGL / Direct3D / WebView2**。
     - 负责：原生 `Shell_NotifyIconW` 系统托盘与快捷右键菜单、纯事件驱动监听器（电源/显示/前台游戏/热键/会话/WMI 事件）、Automation 自动化引擎、全局串行化硬件抽象层、本地命名管道 IPC Server（`\\.\pipe\MSIToolkit-IPC-v1`）、原生轻量化分层窗 OSD / 屏幕准星、按需拉起 UI。
     - **资源硬指标**：初始化后断开最后客户端、无在途 HAL、冷却后静置 15 秒，Working Set **≤ 12 MiB**、Private **≤ 4 MiB**、累计 CPU 时间增量 **0 ms**。空闲使用 `IDLE_PRIORITY_CLASS` + Windows **EcoQoS**；连续空闲 2 秒后借用 IOCP 的单次截止时间修剪本进程的冷页，持续空闲时不轮询。实际数值和测量口径见 [集成报告](INTEGRATION_REPORT.md#7-空闲工作集优化与一键验收)。
     - **本机 Release 静置实测**：最终总验的隔离只读端点 Working Set **0.3203125 MiB**、Private **3.55859375 MiB**、**16 线程 / 287 句柄**，15 秒累计 CPU 时间增量 **0 ms**；PE 普通与延迟导入均无 Direct3D、Direct2D 或 DXGI，静置进程未加载 NVML/NVAPI。
     - **自启与提权**：单点提权，通过计划任务 `LogonTrigger`（`RunLevel=Highest`）静默自启，零 Windows 服务常驻，零 UAC 弹窗。
   - **`msilt-ui.exe`（按需启动的 Slint Fluent 2 控制中心）**：
     - **非提权进程**，用户点击托盘或热键时按需启动，通过命名管道连接 `msilt-daemon`，关闭窗口即退出。
     - **Slint 1.19.1 `renderer-software` + `backend-winit`** 负责 CPU 光栅化，Windows 11 DWM **Mica** 负责窗口底材合成；Mica 不可用时回退为不透明底色。Fluent 2 控件跟随系统深浅主题。
     - **本机开窗实测**：Private **11.7–11.8 MiB**（约 12 MiB）、Working Set **33.0 MiB**、首帧 **101–118 ms**（约 110 ms）、**0 显存**、关窗后 **0 残留进程**。发布门槛：Working Set **≤ 45 MiB**、首帧 **< 250 ms**。测量详见 [UI 设计规范 §9](UI_DESIGN_SPEC.md#9-性能预算与实测方法)。
   - **`msilt-cli.exe`（非提权命令行工具）**：毫秒级直连命名管道，便于外部脚本/快捷键/StreamDeck 联动。
   - **`msilt-probe.exe`（Phase 0 纯只读硬件实证工具）**：不执行任何 EC/UEFI 写入，采集本机 `MSI_ACPI` / EC / HID / NVML / 显示 / 电池 / UEFI 真实契约数据。

3. **硬件写安全铁律（严防砖机与双写竞争）**
   - **全局串行锁 + 看门狗**：ACPI-WMI 固件方法非线程安全，所有 WMI 调用必须经单线程串行执行器 + 超时保护（防止固件挂死拖垮主事件循环）。
   - **数据驱动 `ec_map.json`**：绝不硬编码 EC 地址，所有写入操作必须查表匹配 `EC 固件前缀`（如 `17S3EMS1.1`），**未知固件强制只读降级**。
   - **三重写入保护**：
     1. 硬编码 `denyWrite` 黑名单（永不写入 `0x50` / `0x51` PL1/PL2 等危险或覆盖 BIOS 功耗墙的地址）；
     2. 写前快照原值 + 写后回读校验 + 失败自动回退；
     3. GPU MUX 切换等高危操作在 Phase 0 未完全证实本机精确变更路径前禁止开放写入口；开放后必须采用“预约下次重启生效、不强制立即重启”并带安全校验门禁。
   - **冲突软件停用器（SoftwareDisabler）**：内置一键安全停用 & 随时无损还原 MSI 官软 7 个服务 + 计划任务 + Run 键，杜绝双写 EC。

4. **许可证与合规红线**
   - LLT 与 GhostDeck 为 GPL-3.0，msi-ec 为 GPL-2.0。**严禁直接复制任何 GPL 源码**；仅参考公开的硬件寄存器事实数据、协议规范与架构思路，全部 Rust/Slint 代码独立编写。

---

## 2. Cargo Workspace 模块边界与依赖方向

```
msilt-daemon     ──▶ msilt-core, msilt-hal, msilt-automation
msilt-probe      ──▶ msilt-hal
msilt-hal        ──▶ msilt-core
msilt-automation ──▶ msilt-core
msilt-ui         ──▶ msilt-core
msilt-cli        ──▶ msilt-core
```

- `crates/msilt-core`：纯 Rust 零 Windows UI 依赖。领域状态模型、能力位掩码（`CapabilityFlags`）、`ec_map.json` 解析与 `WriteGuard`、IPC 协议（Length-prefixed JSON/bincode over Named Pipe）、配置持久化、日志模型。
- `crates/msilt-hal`：Windows 底层硬件交互（WMI-ACPI COM、24-Zone HID、NVML/NVAPI、Display CCD/Brightness/WCS ICM、Battery IOCTL、Windows Service/Task Disabler），实现 `msilt-core` 定义的 Trait，内置 `Mock` 供测试。
- `crates/msilt-automation`：纯事件驱动的 Trigger / Condition (AND/OR) / Action 规则引擎 + 环境变量注入脚本执行器 + 场景状态机。
- `crates/msilt-daemon`：原生 Win32 消息循环、EcoQoS 与空闲工作集修剪、托盘/热键、GDI+ 分层窗 OSD、IOCP 命名管道 Server、按需拉起 `msilt-ui`。
- `crates/msilt-ui`：应用架构仅依赖 `msilt-core` 的 IPC 契约、Slint 软件渲染和 DWM/窗口/命名管道所需的最小 Win32 API；序列化与窗口句柄适配为薄辅助层。**禁止直接或间接依赖 `msilt-hal`**，所有硬件读写通过 daemon 完成。
- `crates/msilt-cli`：命令行子命令解析与 IPC Client。
- `crates/msilt-probe`：Phase 0 只读探测器（可输出脱敏硬件契约 JSON 报告）。

---

## 3. 分阶段交付路线

- **Phase 0（架构定稿 + 只读硬件验证）**：
  - 产出 `HARDWARE_PROTOCOLS.md`、`CONTRACTS.md`、`UI_DESIGN_SPEC.md`、`AUTOMATION_SPEC.md` 并完成跨模型互审；
  - 搭好 Cargo Workspace 与 `msilt-core`、`msilt-probe`；
  - 在本机比较 Slint 软件、Skia、FemtoVG 与 WGPU 后端，选定 `renderer-software` + Win11 DWM Mica；
  - 用户授权运行一次只读 `msilt-probe`，100% 锁死 `17S3EMS1.107` 的 WMI/EC/风扇/RPM/HID/MUX 真实读数与协议格式。
- **Phase 1（核心闭环 MVP — 替代 80% 日常功能）**：
  - `msilt-daemon` 纯 Win32 常驻体（托盘 + EcoQoS + IPC + MSI 服务停用器）；
  - 完整 `msilt-hal`（场景档位、6 点风扇曲线、Cooler Boost、充电上限、General Settings、显示 60/240Hz、True Color 14 个 `.icm` 切换、NVML/NVAPI 监控与超频）；
  - `msilt-ui` Fluent 2 + Mica 主窗口（Dashboard / 性能风扇 / 电池显示 / 设置）。
- **Phase 2（全功能补齐 — 24 区 RGB + OSD/准星 + GPU MUX）**：
  - 24 区键盘灯效 (`1462:1603`) HID 驱动与实时矩阵 UI；
  - 在已落地的 CPU 软件位图分层窗通知上扩展 FPS/温度/功耗悬浮与屏幕准星；
  - 经验证安全的 GPU MUX 预约切换。
- **Phase 3（超越官软 — Automation 规则引擎 + CLI + 极致压测）**：
  - LLT 级 Automation 管道（AC/DC、游戏进程、外接屏、定时、电量联动）；
  - `msilt-cli` 完整命令集；
  - 内存/CPU/冷启动/独显休眠专项压测与极致调优。
