# MSI-Toolkit 自动化引擎 / 原生 OSD / 官软停用器 技术规范（AUTOMATION_SPEC）

| 项 | 值 |
|---|---|
| 文档 ID | `MSILT-SPEC-AUTOMATION` |
| 版本 | `v1.0.0-draft.1` |
| 负责人 | `@Pi-Auto`（独占所有权） |
| 覆盖 crate | `msilt-automation`（规则引擎）、`msilt-daemon`（OSD/准星/事件宿主）、`msilt-hal`（停用器后端，仅接口契约） |
| 上游约束 | `docs/ARCHITECTURE.md` §1 四条铁律（零第三方内核驱动 / 双体低占用 / EC 写安全 / 许可证红线） |
| 状态 | Phase 0 契约冻结草案；标注 `[待证]` 的条目必须由 `msilt-probe` 或 Phase 1 实证回填后才可升级为 `[已证]` |

> **本文档的措辞约定**：`必须` / `禁止` 为强制项，有违反即为 blocker；`应` 为默认项，偏离需在 `docs/collab/04-pi-auto.md` 记录理由；`[实测]` 表示本机取证（附录 C），`[待证]` 表示尚无本机证据。

---

## 0. 实测环境基线（本次取证快照）

| 事实 | 值 | 取证方式 |
|---|---|---|
| OS | Windows 11 家庭版 中文版，`10.0.26200` | `Win32_OperatingSystem` |
| 机型 / 平台 | MSI Vector 17 HX AI A2XWJG，`MS-17S3` | `docs/ARCHITECTURE.md` |
| 内存 | 33,207,572 KiB 可见 | `Win32_OperatingSystem` |
| 内屏 | `DISPLAY\AUO84A6\5&897be1c&0&UID4353_0`，2560×1600，**仅 60Hz / 240Hz 两档**（`EnumDisplaySettings` 枚举） | 实测 |
| 外接显示器 | **当前无**（`Screen.AllScreens` 仅 1 个） | 实测 |
| 电池 | `BIF0_9`，当前 95%，`BatteryStatus=2`（AC 供电） | `Win32_Battery` |
| 电源方案 | 平衡 `381b4222-f694-41f0-9685-ff5bb260df2e` | `powercfg /getactivescheme` |
| 24 区键盘灯 | `USB\VID_1462&PID_1603\812BACD4C000` + `HID\VID_1462&PID_1603\6&1262B4E1&0&0000`，`OK` | `Get-PnpDevice` |
| EC 通道 | `root\WMI:MSI_ACPI` **存在**，方法含 `Get_EC/Set_EC/GetPackage/SetPackage/Get_Thermal/Set_Thermal/Get_Fan/Set_Fan/Get_Power/Set_Power/Get_MasterBattery/Set_MasterBattery/Get_SMBUS/Set_SMBUS/Get_BIOS_64/Set_BIOS_64/...`（共 39 个方法，签名均为 `(Data: MSI_* 实例) -> Boolean`） | `Get-CimClass` |
| 固件事件类 | `root\WMI:MSI_Event` 存在，属性 `{ Active, InstanceName[KEY], MSIEvt:UInt32, SECURITY_DESCRIPTOR:UInt8Array, TIME_CREATED:UInt64 }` —— **形态上是 WMI 事件类**，但 `WQL SELECT * FROM MSI_Event` 在非提权会话返回"无法分析的查询" | 实测，见 §3.8 与附录 C.3 **[待证]** |
| 参数对象类 | `MSI_Power / MSI_System / MSI_Device / MSI_CPU / MSI_VGA / MSI_Thermal*` 均**不可实例化枚举**（`Get-CimInstance` 报"不支持"），仅作为 `Data` 参数载体 | 实测 |
| WMI 宿主 | `WmiAcpi` 服务 `Start=3`（demand）、`C:\Windows\System32\drivers\WmiAcpi.sys` 57,344 B | 实测 |
| MOF 提供者 | `C:\Windows\SysWOW64\msiapcfg.dll` 16,488 B；`C:\Windows\System32\msiltcfg.dll` 45,056 B；`C:\Windows\SysWOW64\msiltcfg.dll` 17,920 B | 实测 |
| True Color 色彩文件 | `C:\Windows\System32\spool\drivers\color\` 下实际 **6 个** `TrueColor_*.icm`（`Adobe_6500` / `DCIP3_6300` / `DisplayP3_6500` / `Native_6500` / `sRGB_4400` / `sRGB_6500`）+ `RSWOP.icm` | 实测（**与 ARCHITECTURE 中"14 个 .icm"不一致，见 §5.3 与协作帖质疑 Q-07**） |
| ETW 可用性 | `Microsoft-Windows-DxgKrnl` 提供者存在，关键字 `Present = 0x08000000`；`Microsoft-Windows-Kernel-Process` 存在，关键字 `WINEVENT_KEYWORD_PROCESS = 0x10` | `Get-WinEvent -ListProvider` 实测 |
| 当前会话 | **非提权**（`Administrators` 仅 deny-only） | `whoami /groups` |

---

## 1. 设计公理（违反即 blocker）

1. **A1 零轮询核心**：`msilt-automation` 的任何内置 Trigger **不得**以固定周期 `Sleep` / `Timer tick` 采样式工作。允许的定时器只有一种形态：**"单一最近到期时间点"** 的 waitable timer（§3.7）。
2. **A2 事件不阻塞**：Win32 回调（`WinEventProc`、`POWER_SETTING_CALLBACK`、ETW `ProcessTrace` 回调、线程池回调）内**禁止**做任何 I/O、锁等待、WMI 调用，只允许 `post(RawEvent)`（有界 channel，`try_send`，满则按 §3.1 合并或丢弃并计数）。
3. **A3 单写者**：所有硬件写操作只能发生在 `HalExecutor` 线程（§2.2）。Automation 引擎**禁止**持有任何 HAL 句柄。
4. **A4 状态可回滚**：任何自动化产生的硬件状态变更**必须**以"租约（lease）"形式存在（§6.1），无租约的裸写代码在 code review 中直接拒绝。
5. **A5 EC 写预算**：任意 EC 写入通道受 `EcGovernor` 令牌桶约束（§4.4），**硬上限 4 次写 / 10s，同一字段最小间隔 250ms**。
6. **A6 零注入 / 零内存读写**：禁止 `OpenProcess` 取得 `PROCESS_VM_READ` / `PROCESS_VM_WRITE` / `PROCESS_VM_OPERATION`；禁止 `SetWindowsHookEx` 全局键鼠钩子；禁止 Hook 任何图形 API（§11.8）。
7. **A7 提权边界**：daemon 以 `Highest` 运行，因此**禁止**以高完整性级别执行任何用户可写的脚本；用户脚本必须降权到交互用户 `Medium` IL 执行（§5.4）。
8. **A8 只写 Windows 状态，不碰固件**：停用器（§12）只修改服务/任务/Run 键，**禁止**在停用流程中产生任何 EC 写。
9. **A9 能力门控**：每条规则的每个 Action 都绑定一个 `CapabilityFlag`；运行时能力缺失 → 规则标记 `DisabledUnsupported(reason)`，**不产生错误日志刷屏、不重试**。

---

## 2. 运行时拓扑

### 2.1 线程与所有权（daemon 内）

```
┌───────────────────────────────────────────────────────────────────────────┐
│ msilt-daemon.exe                                                          │
│                                                                           │
│  T0 Win32Pump      HWND(msilt.hidden) ── WM_POWERBROADCAST / WM_DISPLAYCHANGE│
│                    SetWinEventHook / WTS / RegisterHotKey / waitable timer │
│                    硬约束：单个消息处理 ≤ 2ms，只做 post()                  │
│                                    │ RawEvent (bounded 256)                │
│                                    ▼                                      │
│  T1 AutomationEngine   Condition 求值 + 租约仲裁 + 计划生成（纯计算，无 I/O）│
│                                    │ ActionPlan (bounded 32)               │
│                                    ▼                                      │
│  T2 HalExecutor        唯一允许触碰 WMI-ACPI / HID / CCD / MSCMS 的线程     │
│                        异步 WMI + 超时看门狗 + EcGovernor                  │
│                                    │ ApplyReceipt                        │
│                                    ▼                                      │
│  T3 IpcServer          \\.\pipe\MSIToolkit-IPC-v1（最多 8 实例，IOCP）      │
│                        只读遥测走三缓冲快照；写请求以 UserPriority 入 T1      │
│                                                                           │
│  T4 OsdThread          【按需创建/销毁】分层窗 OSD、准星、动画              │
│  T5 TraceThread        【按需创建/销毁】ETW real-time 会话（FPS / 进程事件）  │
└───────────────────────────────────────────────────────────────────────────┘
```

- **空闲线程数 = 4**（T0/T1/T2/T3）；`T4`/`T5` 仅在需要时存在。总线程预算 **≤ 6**。
- `T1` 与 `T2` 之间是**单向**的：`T1` 只提交"目标态差异"，`T2` 只回报"实际回读值"。`T2` 永不同步回调 `T1`（避免锁反转），一切结果以 `RawEvent::HalReceipt` 形式**回到 T1 的入站队列**。

### 2.2 为什么 HAL 必须是"异步 WMI + 独立线程"

`MSI_ACPI` 方法在固件侧执行，`Set_EC`/`Set_Thermal` 最坏情况可能因 EC 忙而阻塞数百毫秒乃至挂死。

- **禁止**在 `T0`（消息泵）或 `T1`（求值）线程调用任何 WMI 方法 —— 一次挂死就会让托盘、热键、OSD 全体失响应。
- **必须**使用 `IWbemServices::ExecMethodAsync`（`IWbemObjectSink` 回调接收结果），使调用方线程立即返回。真正的阻塞发生在 **`WmiPrvSE.exe`（进程外）**，随后的 `WMILT_TIMEOUT` 只损失一次调用，不损失 daemon。
- 超时策略（`T2` 内实现）：`EC read 300ms` / `EC write 1200ms` / `ACPI package 1500ms` / `HID feature 400ms` / `CCD apply 2000ms`。超时后：
  1. 标记该 `OpId` 为 `TimedOut`（**不**重试写操作，写重试是砖机风险源）；
  2. 置 `hal_health = Degraded`，暂停除"恢复原值"以外的所有写计划；
  3. **只读**探活（`Get_EC` 安全地址）3 次 × 300ms 间隔：全失败 → `hal_health = Dead`，OSD 警告 + 规则引擎整体进入 `SafeMode`（只允许恢复 baseline）；
  4. 恢复成功后 `hal_health = Ok`，`EcGovernor` 令牌桶重置为半满（避免恢复瞬间的写风暴）。

### 2.3 背压与合并

| 队列 | 容量 | 满时策略 |
|---|---|---|
| `RawEvent`（回调→T1） | 256 | 按 `merge_key = (source, entity)` **覆盖合并**；仍满则丢弃最低 `severity`，累加 `dropped_events` 计数 |
| `ActionPlan`（T1→T2） | 32 | 按 `field` 合并（后写覆盖前写，保留最终目标态）；合并次数计入 `plans_coalesced` |
| IPC 响应 | 每连接 1 帧在途 | `SnapshotSeq` 单调递增，客户端只接受更新帧，过期帧在服务端丢弃 |

> **不存在无界队列**。所有队列必须是固定容量，并在 `automation.stats` 暴露长度 + 丢弃计数（§9）。

---

## 3. 事件源（Trigger）规范

### 3.1 归一化事件模型

```rust
pub struct RawEvent {
    pub seq: u64,            // 引擎内单调序号（用于幂等与乱序检测）
    pub ts_mono_ns: i64,     // QueryPerformanceCounter → ns，跨源统一时基
    pub source: EventSource, // 见下表
    pub merge_key: u32,      // 同一实体的合并键
    pub payload: EventPayload,
}

pub enum EventSource {
    PowerSource, BatteryPct, PowerSaving, EffectivePowerMode,
    DisplayState, LidSwitch, SuspendResume,
    ForegroundChanged, ProcessExited, ProcessStarted /*ETW, opt-in*/,
    DisplayTopology, DeviceNodesChanged,
    SessionLock, SessionUnlock, SessionLogoff,
    Hotkey(HotkeyId), Deadline(DeadlineId),
    FirmwareEvent(u32) /* MSI_Event.MSIEvt */,
    TelemetryTick, HalReceipt, IpcCommand,
}
```

**必须**：`EventPayload` 只携带**事实**（如 `ac=true`），不携带派生结论（如 `shouldSwitchToExtreme`）。派生只发生在 `T1`，保证可重放、可测。

### 3.2 电源域

| 事件 | 机制 | 注册方式 | 说明 |
|---|---|---|---|
| AC/DC 切换 | `GUID_ACDC_POWER_SOURCE {5D3E9A59-E9D5-4B00-A6BD-FF34FF516548}` | `RegisterPowerSettingNotification(hwnd, &g, DEVICE_NOTIFY_WINDOW_HANDLE)` → `WM_POWERBROADCAST/PBT_POWERSETTINGCHANGE` | 载荷 `DWORD`：0=DC,1=AC,2=Hotplug 未知 |
| 电池百分比 | `GUID_BATTERY_PERCENTAGE_REMAINING {A7AD8041-B45A-4CAE-87A3-EECBB468A9E1}` | 同上 | **OS 只在整数百分比变化时投递**，天然免轮询；阈值穿越判定在 `T1` 完成（带迟滞，§4.3） |
| 省电模式 | `GUID_POWER_SAVING_STATUS {E00958C0-C213-4ACE-AC77-FECCED2EEEA5}` | 同上 | Win11 "节能模式"开关 |
| 有效电源模式 | `PowerRegisterForEffectivePowerModeNotifications`（powrprof，Win10 1809+） | 回调 `EFFECTIVE_POWER_MODE_CALLBACK` | 取值 `BatterySaver / BetterBattery / Balanced / HighPerformance / GameMode / MaxPerformance`；**这是比 `GetSystemPowerStatus` 更细的语义层**，直接映射到我们的 Scenario 建议 |
| 控制台显示状态 | `GUID_CONSOLE_DISPLAY_STATE {6FE69556-704A-47A0-8F24-C28D936FDA47}` | `RegisterPowerSettingNotification` | 0=off,1=on,2=dim → **休眠 OSD/关闭准星**，避免黑屏时残留 |
| 唤醒/挂起 | `WM_POWERBROADCAST`：`PBT_APMSUSPEND`、`PBT_APMRESUMEAUTOMATIC`、`PBT_APMRESUMESUSPEND` | 消息 | 挂起前**必须**：停止 ETW 会话、销毁 OSD 窗口、flush 配置；恢复后重新注册 hook（`SetWinEventHook` 句柄在挂起后可能失效） |

> `DEVICE_NOTIFY_CALLBACK` + `PowerSettingRegisterNotification` 变体作为**备选**：可在无窗口时使用，但回调落在系统线程池线程上，同样必须遵守 A2（只 post）。

### 3.3 前台进程与游戏生命周期（**核心设计**）

#### 3.3.1 进入：`SetWinEventHook`（O(1) 成本，被动）

```c
g_hForegroundHook = SetWinEventHook(
    EVENT_SYSTEM_FOREGROUND, EVENT_SYSTEM_FOREGROUND,
    NULL, WinEventProc, 0, 0,
    WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS);
```

- `WINEVENT_OUTOFCONTEXT`：回调在**我们自己的线程**（T0 的消息泵）投递，无跨进程上下文切换成本，且**不向目标进程注入任何东西**（反作弊友好，这是与 `WINEVENT_INCONTEXT` 的本质区别，后者需要 DLL 注入 —— **绝对禁止**）。
- 回调内只做：`GetWindowThreadProcessId(hwnd)` → `OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION)` → `QueryFullProcessImageNameW` → post。**不读进程内存**。
- 白名单匹配采用 **规范化小写绝对路径 + 可执行文件签名主体 + 可选窗口类名**，禁止仅按进程名匹配（`game.exe` 重名误判）。
- 前台窗口为 0/桌面/锁屏（`GetShellWindow()`、`Progman`、`WorkerW`、`LockApp.exe`）→ 归一化为 `Foreground::Shell`。

#### 3.3.2 退出：**进程句柄等待，而不是轮询**

进入游戏时（只发生一次）：

```rust
let h = OpenProcess(SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION, FALSE, pid)?;
let wait = RegisterWaitForSingleObject(&pool, h, on_exit, ctx, INFINITE,
                                       WT_EXECUTEONLYONCE);
```

- 游戏退出 → 内核对象 signalled → 线程池回调 → `RawEvent::ProcessExited`。**零轮询、零 CPU**。
- 若 `OpenProcess` 被反作弊拒绝（`ERROR_ACCESS_DENIED`）：降级为 **"焦点丢失 + 下一次前台事件时惰性校验"**，即当其它窗口成为前台时，用 `GetExitCodeProcess`（需要句柄，失败则用 `QueryFullProcessImageNameW` 二次确认）。**仍然不引入定时器**。
- 句柄/等待必须在退出、超时、daemon 关闭时 `UnregisterWaitEx` + `CloseHandle`，泄漏计数纳入 §9。

#### 3.3.3 后台进程启动（可选能力，默认关闭）

| 方案 | 成本 | 结论 |
|---|---|---|
| WMI `__InstanceCreationEvent WITHIN 1/2` | WMI 轮询查询，`WITHIN n` 是对**整个命名空间**的轮询；高负载下超过 100+ 进程事件会产生明显延迟与 `WmiPrvSE` CPU；且与 `EventPolling` 语义相悖 | **不采用为默认**；仅在用户显式启用"后台进程启动"规则且系统空闲时允许，UI 必须显示"会增加 WMI 轮询开销" |
| ETW `Microsoft-Windows-Kernel-Process`（`WINEVENT_KEYWORD_PROCESS=0x10`） | 真实时推送；需管理员（我们满足）；`ProcessTrace` 需专用线程；空闲成本 ≈ 0.0% CPU | **推荐**，opt-in。会话仅在"存在依赖该触发源的启用规则"时创建，规则全部停用后立即 `ControlTrace(STOP)` 并退出 T5 |
| `CreateToolhelp32Snapshot` 轮询 | 秒级轮询 | **禁止**（违反 A1） |

#### 3.3.4 `[已证]` ETW 提供者可用性

本机 `Get-WinEvent -ListProvider` 确认：`Microsoft-Windows-Kernel-Process` 存在，关键字 `WINEVENT_KEYWORD_PROCESS=0x10`（进程创建/退出）、`WINEVENT_KEYWORD_IMAGE=0x40`。→ 该路径技术上成立。

### 3.4 显示拓扑

| 事件 | 机制 | 注意 |
|---|---|---|
| 显示模式/分辨率/刷新率变化 | `WM_DISPLAYCHANGE` | 一次插拔会连发 2~6 条 → **必须** 400ms 合并窗口（§4.3 `settle`） |
| 显示器接入/移除 | `WM_DEVICECHANGE` (`DBT_DEVNODES_CHANGED`) | 与上条合并到同一 `merge_key = Topology` |
| 工作区/DPI 变化 | `WM_SETTINGCHANGE` (`SPI_SETWORKAREA=0x2F`, `SPI_SETLOGICALDPIOVERRIDE`) | 用于 OSD 重新定位/重渲染 |
| 真值来源 | 事件到达后**一次性**执行 `GetDisplayConfigBufferSizes` + `QueryDisplayConfig`，构建 `TopologySnapshot { displays: Vec<DisplayInfo{ id, friendly, w,h, refresh, hdr, primary }>, hash }` | 禁止每秒枚举 `EnumDisplayDevices` |

`TopologySnapshot.hash` 与上一版相同 → 事件丢弃（抑制无效抖动）。OSD/准星订阅 `TopologyChanged` 并重建缓冲。

### 3.5 会话与空闲

| 事件 | 机制 |
|---|---|
| 锁屏/解锁/登出/切换用户 | `WTSRegisterSessionNotification(hwnd, NOTIFY_FOR_THIS_SESSION)` → `WM_WTSSESSION_CHANGE`（`WTS_SESSION_LOCK=0x7` / `UNLOCK=0x8` / `LOGOFF=0x5`） |
| 空闲阈值 | **唯一必须使用定时器的语义**。算法：`t = GetLastInputInfo().dwTime`；下次唤醒点 = `t + idle_threshold + 1ms`；到点后若用户仍有输入则重算。**每个阈值每次激活最多 1 次唤醒**，不是秒级 tick |

补充约束：
- **禁止** `SetWindowsHookEx(WH_KEYBOARD_LL/WH_MOUSE_LL)`（既是反作弊红旗，也是延迟源）。
- 锁屏时：停止 ETW 会话、隐藏 OSD/准星（`GUID_CONSOLE_DISPLAY_STATE=off` 也会触发）、规则引擎进入 `Suspended`（不评估、不写 EC）。
- 解锁后：**不**重放锁屏期间错过的动作，只做一次收敛（`Reconcile`），因为错过的瞬时事件已无意义。

### 3.6 硬件热键

- `RegisterHotKey(hwnd, id, MOD_NOREPEAT | mods, vk)`，id 空间 `0xA000..0xA0FF`（128 个），配置期做**冲突预检**（用 `RegisterHotKey` 试注册 → 立即 `UnregisterHotKey`）。
- 注册失败（被其它程序占用）→ IPC 上报 `HotkeyConflict{vk,mods}`，UI 红字提示，**不静默失败**。
- `MOD_NOREPEAT` 必选，防止按住产生写风暴。
- **Fn 组合键（亮度/风扇 Boost）无法通过 `RegisterHotKey` 捕获**：它们在 ACPI/EC 侧被驱动直接消费。捕获路径只能是 `MSI_Event`（§3.8 `[待证]`）或 HID Consumer Page（`0x000C`）。**在 Phase 1 之前，UI 不得承诺"自定义 Fn 键"**。

### 3.7 定时触发（单一最近到期调度器）

```rust
// 唯一的时间驱动组件。无任何"每秒 tick"。
struct DeadlineScheduler {
    heap: BinaryHeap<Reverse<(Instant, DeadlineId)>>, // 惰性删除
    timer: HANDLE, // CreateWaitableTimerExW(.., 0 /* 非高精度 */ ..)
}
```

- 每次规则集变更/事件完成后：取堆顶 `next`，`SetWaitableTimerEx(timer, due, 0, NULL, NULL, /*TolerableDelay*/ 50ms, 0)`。
- 无定时规则 → 定时器**不**设定，`WaitForMultipleObjects` 只剩 IPC/事件句柄 → 真 0 唤醒。
- 高精度（`CREATE_WAITABLE_TIMER_HIGH_RESOLUTION` + `timeBeginPeriod`）**只允许**在 OSD 动画期间持有，动画结束立即 `timeEndPeriod`（否则破坏整机省电）。
- 支持语义：`AtTimeOfDay`（每日/指定星期）、`Every(Duration ≥ 60s)`、`AfterTriggerDelay`。
- **禁止** `Every(< 60s)`：配置校验直接拒绝并提示"小于 60s 的周期任务应使用 Trigger 而非 Timer"。

### 3.8 固件事件 `MSI_Event`（潜在王牌，`[待证]`）

本机 `root\WMI` 中确实存在 `MSI_Event` 类且具备 WMI 事件类形态（`TIME_CREATED` / `SECURITY_DESCRIPTOR` / `MSIEvt:UInt32`）。若可订阅，则"EC 侧 Fn 键、AC 插入、盖子、热区报警"都能变成**真正 0 成本推送**，这是相对 LLT 的结构性优势。

**当前证据不足以采用**：
1. 非提权会话下 `SELECT * FROM MSI_Event` 返回"无法分析的查询"（可能是权限、也可能该 provider 未实现事件投递）；
2. `Get-CimInstance MSI_Event` 返回空集（无实例，符合纯事件类特征，但也可能是未激活）。

**Phase 0 `msilt-probe` 必须回答（写入 `HARDWARE_PROTOCOLS.md`）**：

| 编号 | 待证问题 | 判定标准 |
|---|---|---|
| P-A1 | 提权下 `ExecNotificationQueryAsync("SELECT * FROM MSI_Event")` 是否注册成功 | `HRESULT == S_OK` 且 60s 内无 `OnCompleted` |
| P-A2 | `MSIEvt` 编码表：按 Fn+F5/F6（亮度）、插拔 AC、按 Cooler Boost 各触发哪几个值 | 可复现的 `事件 ↔ 动作` 映射表 |
| P-A3 | 订阅是否会因 `MSI Foundation Service` / `OmApSvcBroker` 停止而失效 | 停用后订阅是否静默死亡（决定 §12 分档策略） |
| P-A4 | 长时订阅是否有内存/句柄增长（`WmiPrvSE` 内存曲线） | 8 小时 < 2 MiB 增长 |

**在 P-A1 通过前**：Automation 的 `FirmwareEvent` 触发源标记为 `Unavailable`，所有依赖它的规则在 UI 中显示为"等待硬件验证"，**不得**作为唯一触发源。

### 3.9 遥测（按需采样，非轮询核心）

- `TelemetryProvider` 只有"被订阅"时才启动：`OSD 悬浮条可见` 或 `存在启用了 Telemetry Condition 的规则`。
- 采样率：**1 Hz**（EC 温度/风扇/功耗，批量一次 WMI 调用；`[待证]` 是否需要多条 `Get_*`）；GPU 走 NVML。
- 无订阅者时立即停止，`nvmlShutdown` 释放（见 §5.3 关于独显唤醒的红线）。
- **禁止**把遥测接入 Trigger 的常驻路径（例如"CPU > 80°C 就切 Extreme"在无 OSD 时也应工作 → 此时它属于"显式订阅"，规则停用即退订）。

### 3.10 零轮询对照表（验收用）

| 能力 | 本文档方案 | 唤醒/秒（空闲） | 备注 |
|---|---|---|---|
| AC/DC | `RegisterPowerSettingNotification` | 0 | 纯 OS 推送 |
| 电量阈值 | 同上（百分比变化时才投递） | 0 | 迟滞在 T1 |
| 前台游戏 | `SetWinEventHook(OUTOFCONTEXT)` | 0 | 目标进程无注入 |
| 游戏退出 | `RegisterWaitForSingleObject` on process handle | 0 | 内核对象等待 |
| 外接屏 | `WM_DISPLAYCHANGE` + 400ms 合并 | 0 | |
| 锁屏 | `WTSRegisterSessionNotification` | 0 | |
| 空闲 | 单点 waitable timer | 0（无空闲规则时） | 每次激活 1 次 |
| 定时规则 | 单点 waitable timer（tolerance 50ms） | 0（无规则时） | |
| 后台进程启动 | ETW `Kernel-Process`（opt-in） | 0（CPU）、事件驱动 | 会话按需 start/stop |
| OSD FPS | ETW `DxgKrnl Present`（仅可见时） | 0（不可见时） | §11.5 |
| 配置外部修改 | `ReadDirectoryChangesW` | 0 | 唯一文件监视 |

**验收判据**：在"无启用规则 + OSD 隐藏"状态下，`msilt-daemon` 的 `TotalProcessorTime` 60s 增量 **必须** `< 60ms`（≈0.1% 单核），`QueryPerformanceCounter` 唤醒计数（`powercfg /energy` 或 ETW `Microsoft-Windows-Kernel-Power` 的 `Idle State` 事件）**必须** ≈ 0 次/秒。

---

## 4. Condition 引擎

### 4.1 AST（v1，冻结）

```rust
pub enum Condition {
    Always,
    // ---- 电源域
    AcConnected(bool),
    BatteryPct { op: CmpOp, value: u8 },          // 迟滞见 §4.3
    EffectivePowerModeIn(Vec<EffectivePowerMode>),
    PowerSaving(bool),
    // ---- 进程域
    ForegroundIs(MatcherId),
    ProcessRunning(MatcherId),
    GameActive,                                    // 由 3.3 生命周期维护的派生事实
    // ---- 显示域
    DisplayTopology { external: Option<bool>, count: Option<CmpOpU32>, any_hdr: Option<bool>,
                      primary_refresh: Option<CmpOpU32> },
    // ---- 会话/时间
    SessionState(SessionState),                    // Locked / Unlocked / LoggedOff
    IdleFor { ms: u64 },
    TimeWindow { days: DayMask, from: HhMm, to: HhMm },
    // ---- 遥测（昂贵，最后求值）
    Telemetry { cpu_temp_ge: Option<f32>, gpu_temp_ge: Option<f32>,
                fan_rpm_ge: Option<u32>, gpu_power_ge: Option<f32> },
    // ---- 组合
    Not(Box<Condition>),
    All(Vec<Condition>),
    Any(Vec<Condition>),
    // ---- 元
    ModeIs(ScenarioId),                            // 当前场景栈顶
    RuleFiredRecently { rule: RuleId, within_ms: u64 },
}
```

**解析期规范化**（`normalize()`，在加载配置时执行一次）：
1. 嵌套 `All(All(..), ..)` 展平为单层；`Any` 同理；`Not(Not(x)) → x`；
2. 完全相同的叶子去重（结构相等）；
3. 按 `cost_rank()` 稳定排序（`Always=0`, 电源/会话/模式=1, 前台/进程=2, 显示=3, 空闲/时间=4, 遥测=9）；
4. 限制：深度 `≤ 8`、叶子 `≤ 64`/规则、规则总数 `≤ 512`、`Any` 分支 `≤ 32`。超限 → 配置校验失败（不做静默截断）。

### 4.2 求值语义

- **快照求值**：`T1` 在每次事件循环开始时构造 `EvalContext{ world: &WorldSnapshot, now, rule_id, history }`，其中 `world` 是**不可变**的。规则集在一次事件内的所有求值共享同一个 `world` → **无撕裂读**。
- **短路 + 成本序**：`All` 从低成本项开始，遇 `false` 立即返回；`Any` 遇 `true` 立即返回。
- **纯函数**：`eval(&Condition, &EvalContext) -> bool` 不修改任何状态（防抖状态在规则包装层，不在 AST 内）。
- **可重放**：`(rule_set_version, event_log, initial_world)` 可完整重算出决策序列 → 单元测试用真值快照。
- **单位与精度**：温度 `f32` 比较统一四舍五入到 1 位小数；功耗 1 位；百分比整数。

### 4.3 防抖 / 冷却 / 迟滞（形式化）

设条件原始布尔序列 `c(t)`，规则 `R` 的输出 `o(t)`：

| 机制 | 参数 | 语义 |
|---|---|---|
| **防抖（进入）** | `debounce_on = D_on`（默认 800ms） | `o` 由 0→1 需 `c` 连续为真 ≥ `D_on` 且期间无中断 |
| **防抖（退出）** | `debounce_off = D_off`（默认 1500ms） | 退出更慢，避免"焦点短暂跳走"导致恢复抖动 |
| **合并窗口（拓扑）** | `settle = 400ms` | 事件风暴合并后只评估一次 |
| **冷却** | `cooldown = C`（默认 3000ms；EC 写类动作强制 ≥ 2000ms） | 触发后 `C` 内不再触发同一规则 |
| **迟滞（电量）** | `hysteresis = 3%` | `BatteryPct < 20` 触发后，需 > 23 才允许再次触发 |
| **抖动检测** | `flap_window = 60s`, `flap_threshold = 5` | 同类状态翻转 ≥5 次 → 冻结相关规则 `120s`，OSD 提示"检测到频繁切换，已临时冻结自动化" |

实现：每条规则持有小型状态机（`Idle / Pending{deadline} / Armed / Fired{at}`），`Pending` 的截止时间注册到 §3.7 的 **同一个** `DeadlineScheduler`（不额外创建定时器）。

**抖动防护数学**（防 EC 写风暴）：
- 无防护：AC 以 2 Hz 抖动 → 每次触发 ≥ 1 次 EC 写 → 2 写/s = 120 写/min。
- 有 §4.3 + §4.4：`D_on=800ms` 过滤掉 2Hz 抖动（永远无法连续为真 800ms）→ 0 写；`EcGovernor` 兜底 4 写/10s = 24 写/min 上限。
- `flap` 检测进一步把"1 Hz 稳定抖动"（`D_on` 刚好能过）也冻结。

### 4.4 `EcGovernor`（EC 写节流器，硬安全件）

```rust
pub struct EcGovernor {
    bucket: TokenBucket { capacity: 4, refill: 1 per 2500ms }, // → 4 事务/10s 稳态
    last_op: HashMap<FieldId, Instant>,                        // 每字段最小间隔 250ms
    global_last: Instant,                                      // 全局最小间隔 100ms（固件争用）
    deny_write: &'static [u8],                                 // 硬黑名单（见下）
}
```

> **计数单位 = 事务（transaction），不是字节写。** 经与 `msilt-core::ec` 的实测契约对齐（`WriteGuard::prepare` + `PreparedWrite::commit` + `WriteReceipt`）：一次 `FanCurve` 写入 = **6 个寄存器** 的「写+回读校验」，即 **1 个逻辑事务**。令牌桶因此按 `WriteReceipt` 记账：6 点风扇曲线消耗 **1** 个令牌，而不是 6 个；字段最小间隔也以事务为单位（402ms 级的事务耗时天然满足 250ms 间隔）。若按字节记账，一次风扇曲线会吃掉全部 4 个令牌并卡死 3 秒 —— 这是必须写进契约的细节。
>
> **实测延迟预算**：`commit()` 对每个 knot 做 `write_byte → verify(read_byte)`，多于 1 项时再整体复验一遍 → 一次曲线变更 ≈ **12 次 WMI 往返**（6×写+读，再 6×读）。按保守 25ms/往返估算 ≈ 300ms，故 `T2` 对该操作的超时必须 ≥ 1500ms（§2.2），且期间**禁止**其它 EC 操作插队（单写者已保证）。此项必须在 T-06 中实测并回填。

- **拒绝**（不是排队）情形：字段相同且目标值相同 → 直接 `Noop`（幂等短路，不消耗令牌）。
- 令牌不足 → `Deferred{earliest}`，计划压回 `T1` 的待处理集合（按 `field` 合并，只保留**最终目标态**），到期后执行。**绝不丢弃终态**。
- 硬黑名单（沿用 ARCHITECTURE §1.3）：`0x50` / `0x51`（PL1/PL2）等，`EcGovernor` 在**最后一层**再次拦截（双保险：`WriteGuard` 在 core，`EcGovernor` 在 automation）。
- 计数器：`ec_writes_total`、`ec_writes_throttled`、`ec_write_max_burst`，暴露到 IPC 统计（§9）。

---

## 5. Action 与目标态收敛

### 5.1 为什么是 "desired state" 而不是 "command"

命令式动作（`SetFanCurveCurveX()`）在"多条规则同时命中"和"退出场景要恢复"两件事上必然失控。因此：

- **Action = 对一个 `FieldId` 的期望值提议**；
- **HAL = 把 `field → 实际值` 收敛的执行者**，并回报回读值；
- **恢复 = 释放租约**，由仲裁器把字段回落到下层租约或 `baseline`；
- **幂等**：`propose(field, value)` 在 `value == current` 时不产生任何 I/O（`Receipt::Noop`）。

### 5.2 字段表（`FieldId`，v1 冻结）

| FieldId | 取值域 | 典型延迟 | 幂等 | 安全等级 | 备注 |
|---|---|---|---|---|---|
| `PerfMode` | `Balanced / Silent / Extreme`（+ `Custom`） | 80–300ms（EC 包） | 是 | 中 | 写 EC，受 Governor |
| `FanCurve` | 6 点 `(tempC→duty%)` 单调递增 | 100–400ms（实测预算 ≈300ms，见 §4.4） | 是 | **高** | 单事务提交由 `msilt-core::WriteGuard`（`EcWriteIntent::FanCurve`）保证；自动化层只按 1 个事务记账 |
| `CoolerBoost` | bool | 50–200ms | 是 | 中 | 与 `FanCurve` 互斥，需仲裁（§6.5） |
| `ChargeLimit` | 60–100 %（`100`=不限） | 200–600ms | 是 | **高** | 频繁写会磨损 EC NVRAM → Governor 下限 5s |
| `RefreshRate` | 60 / 240 | 300–1200ms（CCD） | 是 | 中 | 必须回读实际值（可能被 DRM/全屏独占拒绝） |
| `Hdr` | bool | 300–1500ms | 是 | 中 | `DISPLAYCONFIG_..._HDR` |
| `Brightness` | 0–100 | 20–80ms | 是 | 低 | WMI 亮度或 EC，二选一（按能力位） |
| `IcmProfile` | 已安装 profile 名 | 50–300ms | 是 | 低 | MS CMS 关联；**不等于**面板硬件色域切换（§5.3） |
| `RgbTheme` | 主题 id / 24×3 矩阵 | 5–30ms（HID） | 是 | 低 | 走 `HID Feature Report`，不占 EC |
| `WinKeyLock` / `Touchpad` / `MicMute` / `SpeakerMute` | bool | <10ms | 是 | 低 | |
| `MonitorPower` | `On / Off` | — | 否 | 低 | `SendMessage(WM_SYSCOMMAND, SC_MONITORPOWER)`，非幂等，不进栈 |
| `Toast` | 文本模板 + 字段 | — | **否** | 无 | 一次性，不进租约 |
| `Script` | 程序路径 + 参数 | 可变 | **否** | **高** | §5.4 |

### 5.3 动作表（`Action` → HAL op → 能力位）

```rust
pub enum Action {
    Set(FieldId, FieldValue),
    Script(ScriptSpec),
    Toast(ToastSpec),
    Chain(Vec<Action>),                 // 仅允许"同一字段组"内的顺序动作
    Emit { event: String, payload: Json }, // 给 IPC 订阅者（StreamDeck/外部联动）
}
```

| Action | HAL op | CapabilityFlag | 冲突互斥组 |
|---|---|---|---|
| `Set(PerfMode, ..)` | `scenario.set_mode` | `CAP_SCENARIO` | `perf` |
| `Set(FanCurve, ..)` | `fan.set_curve`（单事务） | `CAP_FAN_CURVE` | `fan`（与 CoolerBoost 冲突） |
| `Set(CoolerBoost, ..)` | `fan.set_boost` | `CAP_COOLER_BOOST` | `fan` |
| `Set(ChargeLimit, ..)` | `battery.set_charge_limit` | `CAP_CHARGE_LIMIT` | — |
| `Set(RefreshRate, ..)` | `display.set_refresh` | `CAP_REFRESH_RATE` | `display` |
| `Set(Hdr, ..)` | `display.set_hdr` | `CAP_HDR` | `display` |
| `Set(Brightness, ..)` | `display.set_brightness` | `CAP_BRIGHTNESS` | `display` |
| `Set(IcmProfile, ..)` | `color.associate_profile` | `CAP_ICM` | `color` |
| `Set(RgbTheme, ..)` | `keyboard.apply_theme` | `CAP_RGB` | `rgb` |
| `Set(WinKeyLock, ..)` | `input.set_winkey_lock` | `CAP_WINKEY` | `input` |
| `Set(Touchpad, ..)` | `input.set_touchpad` | `CAP_TOUCHPAD` | `input` |
| `Script(..)` | `proc.run_restricted` | `CAP_SCRIPT` | — |

**关于 True Color 的两级定义（对 ARCHITECTURE "14 个 .icm" 的纠偏）**

本机 `spool\drivers\color` 只有 **6 个** `TrueColor_*.icm`，且 `MSI True Color` 目录中存在 `IntelCSC.exe` / `cvt.exe` / `calibration\`，说明官方的色域切换**可能**包含 GPU/面板级 LUT/CSC 变换，不只是 ICC 关联。因此：

- **L1（Phase 1 开放）**：`IcmProfile` = MS CMS 关联（`InstallColorProfileW` 已存在 → `WcsAssociateColorProfileWithDevice`）。安全、可回滚、无 EC。
- **L2（Phase 2，需实证）**：真正的色域（sRGB/DCI-P3/Adobe）硬件切换。`[待证]` 是否仅靠 ICC 就能达成；若需要 `IntelCSC.exe` 之类的外部进程，则**不纳入自动化**（外挂官方 exe = 不可控副作用，且属于"官方实现的黑盒"）。
- UI 文案必须写清："L1 影响色彩映射，不改变面板原生色域"。

### 5.4 脚本执行器（提权边界，A7）

**威胁模型**：daemon 以 `Highest` 运行；若直接 `CreateProcess` 执行用户可写的 bat/exe，等于把"任意用户 → 管理员代码执行"的 LPE 通道开放给任何能写配置目录的程序。

**必须**：
1. 运行在**交互用户的中完整性令牌**上：`WTSQueryUserToken(session_id)` → `CreateProcessAsUserW`（令牌副本，`TokenIntegrityLevel = Medium`，`SeChangeNotifyPrivilege` 保留，其余 `AdjustTokenPrivileges` 关闭）。若拿不到用户令牌（多用户/无会话）→ **拒绝执行**并报错。
2. `CreateProcessAsUserW(..., CREATE_SUSPENDED | CREATE_NO_WINDOW | CREATE_BREAKAWAY_FROM_JOB)`，随后 `AssignProcessToJobObject` 到每规则一个 `Job`：
   - `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`（daemon 崩溃也带走子进程）；
   - `JOB_OBJECT_LIMIT_PROCESS_MEMORY = 512 MiB`；
   - `JOB_OBJECT_LIMIT_JOB_TIME`（默认 10s，配置上限 120s）。
3. **不接受命令行字符串**：`ScriptSpec { program: PathBuf, args: Vec<String>, cwd: PathBuf }`，`program` 必须存在且为 `.exe/.cmd/.ps1/.bat`，参数**逐个**传递（不经 shell 拼接）。
4. 路径白名单：拒绝 `%WINDIR%`、`%ProgramFiles%`、UNC（`\\`）、重解析点（`FILE_ATTRIBUTE_REPARSE_POINT`）、以及任何"父目录可被非管理员写"的路径（`[实现建议]` 用 `GetNamedSecurityInfo` 检查 DACL；简化版：只允许 `%LOCALAPPDATA%\MSI-Toolkit\scripts\` 之下）。
5. 每次执行前对 `program` 计算 SHA-256 并写入日志；配置中可固定 `expected_sha256`，不匹配则拒绝（防止"配置被改 → 脚本被替换"）。
6. 输出捕获：stdout/stderr 各上限 **8 KiB**（超出截断 + 标记 `truncated`），不落盘明文超过 64 KiB。
7. `Script` 动作**永不**进入租约栈（不可回滚），也不参与 `Reconcile`。

**注入的环境变量**（`[硬契约]`，其他组不得改名）：

| 变量 | 含义 |
|---|---|
| `MSILT_AC_CONNECTED` | `1` / `0` |
| `MSILT_BATTERY_PCT` | `0..100` |
| `MSILT_ACTIVE_MODE` | `Balanced` / `Silent` / `Extreme` / `Custom` |
| `MSILT_EFFECTIVE_POWER_MODE` | `BatterySaver` … `MaxPerformance` |
| `MSILT_RULE_ID` / `MSILT_RULE_NAME` | 触发规则 |
| `MSILT_TRIGGER` | `power` / `foreground` / `display` / `session` / `idle` / `timer` / `hotkey` / `manual` |
| `MSILT_GAME_EXE` | 当前前台/退出的游戏路径（无则空串） |
| `MSILT_REFRESH_RATE` | 当前主屏刷新率 |
| `MSILT_EXTERNAL_DISPLAY` | `1` / `0` |
| `MSILT_SESSION_STATE` | `unlocked` / `locked` |
| `MSILT_EVENT_TS` | RFC3339 本地时间 |
| `MSILT_CONFIG_VERSION` | 配置 schema 版本 |

> 变量名以 `MSILT_` 为前缀（不是 `MSILT_` 以外的变体），且**首字母大写其余小写**的风格已冻结，避免跨组各自命名。

### 5.5 `Toast` 动作

- 参数：`{ template: String, fields: Map<String,String>, duration_ms: Option<u32>, icon: Option<ToastIcon>, monitor: MonitorSel }`；
- 模板变量替换在 `T1` 完成（不进入 OSD 线程的解析路径），OSD 只接收"已展开的字符串 + 样式"；
- 相同 `(template, values)` 在 800ms 内不重复弹出（合并）；
- `duration_ms` 上限 5000，下限 800，默认 1800（§11.4）。

---

## 6. 仲裁与状态栈

### 6.1 租约模型

```rust
pub struct Lease {
    pub field: FieldId,
    pub value: FieldValue,
    pub owner: Owner,           // Rule(RuleId) | Scenario(Id) | User(IpcClientId) | Baseline
    pub priority: i16,          // 0..=1000；User 恒为 1000
    pub specificity: u8,        // 条件叶子数（越具体越优先，解决"两条都命中"）
    pub acquired_at: Instant,
    pub ttl: Option<Duration>,  // 默认 None；`User` 写操作 ttl=None，独占至显式释放
}
```

- 每个 `FieldId` 维护一个**有序租约栈**（`Vec<Lease>`，按 (priority, specificity, acquired_at) 升序）。**栈顶 = 生效值**。
- 栈为空 → 生效 `baseline[field]`；`baseline` 在**首次**被任何租约覆盖时记录（`read-modify-write` 中的 read 值来自回读，不是缓存）。
- 释放（`Rule` 停用 / `Scenario` 出栈 / `ttl` 到期）→ 直接从栈中移除 → **自动**回落，无需"反向动作"代码。这就是"退出游戏恢复进游戏前状态"的实现方式（§6.3）。
- 栈深度上限 8，超出则拒绝最低优先级租约的获取（并记 `lease_pressure` 计数），**不**静默丢弃栈顶。

### 6.2 优先级与特异性

```
winner(field) = argmax over leases of (priority, specificity, acquired_at)
```

| Owner 类型 | priority | 说明 |
|---|---|---|
| `User`（UI/CLI 手动操作） | 1000 | 永远压过自动化；用户手动改动后，自动化规则在同字段上**不再生效**直到用户主动交还（`lease.release` 或规则重新激活） |
| `Scenario(Game)` | 800 | 场景入栈 |
| `Rule`（显式 `priority`） | 100–799 | 配置可写；默认 500 |
| `Rule`（未指定） | 500 | |
| `Rule`（`safety/*` 内置） | 900 | 仅限内部保护规则（如"温度 > 95°C 强制 CoolerBoost"），用户规则不可占用 900+ |
| `Baseline` | 0 | 栈空时的兜底 |

- `specificity` = 命中条件树的叶子数量，用于同优先级打破平局（"插 AC + 前台是游戏" 打赢 "插 AC"）。
- **同 owner 重复获取** → 更新 value 与 `acquired_at`（不新增条目）。
- **反对称保护**：如果两条规则对同一字段写入**相反**值且优先级相同，仲裁器记录 `conflict` 计数并在 UI 的"规则诊断"页高亮（不阻塞执行，栈顶赢）。

### 6.3 场景栈（游戏进出时序）

```
t0  前台变为 game.exe（白名单命中）
    ├─ ScenarioStack.push(Scenario{ id:"game", rule:"进入游戏", entered_at:t0 })
    ├─ 快照 baseline（读取当前 PerfMode/RefreshRate/FanCurve/RgbTheme/Touchpad ...）
    ├─ 租约获取（priority=800）：PerfMode=Extreme, RefreshRate=240, FanCurve=..., Touchpad=false, RgbTheme=..., Toast("已切换 极速模式")
    ├─ OpenProcess(SYNCHRONIZE) + RegisterWaitForSingleObject(→ 退出事件)
    └─ OSD 悬浮条（若用户开启）启动 ETW 采样

t1  game.exe 存活期间：其他规则（如"插 AC"）照常评估，但对已被 800 优先租约占用的字段无效

t2  game.exe 退出（内核对象 signalled）
    ├─ ScenarioStack.pop("game")
    ├─ 释放该场景全部租约 → 字段逐层回落到下层租约或 baseline
    ├─ 若 baseline 是"用户手动值" → 原样恢复（不做任何"智能推断"）
    ├─ Toast("已恢复 平衡模式 / 60Hz")
    ├─ 停止 ETW 会话、销毁 OSD 缓冲
    └─ 移除 process handle / wait 注册
```

**崩溃/异常路径**：
- daemon 重启后，`ScenarioStack` 从 `state.json` 恢复为 `Cold`：**不**尝试重放"退出游戏"（进程已消失），而是执行一次 `Reconcile`：重新读回真实硬件状态 → 与"应有栈"比对 → 差异字段按栈重新收敛。若真实状态与 baseline 差异无法解释，则**只告警不写 EC**（避免重启即写风暴）。
- 系统睡眠/唤醒：栈保留（内存）；唤醒后 `Reconcile`。

### 6.4 漂移收敛（Drift Reconcile）

- 触发条件：`HalReceipt` 回读值 ≠ 栈顶期望值，或 `MSI_ACPI` 被外部（官方软件/固件）改动。
- 手段：只用**已有**的 `DeadlineScheduler` 排一次 `Reconcile`（默认 30s 后，且仅当字段仍被租约占用）。
- 上限：同一字段 60s 内最多 1 次收敛尝试；连续 3 次失败 → 该字段标记 `Unstable`，停止收敛并告警（**防止与官方软件"互相拉扯"写 EC**，这是砖机风险）。
- `Unstable` 状态的字段在 UI 显示"被其它软件占用"。

### 6.5 冲突矩阵（必读）

| 冲突 | 处理 |
|---|---|
| `FanCurve` vs `CoolerBoost` | 同一互斥组 `fan`：CoolerBoost=true 时 FanCurve 租约保留但不生效（HAL 层不写曲线）；CoolerBoost 释放后重新应用曲线（单次写） |
| `RefreshRate` vs `Hdr` 同时变更 | 合并为一个 `display` 事务：先 `ChangeDisplaySettingsExW`（含刷新率），一次 CCD apply 后再设 HDR（避免两次模式切换导致黑屏闪烁） |
| 官方 MSI Center 同时在写 EC | 由 `MSI Foundation Service` 存在检测（§9 计数器 `msi_official_running`）+ 漂移收敛限流处理；UI 建议启用停用器 |
| 用户手动改 + 自动化规则 | `User` 租约 1000 压过；规则激活时不抢占，仅记录"被抑制" |
| 两条规则同时 `Script` | 允许并发（各自 Job）；同一 `program+args` 在 2s 内去重 |
| `PerfMode` 与 `FanCurve` 同时命中 | 顺序固定：先 `PerfMode`（平台包），后 `FanCurve`（覆盖模式默认曲线）；顺序在 `ActionPlan` 中显式编号，禁止依赖 map 迭代顺序 |

---

## 7. 配置格式（`automation.toml` v1）

路径：`%LOCALAPPDATA%\MSI-Toolkit\automation.toml`（**注意**：daemon 提权后 `%LOCALAPPDATA%` 指向 SYSTEM，必须由 daemon 显式解析交互用户 profile，见 §13 对 `@Codex-Core` 的诉求 C-02）。

```toml
schema = 1
# 全局保护
[limits]
max_rules = 512
max_condition_depth = 8
min_timer_period_ms = 60000
ec_writes_per_10s = 4
ec_min_gap_ms = 250

[[rules]]
id = "game-extreme"
name = "进入游戏自动极速"
priority = 600
enabled = true
capabilities = ["CAP_SCENARIO","CAP_REFRESH_RATE","CAP_RGB","CAP_TOUCHPAD"]

[triggers.game_enter]
on = "foreground"
exit = "process"            # process | foreground_lost | both
debounce_on_ms = 800
debounce_off_ms = 1500

[condition]
all = [
  { process_running = "whitelist:games" },
  { ac_connected = true },
  { not = { session_state = "locked" } },
]

[[actions]]
field = "PerfMode"
value = "Extreme"

[[actions]]
field = "RefreshRate"
value = 240

[[actions]]
field = "Touchpad"
value = false

[[actions]]
toast = { template = "已进入 {mode} 模式", fields = { mode = "极速" } }

[actions_custom]              # 兼容 LLT 风格的书式写法
```

**规范要求**：
- 未知字段 → **加载失败**（拒绝静默忽略，避免"我配了但没生效"）；`schema` 版本不同 → 走迁移器（`v1→v2` 等），迁移前备份到 `automation.toml.bak`。
- 热加载：`ReadDirectoryChangesW` 监听目录 → 防抖 500ms → 解析 → 校验 → **原子替换**（`ArcSwap`）。校验失败 → 保留旧配置 + 日志 + IPC 事件 `automation.config_invalid`。
- 每个规则必须有稳定 `id`（UI 生成 UUID）；导入导出以 `id` 为键合并。

---

## 8. 资源预算与测量方法

| 组件 | Working Set | Private | 说明 |
|---|---|---|---|
| `msilt-automation`（512 规则） | ≤ 1.2 MiB | ≤ 1.0 MiB | 规则压缩表示 + 内联条件池；配置热更新用双缓冲，旧版延迟 2 个事件后释放 |
| 事件队列（256 槽） | 64 KiB | 64 KiB | 固定容量 `array`，无堆分配 |
| 租约栈（≤ 8 × 24 字段） | 24 KiB | 24 KiB | |
| OSD 通知条（可见时） | 移动端 0.2–1.0 MiB 瞬态 | 同 | 520×112 @100% → 233 KiB；@200% DPI → 932 KiB |
| OSD 监控条（可见时） | 0.3–1.1 MiB 瞬态 | 同 | 340×160 |
| 准星（可见时） | 64 KiB 瞬态 | 同 | 128×128×4 |
| GDI+ 运行时 | ≤ 240 KiB（仅 OSD 会话期间） | 同 | 空闲 30s 后 `GdiplusShutdown` |
| ETW 会话缓冲 | 4 × 64 KiB（内核侧，非本进程） | 0 | 仅 FPS/进程事件时 |

**验收方法与命令**（任何人可复跑）：

```powershell
# 1) 常驻基线（无规则、OSD 隐藏、静置 60s）
$p = Get-Process msilt-daemon
$p.Refresh(); $ws1=$p.WorkingSet64; $t1=$p.TotalProcessorTime
Start-Sleep 60; $p.Refresh()
"{0:N1} MiB WS  ΔCPU={1} ms  handles={2} threads={3}" -f `
  ($p.WorkingSet64/1MB), ($p.TotalProcessorTime-$t1).TotalMilliseconds, $p.HandleCount, $p.Threads.Count

# 2) OSD 可见期峰值（打开悬浮条 60s 后关闭，再取一次）
#    目标：关闭后 30s 内 WS 回落至基线 + ≤ 64 KiB 抖动，handle 数完全回到基线

# 3) 唤醒审计
powercfg /energy /duration 60 /output "$env:TEMP\energy.html"
#    检查 "平台计时器分辨率" 与 "请求的计时器分辨率" 项；正常 OSD 空闲时应无 1ms 请求
```

**硬指标**：基线 WS ≤ 12 MiB（含托盘、IPC、引擎、HAL 客户端）；`threads ≤ 6`；`handles` 稳定（±0 增长/小时）；空闲 CPU 60s 增量 < 60ms。

---

## 9. 可观测性

- **环形日志**（`ring buffer`，容量 4096 条，每条 ≤ 512 B）：**零磁盘 I/O**，仅在 `--debug` 或崩溃转储时落盘。
- **计数器**（`automation.stats` IPC）：
  `events_in / events_dropped / plans_coalesced / rules_evaluated / rules_fired / ec_writes_total / ec_writes_throttled / lease_pressure / conflicts / hal_health / hal_timeouts / drift_fixes / drift_failures / dropped_frames / etw_lost_events / etw_session_state / msi_official_running`
- **崩溃转储**：`minidump` 由 daemon 在 `UnhandledExceptionFilter` 内写 `%LOCALAPPDATA%\MSI-Toolkit\crash-*.dmp`（含自动化状态摘要，不含用户脚本内容）。
- **一致性**：所有计数通过 IPC 暴露给 UI 的"诊断"页（这是超越 LLT 的可解释性）。

---

## 10. 失败模式矩阵

| 场景 | 检测 | 响应 | 用户可见 |
|---|---|---|---|
| WMI 调用超时 | `T2` 计时器 | 中止计划、`hal=Degraded`、3 次只读探活 | OSD 警告 + UI 诊断行 |
| 只读探活全失败 | 探活结果 | `hal=Dead`；规则进 `SafeMode`（只允许 baseline 恢复） | 托盘红点 + Toast |
| EC 写被拒（固件 NAK） | 回读不符 | 不重试；记 `drift`；30s 后单次收敛 | 诊断行（不弹窗） |
| 外部软件抢占 EC | 回读持续偏移 3 次 | `Unstable`，停止收敛，建议启用停用器 | UI 提示"被其它软件占用" |
| 规则写风暴 | `EcGovernor` 令牌耗尽 ≥ 3 连 | 该字段降速；若抖动检测命中 → 冻结 120s | Toast "检测到频繁切换，已临时冻结" |
| ETW 会话启动失败 | `StartTrace` HRESULT | OSD 悬浮条 FPS 显示 `N/A`；其余功能不受影响 | 悬浮条显示 `N/A` |
| 脚本被拒（令牌/路径/哈希） | 前置校验 | 不执行，记 `script_rejected` | Toast + 诊断行 |
| 配置文件损坏 | 解析失败 | 保留旧配置；`.corrupt` 副本 | UI 提示 |
| 系统睡眠 | `PBT_APMSUSPEND` | 停 ETW、销毁 OSD、flush 状态 | — |
| daemon 被杀 | 计划任务 `RestartOnFailure` | 重启后 `Reconcile`（不重放退出动作） | — |

---

## 11. 原生零注入 OSD 与屏幕准星（`msilt-daemon`）

### 11.1 窗口属性（逐字冻结）

```c
HWND hOvd = CreateWindowExW(
    WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOPMOST | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
    L"MSILT_OSD", L"MSILT OSD",
    WS_POPUP,
    0,0,0,0, NULL, NULL, hInstance, NULL);
```

| 标志 | 理由 |
|---|---|
| `WS_EX_LAYERED` | 必须配合 `UpdateLayeredWindow(ULW_ALPHA)` 实现逐像素 alpha；也是"不进入 DWM 内容重定向"的最轻路径 |
| `WS_EX_TRANSPARENT` | 鼠标/触摸 100% 穿透。**不能省**：仅靠 layered 的 alpha hit-test 在 1% alpha 的边缘像素上仍可能吃掉点击 |
| `WS_EX_TOPMOST` | 覆盖无边框全屏游戏 |
| `WS_EX_NOACTIVATE` | 点击不夺焦（否则游戏掉帧/输入丢失）；配合 `ShowWindow(SW_SHOWNOACTIVATE)` |
| `WS_EX_TOOLWINDOW` | 不进 Alt-Tab / 任务栏 / 窗口切换器 |

附加：
- `SetWindowDisplayAffinity(hwnd, WDA_EXCLUDEFROMCAPTURE)` 对**准星**设为**可选关闭**（默认 `WDA_NONE`：准星应出现在录制中，否则用户以为坏了）；对**通知条**可选开启。不要把两者混为一谈。
- `ShowWindow(hOvd, SW_SHOWNOACTIVATE)`，`SetWindowPos(..., HWND_TOPMOST, ..., SWP_NOACTIVATE | SWP_SHOWWINDOW)`。
- 进程级：`SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2)`（在 `main` 最早期，早于任何窗口）。

### 11.2 合成管线：**GDI+ 软件光栅，明确禁用 Direct2D**

ARCHITECTURE §1.1 铁律：daemon **严禁链接 Direct3D**。Direct2D 的硬件路径需要 `ID3D11Device`（会创建显存分配 + 可能唤醒 RTX 5090 + 与"0 显存占用"硬指标冲突）。因此：

```
文字/形状 → GDI+ (SmoothingModeAntiAlias, TextRenderingHintAntiAliasGridFit)
          → 32bppPremultipliedBGRA DIB Section (CreateDIBSection)
          → UpdateLayeredWindow(hwnd, hdcSrc, &ptDst, &size, hdcSrc, &ptSrc, 0, &BLENDFUNCTION{AC_SRC_OVER,0,alpha,AC_SRC_ALPHA}, ULW_ALPHA)
```

- **必须**用 `TextRenderingHintAntiAliasGridFit`（灰度抗锯齿），**禁止** `ClearTypeGridFit`：分层窗背景透明，子像素 RGB 抗锯齿会产生彩色毛边。
- 自绘 alpha：所有绘制都预乘 alpha（`PixelFormat32bppPARGB`），否则淡出时会出白边。
- 禁用 `DwmEnableBlurBehindWindow` / Mica（那是 UI 进程的事；OSD 要的是"无背景的纯元素"）。
- **不创建** `ID2D1Factory`、`ID3D11Device`、`DXGI` 任何对象；`msilt-daemon` 的导入表中不得出现 `d2d1.dll` / `d3d11.dll` / `dxgi.dll`（CI 用 `dumpbin /imports` 断言，见 §14）。

### 11.3 生命周期与"隐藏 = 0 KiB"

```rust
struct OsdSurface { hwnd: HWND, hdc_screen: HDC, hdc_mem: HDC, hbmp: HBITMAP,
                    old_bmp: HGDIOBJ, px: *mut u32, w: u32, h: u32 }

impl Drop for OsdSurface {   // 唯一释放路径，禁止手工半释放
    fn drop(&mut self) {
        // SelectObject(hdc_mem, old_bmp); DeleteObject(hbmp);
        // DeleteDC(hdc_mem); ReleaseDC(NULL, hdc_screen);
        // DestroyWindow(hwnd);
    }
}
```

- **仅在可见时分配**：`show()` 建 `OsdSurface`；`hide()` 立即 `drop` → DIB / DC / 窗口全部销毁。
- GDI+ 运行时：`show()` 首次 `GdiplusStartup`；**最后一次 hide 后 30s**（由一个一次性 waitable timer 触发，复用 §3.7 调度器）`GdiplusShutdown`。
- **诚实声明**：进程的 allocator / 堆会有少量缓存残留，测量口径为 `PrivateUsage` 差值，验收标准 **≤ 64 KiB**（不是严格的 0 字节，避免写不可验证的指标）。`handle` 数必须**完全**回到基线。
- 禁止在 `WM_PAINT` 里做 GDI+ 绘制（分层窗不走 `WM_PAINT`）；所有绘制都在 `update_layered()` 内一次性完成。

### 11.4 通知条（Toast）时序（`[硬契约]`）

| 阶段 | 时长 | 曲线 |
|---|---|---|
| fade-in | 120ms | `alpha = 0 → 255`，ease-out cubic |
| hold | `duration_ms - 400`（默认 1400ms） | alpha=255 |
| fade-out | 280ms | `alpha = 255 → 0`，ease-in quad |
| 总计 | **1800ms（默认）** | |

- 帧调度：动画期间 `SetTimer(hwnd, TID_ANIM, 16, 0)` 或 `CREATE_WAITABLE_TIMER_HIGH_RESOLUTION`（二选一，**必须**在动画结束 `KillTimer`/`timeEndPeriod`）。
- 预计算 96 级 alpha 表，避免每帧浮点插值（省 CPU，典型 60 帧动画总 CPU < 3ms）。
- 位置：默认 `MonitorFromWindow(foreground) → work_area` 底部居中，距底 96px（DPI 缩放）。可由配置改为右上角。
- 连续 Toast：队列上限 3，超出丢弃最旧；相同 `template` 在 800ms 内合并（§5.5）。
- 排版：标题 15px SemiBold + 副标题 12px Regular（按 DPI 缩放），圆角 8px，背景 `#E6111114`（88% 不透明），1px 边框 `#22FFFFFF`。
- 内容长度上限：标题 64 字符、副标题 96 字符；超长截断加省略号（**不允许**自动变大窗口）。

### 11.5 监控悬浮条与 FPS 采样

**数据源**

| 指标 | 来源 | 备注 |
|---|---|---|
| CPU 温度 / 风扇 RPM / 平台功耗 | EC（经 HAL 批量只读） | 1 Hz |
| GPU 温度 / 功耗 / 风扇 | NVML | 1 Hz，**仅悬浮条可见时** `nvmlInit` |
| FPS | ETW `Microsoft-Windows-DxgKrnl`，关键字 `Present (0x08000000)` | `[实测]` 该提供者与关键字在本机存在 |

**FPS 采样器状态机（`FpsSampler`）**

```
Idle ──(tracked_pid 出现 & 悬浮条可见)──▶ Starting
Starting ──StartTraceW(SESSION "MSILT-FPS", EVENT_TRACE_REAL_TIME_MODE)──▶ Tracing
Tracing ──EnableTraceEx2(DxgKrnlGuid, level=INFO, MatchAnyKeyword=Present)──▶ Running
Running ──每 1000ms──▶ Flush: 按 PID 聚合 Present::Stop 计数 → fps = n/Δt → EMA(0.3)
Running ──(悬浮条隐藏 | 游戏退出 | 锁屏 | 挂起)──▶ Stopping
Stopping ──ControlTrace(EVENT_TRACE_CONTROL_STOP) + CloseTrace + 线程退出──▶ Idle
```

**硬约束**
- 会话名 `MSILT-FPS` 全局唯一；`StartTraceW` 返回 `ERROR_ALREADY_EXISTS` → 复用已有会话（可能是上次崩溃残留），先 `ControlTrace(STOP)` 再启动。
- `ProcessTrace` 阻塞在专用线程（T5），`CloseTrace` 从 T2/T0 调用可使其返回。
- 只统计 **tracked PID 集合**（当前游戏 + 可选"前台进程"），其余事件在回调内即丢弃（不做表查找以外的分配）。
- 事件风暴保护：单秒事件数 > 20,000 → 采样降级为 250ms 聚合 + 记 `etw_lost_events` 估算（用 `EVENT_TRACE_PROPERTIES.EventsLost`）。
- 开销目标：`Running` 期间单核 CPU < 0.4%；`Idle` 期间会话不存在（**0**）。
- **禁止** D3DKMT 统计（`D3DKMTQueryStatistics` 属未文档化内核 thunk，且按进程取 present 统计会被部分反作弊视为可疑），`[已弃用方案]`。
- 未提权时（理论上不会有，daemon 恒 Highest）→ FPS 显示 `N/A (需要管理员权限)`。

**悬浮条布局**（逻辑像素，@100% DPI；`340×160`）：

```
┌──────────────────────────────┐
│ CPU  72°C  4.2GHz   38%      │  ← EC + PDH(可选)
│ GPU  68°C  115W     2400MHz  │  ← NVML
│ FAN  3200 / 3400 RPM         │  ← EC
│ FPS  238   (1% low 226)      │  ← ETW
└──────────────────────────────┘
```

- 半透明黑底 `#CC0B0B0D`，圆角 10px，等宽数字（`Consolas`/`Cascadia Mono` fallback）避免数字跳动。
- 隐藏时：停止 `FpsSampler` + `nvmlShutdown` + 释放 DIB。
- **反作弊声明**：ETW 是系统级追踪（与任务管理器/GPU-Z 同类，不接触目标进程）。悬浮条本身是普通顶层窗口。

### 11.6 准星几何规范（**normative，供 `@Pi-UI` 预览镜像**）

| 参数 | 取值 | 默认 |
|---|---|---|
| `shape` | `dot` / `cross` / `t` / `ring` / `dot_ring` | `cross` |
| `size` | 8–96 px（逻辑） | 16 |
| `thickness` | 1–6 px | 2 |
| `gap` | 0–64 px（中心留空半径） | 4 |
| `color` | sRGB + alpha（0–255） | `#FF00FF00`（纯绿） |
| `outline` | `off` / `1px` | `on`（`#80000000`）|
| `anchor` | `center` / `offset(dx,dy)` | `center` |
| `scale` | 0.5–4.0（在 `size` 之上再乘） | 1.0 |

- 几何定义（`cross`）：「上、下、左、右 4 条矩形，长度 `size/2 - gap`，宽度 `thickness`，从距中心 `gap` 处开始」。
- 渲染：`SmoothingModeAntiAlias` + PARGB 填充；`outline` 先画 1px 外扩暗色再画主色，保证亮背景下可见。
- 交互：热键循环预设（`Ctrl+Alt+C`，可改）；切换时弹 800ms Toast 显示当前形状名；**不**允许准星拦截鼠标（`WS_EX_TRANSPARENT`）。
- 多显示器：跟随"前景窗口所在显示器"，全屏变化时重建。
- 持久化：`crosshair.toml`，与 `automation.toml` 分离（避免规则热加载触发准星重建）。

### 11.7 DPI / 多显示器 / 全屏边界（诚实声明）

- **Per-Monitor V2**：所有尺寸按 `GetDpiForMonitor(MDT_EFFECTIVE_DPI)` 缩放，且在 `WM_DPICHANGED` 后重建 DIB（重绘成本 ~1ms）。
- 显示器热插拔：`WM_DISPLAYCHANGE` → 400ms 合并 → 重建（`[硬需求]` 不得出现"准星卡在已拔掉的显示器上"）。
- **独占全屏（Legacy Exclusive Fullscreen）下分层窗不可见**（DWM 被旁路）。可保证：无边框全屏 / 窗口化。UI 文案必须写这份边界，**不得**宣称"所有游戏都能显示"。

### 11.8 反作弊安全边界（红队自证清单）

| 类别 | 我们做 | 我们**绝不**做 |
|---|---|---|
| 进程 | `OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION \| SYNCHRONIZE)` 读路径/等退出 | `PROCESS_VM_READ/WRITE/OPERATION`、`ReadProcessMemory`、`WriteProcessMemory`、`CreateRemoteThread`、`NtMapViewOfSection` 注入 |
| 图形 | 普通分层窗 + `UpdateLayeredWindow` | Hook `Present`/`IDXGISwapChain`/`wglSwapBuffers`/Vulkan layer、DLL 注入、Overlay hooking、修改游戏渲染 |
| 输入 | `RegisterHotKey`（系统热键） | `SetWindowsHookEx(WH_KEYBOARD_LL/WH_MOUSE_LL)`、键鼠驱动、`SendInput` 模拟玩家操作 |
| 内核 | 无 | 任何内核驱动、`WinRing0`/`MSIo64`/`NTIOLib`、minifilter、`ObRegisterCallbacks` |
| 追踪 | ETW 真实时会话（系统级、与 WPA/PresentMon 同类） | 读写游戏内存、抓包式注入、篡改进程句柄表 |
| 窗口 | 普通 `WS_EX_TOPMOST` 窗口 | 修改游戏窗口样式/类名/标题、`SetWindowDisplayAffinity` 于游戏窗口 |

**结论**：本方案对主流反作弊（EAC / BattlEye / Vanguard / ACE）呈现的行为面 = "一个普通置顶窗口 + 一个系统 ETW 消费者 + 若干系统通知订阅"，等价于 Discord Overlay（非注入模式）/ GPU-Z / WPA 的子集。**`[待证]`**：仍需在 Phase 2 用 3 款在线游戏做一次实测回归（含 Vanguard 的 TPM/内核检测环境），结果写入 `docs/collab/04-pi-auto.md`。

---

## 12. `SoftwareDisabler` 规范（MSI 官软一键停用 / 无损还原）

### 12.1 实测对象清单（`[实测]`）

**服务（7 个，全部 `Start=2 AUTO_START`、`Type=0x10 WIN32_OWN_PROCESS`、当前 Running）**

| # | 服务名 | ImagePath | 职责推断 | 分档 |
|---|---|---|---|---|
| S1 | `MSI_Center_Service` | `C:\Program Files (x86)\MSI\MSI Center\MSI_Central_Service.exe` | MSI Center 主服务（传感器/功能编排） | **T1** |
| S2 | `MSI Foundation Service` | `C:\Program Files (x86)\MSI\MSI NBFoundation Service\MSIAPService.exe` | 平台基础服务（ACPI/WMI 通道相关） | **T1** |
| S3 | `MSI Sendevsvc`（Display: *Sensor dev service*） | `...\MSI NBFoundation Service\Sendevsvc.exe` | 传感器设备服务 | **T1** |
| S4 | `Micro Star SCM` | `C:\Windows\SysWOW64\MSIService.exe` | 传统 SCM：Fn 键 / 亮度 / 热键代理 | **T1** |
| S5 | `Mystic_Light_Service` | `...\MSI Center\Mystic Light\Mystic_Light_Service.exe` | 24 区灯效控制 | **T2** |
| S6 | `LightKeeperService` | `...\MSI Center\Mystic Light\LightKeeperService.exe` | 灯效保活 | **T2** |
| S7 | `MSITrueColorService`（*MSI True Color Service by Portrait Displays*） | `C:\Program Files\Portrait Displays\MSI True Color\MsiTrueColorService.exe` | 色彩管理 | **T3** |

**计划任务（2 个）**

| # | Task | Path | 动作 | 关键属性 | 分档 |
|---|---|---|---|---|---|
| K1 | `OmApSvcBroker` | `\` | `...\MSI NBFoundation Service\OmApSvcBroker.exe` | **`LogonTrigger` + `ExecutionTimeLimit=PT0S`（无限）+ `RunLevel=Highest` + 当前 Running（`LastTaskResult=0x41301`）** → 这是常驻的 ACPI 包代理，**EC 写的主要竞争源** | **T1** |
| K2 | `OneDC_Updater` | `\` | `...\MSI_Center_Updater\OneDC_Updater.exe OneDragonCenter` | `RunLevel=Highest`，当前 Ready | **T1** |

**自启项**

| # | 位置 | 键/值 | 原值 | 分档 |
|---|---|---|---|---|
| R1 | `HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\Run` | `MSI True Color` | `"C:\Program Files\Portrait Displays\MSI True Color\MsiTrueColor.exe" startup_folder` | **T3** |
| R2 | `HKCU\Software\Classes\Local Settings\...\AppModel\SystemAppData\9426MICRO-STARINTERNATION.MSICenter_kzh8wxbdkxb8p\MSI_Center_UWP` | `State` (DWORD) | `2`（Enabled） | **T3** |

> `HKCU\...\Explorer\StartupApproved\Run` 中**没有** MSI 项（`[实测]`），因此不要用 `StartupApproved` 的位图去"禁用"MSI 项 —— 那条路对本机不成立。

### 12.2 分档策略

| 档位 | 内容 | 停用后会发生什么 | 推荐场景 |
|---|---|---|---|
| **T0 观察**（默认） | 仅检测 + 冲突告警 | 无副作用 | 初次使用、评估期 |
| **T1 核心控制** | S1–S4 + K1 + K2 | MSI Center 完全失效；**Fn 亮度/风扇组合键可能失效**（S4）；官方 OSD 消失；EC 竞争消除 | 我们用 `msilt-*` 全面接管性能/风扇/电池 |
| **T2 灯效** | S5 + S6 | 官方灯效进程停止；我们可用 HID 直接写 24 区（前提 §5.3） | 使用我们的 RGB 主题 |
| **T3 色彩/自启** | S7 + R1 + R2 | True Color 服务与托盘自启消失；ICC 关联仍可用（L1） | 用我们的色彩管理 |
| **完全接管** | T1+T2+T3 | 官软全静默 | 目标形态 |

**预设**：`观察 / 性能接管(T1) / 灯效接管(T2) / 完全接管(T1+T2+T3)`。**禁止**提供"T2/T3 但不含 T1"的组合作为一键预设（T2/T3 与 T1 的技术依赖见 §12.5 顺序），但允许高级用户单独勾选（需二次确认）。

### 12.3 **绝对保留清单**（白纸黑字，代码内 `const PROTECTED: &[&str]`）

| 对象 | 原因 | 违反后果 |
|---|---|---|
| `C:\Windows\SysWOW64\msiapcfg.dll`、`msiltcfg.dll`（Sys32/SysWOW64） | **`root\WMI:MSI_ACPI` 的 MOF/Schema 提供者**；删了类就从 WMI 仓库消失 | **本项目直接死亡**（唯一 EC 通道） |
| `HKLM\SYSTEM\CurrentControlSet\Services\WmiAcpi`（`Start=3`） | Microsoft 的 ACPI-WMI 映射驱动，我们依赖它 | EC 通道全灭 |
| `NahimicService`（*A-Volute*） | 音频增强，与 MSI Center 独立；误停会导致音效丢失 | 用户投诉、非我们职责 |
| `Killer Analytics Service` / `Killer Network Service` / `Killer Provider Data Helper Service` | Intel Killer 网络栈；与 MSI Center 服务名相似极易误伤 | 断网风险 |
| `RtkAudioUniversalService`（Realtek） | 音频设备服务 | 无声 |
| `Huorong`（`sysdiag`，`HKLM Run` 项） | 第三方安全软件，绝不触碰 | 安全软件损坏/告警 |
| `MuMuPlayerService`（`HKCU Run`） | 第三方模拟器 | 用户环境破坏 |
| `MicrosoftEdgeElevationService` / `SecurityHealth` / `OneDrive` | 系统与微软组件 | — |
| 任何 `Win32_SystemDriver` 类型的服务、`bcdedit`、`BCD`、固件/BIOS 设置 | 砖机路径 | 不可恢复 |
| `msiserver`（Windows Installer） | 名字含 "msi" 但属于系统 | **典型误伤点，必须显式排除** |

> **命名陷阱**：`msiserver`、`MSiSCSI` 与 MSI 硬件无关。实现中**只允许白名单精确匹配**（大小写不敏感的精确相等），**禁止**任何 `*MSI*` 通配。

### 12.4 快照与日志 schema（`msi_backup.json`，v1）

```json
{
  "schema": 1,
  "created_at": "2026-11-22T09:30:00+08:00",
  "machine": { "host": "EXAMPLE-PC", "smbios_uuid_sha256": "…",
               "os_build": "26200", "ec_fw": "17S3EMS1.107", "bios": "E17S3IMS.112" },
  "tool_version": "0.1.0",
  "tier_applied": ["T1","T2","T3"],
  "services": [
    { "name": "MSI_Center_Service", "start_type": 2, "type": 16,
      "image_path": "…", "display_name": "MSI Center Service",
      "delayed_autostart": false, "was_running": true, "pid_at_snapshot": 1234,
      "state": "not_modified" }
  ],
  "tasks": [
    { "path": "\\", "name": "OmApSvcBroker", "enabled": true,
      "xml_sha256": "…", "xml": "<Task …>…</Task>",
      "principal": "Users/Author", "run_level": "Highest",
      "state": "not_modified" }
  ],
  "run_values": [
    { "hive": "HKLM", "key": "SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\Run",
      "name": "MSI True Color", "type": "REG_SZ", "data": "\"C:\\…\\MsiTrueColor.exe\" startup_folder",
      "vault_key": "SOFTWARE\\MSILT\\Vault\\Run\\MSI True Color", "state": "not_modified" }
  ],
  "uwp_startup": [
    { "pfn": "9426MICRO-STARINTERNATION.MSICenter_kzh8wxbdkxb8p", "task": "MSI_Center_UWP",
      "state_value": 2, "state": "not_modified" }
  ],
  "verification": { "ec_probe": { "attempts": 3, "ok": 3 }, "checked_at": "…" }
}
```

- `state` ∈ `not_modified | pending | applied | failed | restored`（**journal 语义**，崩溃可续）。
- 写入：`msi_backup.json.tmp` → `FlushFileBuffers` → `MoveFileExW(MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH)`。**禁止**原地覆盖写。
- 保留策略：最多 5 个历史快照（`msi_backup-<ts>.json`），滚动删除。
- 每次 `apply`/`restore` 追加 `msi_backup.journal`（逐对象一行 JSON Lines），用于断电恢复。

### 12.5 停用算法（有序，事务化）

```
APPLY(tier_set) -> Result<Report>
 0. PREFLIGHT
    - 提权检查：IsUserAnAdmin() 或 token 具备 SERVICE_CHANGE_CONFIG/SERVICE_STOP；
      否则返回 NeedsElevation（由 daemon 的 Highest 任务满足）。
    - 打开 SCM：OpenSCManagerW(SC_MANAGER_CONNECT | SC_MANAGER_ENUMERATE_SERVICE)；
      以 SERVICE_QUERY_CONFIG|SERVICE_CHANGE_CONFIG|SERVICE_STOP|SERVICE_START 逐个 OpenService 探测权限。
    - 保护清单自检：msiapcfg.dll / msiltcfg.dll 存在且 SHA-256 与内置基线一致；
      WmiAcpi 服务 Start 值 ∈ {2,3}（不得为 4/0）。
    - EC 只读探活 ×1（Get_EC 安全地址），记录基线。
    - 若检测到任意"非本工具"对 MSI_ACPI 的长时间占用 → 警告（不阻塞）。

 1. SNAPSHOT（§12.4）：
    服务：ReadService / QueryServiceConfig2W(SERVICE_CONFIG_DELAYED_AUTO_START_INFO) 
    任务：schtasks COM (IRegisteredTask::get_Definition → Xml, get_Enabled)
    Run：RegistryGetValueEx 原样（含类型与二进制）
    UWP：HKCU ...\SystemAppData\<PFN>\<Task>\State (DWORD)
    → 原子写盘 + fsync → 记录 journal

 2. QUIESCE（生产者先于消费者，**关键顺序**）
    a) 停止"UI/消费者"：
       STOP MSITrueColorService (T3)  [8s 超时]
       STOP Mystic_Light_Service / LightKeeperService (T2)  [5s]
       STOP MSI_Center_Service (T1)  [8s]
    b) 等待 500ms（让在途 ACPI 请求自然完成/超时）
    c) 停止"broker/提供者"：
       TASK END OmApSvcBroker        [5s]   ← 必须先 End 再 Disable
       STOP MSI Foundation Service   [8s]
       STOP MSI Sendevsvc            [5s]
       STOP Micro Star SCM           [5s]
    d) 若某服务 8s 内未停 → 记录 failed，**不**强杀（Force Kill 只在用户勾选"强制"时允许）

 3. APPLY（顺序：任务 → 服务 → Run → UWP）
    - ChangeServiceConfigW(name, SERVICE_NO_CHANGE, SERVICE_DISABLED(4), …)
      * 先置 Start=4（disabled）再 Stop？→ 否：**先 Stop 后 Disable**（避免 SCM 拒绝/竞态）。
    - IRegisteredTask::put_Enabled(VARIANT_FALSE)（对 K1 先 Stop/End）
    - Run 键：**不删除**，搬到 `HKLM\SOFTWARE\MSILT\Vault\Run\<name>`（保留原类型与数据），
      原值读取后 `RegDeleteValueW`。→ "搬家"而非"删除"，任何时刻可逐字节还原。
    - UWP：`State = 0`（Disabled）

 4. VERIFY（回读，逐对象）
    - 服务：QueryServiceStatusEx == SERVICE_STOPPED 且 Start == 4
    - 任务：Enabled == false
    - Run：值不存在于原位置、且存在于 Vault
    - UWP：State == 0
    - **EC 探活 ×3（间隔 300ms）**：全部成功 → `Verified`；
      任意失败 → 立即 RESTORE(T1) 并报告 `ECChannelLost`

 5. REPORT → 写 journal `applied`、`msi_backup.json`、IPC 事件 disabler.progress/complete
```

**退出码 / 结果码**：`Ok{ changed:[...] }` / `Partial{ changed, failed:[(obj, err)] }` / `NeedsElevation` / `Refused{ reason: Protected|NotMsiHardware|UnsupportedBuild }` / `RolledBack{ cause }`。

**幂等**：目标态已达 → 该项直接 `not_modified`，不产生系统调用日志噪声。

### 12.6 还原算法（100% 无损）

```
RESTORE(source = msi_backup.json | builtin_defaults)
 1. 读快照；校验 schema / machine.smbios_uuid_sha256。
    不匹配 → 返回 MachineMismatch（需 --force 才继续，且强制打印警告）。
 2. 若快照丢失/损坏 → 使用 builtin_defaults（本机实证的厂商默认值）：
    服务 Start=2、任务 Enabled=true、Run 原值、UWP State=2。
 3. 计算 diff：只还原 state ∈ {applied, failed} 的对象（**不**全量覆盖，尊重用户后续手动变更）
 4. 顺序（**与停用相反**：先底层 provider，后消费者）
    a) 服务配置恢复：ChangeServiceConfigW(Start=原值)（含 delayed autostart）
    b) 启动服务：MSI Foundation Service → MSI Sendevsvc → Micro Star SCM → MSI_Center_Service
       每步 StartServiceW + 轮询 QueryServiceStatusEx 至 SERVICE_RUNNING，超时 10s
       （**这是唯一允许的轮询**：服务启动状态无事件通知，1s×(1..10) 且仅还原时发生，可接受）
    c) 灯效：Mystic_Light_Service → LightKeeperService
    d) True Color：MSITrueColorService
    e) 任务：put_Enabled(TRUE)。**不主动 Run**（K1 由 logon trigger 起，K2 由更新器调度）
    f) Run 值：从 Vault 读取（原类型 + 原数据）→ RegSetValueExW 回原位 → 删除 Vault 项
    g) UWP：State = 原值(2)
 5. VERIFY：7 服务 Running、2 任务 Enabled、Run 存在、UWP=2、EC 探活 3/3
 6. 写 msi_restore_report.json，IPC 通知，UI 显示逐项 ✔/✘
```

**"无损"的三重保证**：
1. **不改文件**：不删除、不重命名、不移动任何 vendor 二进制（Run 键值搬进 Vault 只是注册表值搬迁）。
2. **不碰固件**：全程零 EC 写；`msiapcfg.dll` / `msiltcfg.dll` / `WmiAcpi` 全程不触碰（§12.3）。
3. **不碰用户态其它软件**：白名单精确匹配 + 保护清单双重校验，`const PROTECTED` 在运行时**再检查一遍**（防配置注入）。

### 12.7 崩溃 / 断电恢复

- daemon 启动时：若存在 `msi_backup.journal` 且有 `pending` 项 → 
  1. 逐项读回真实状态；
  2. 若真实状态 == 目标态 → 标 `applied`；
  3. 若既非原值也非目标态（**撕裂**）→ 以 `msi_backup.json` 原值为准修正（**恢复优先**，宁可"多开一个服务"也不留半死状态）；
  4. 完成后写 `recovered` 事件 + Toast。
- **绝不**在启动时自动重新停用（避免"用户已手动恢复 → 我们开机又给停了"）。

### 12.8 风险与砖机防御（逐条）

| 风险 | 机理 | 缓解 |
|---|---|---|
| **EC 写撕裂** | 停用瞬间官方软件正在写 ACPI 包，broker 进程被杀 → 固件状态半更新 | §12.5 顺序（消费者先停、500ms 静置、broker 后停）+ 全程零 EC 写 + 停后 3 次只读探活 |
| **充电上限/电池保养失效** | 不再有官方进程周期性下发充电阈值 | 我们在 T1 接管时必须提供等效功能，或 UI 明确提示"电池保养将由本工具接管/已失效" |
| **Fn 键失效** | `Micro Star SCM` 被停 | UI 二次确认文案 + 还原按钮就在同一页 |
| **删除 MOF provider** | 误删 `msiapcfg.dll` → `MSI_ACPI` 类消失 | 硬编码 PROTECTED + 代码中**根本不存在**删除文件的 API 调用（无 `DeleteFile` 依赖）→ 结构上不可能 |
| **Windows 更新回填** | 更新后服务被重置为 AUTO | 我们**不**自动对抗；检测到冲突 → OSD 提示"官方服务已恢复，是否重新停用？"，由用户决定（尊重用户控制权） |
| **误伤第三方** | 通配匹配 | 精确白名单 + PROTECTED 双检 + 单测（§14 T-12） |
| **还原失败** | 快照丢失/权限 | `builtin_defaults` 兜底（不依赖文件） |
| **安全软件告警** | 修改服务启动类型 | 纯 SCM API（`ChangeServiceConfigW`），不 patch 内存、不写驱动；文档公开行为 |

---

## 13. 向其它模块提出的接口契约（我方需求，已同步至 `docs/collab/04-pi-auto.md`）

### 13.1 需要 `@Codex-Core`（`msilt-core`）提供

| ID | 需求 | 理由 |
|---|---|---|
| C-01 | `CapabilityFlags` 必须**逐字段**对应 §5.2 的 `FieldId`（`CAP_FAN_CURVE` 等），并提供 `caps.supports(field) -> Option<Reason>` | A9 能力门控 |
| C-02 | `Paths` 结构必须显式解析**交互用户** profile（`WTSQueryUserToken` + `SHGetKnownFolderPath`），`msilt-automation` 不得调用 `dirs::config_dir()` | daemon 提权后 `%LOCALAPPDATA%` 会指向系统账户 |
| C-03 | IPC 帧必须携带 `seq: u64` + `deadline_ms`；服务端对过期帧直接丢弃并回 `Stale` | 防 UI 慢消费者拖垮 daemon |
| C-04 | `Error` 类型必须区分 `Timeout` / `Unsupported` / `Refused(safety)` / `Transient`，自动化据此决定"重试/停用/告警" | 重试策略不可由字符串匹配 |
| C-05 | 配置持久化必须走我们传入的 `AtomicFile` 抽象（`tmp → fsync → rename`） | §12.4 / §7 的原子写要求 |
| C-06 | `Mode` 枚举必须与 EC 包值解耦（`Mode` 是领域值，EC 值是 HAL 的事） | 防止 EC 值泄漏到规则与 UI |

### 13.2 需要 `@Codex-HAL` 提供

| ID | 需求 | 理由 |
|---|---|---|
| H-01 | **全部 WMI 调用必须暴露 async 版本**（`ExecMethodAsync` + `IWbemObjectSink`），并提供 `cancel_on_timeout(Duration)` | §2.2，固件挂死不拖垮 daemon |
| H-02 | `fan.set_curve` 必须是**单次固件事务**，并**回读校验**（core 已用 `WriteGuard::prepare(FanCurve)` 实现，HAL 只需保证 `EcRegisterIo` 的 6+6 次往返不超时），并**回报实测事务耗时** | 我按 1 事务记账（§4.4）；需要真实耗时来定超时与 OSD 反馈阈值 |
| H-03 | 提供**批量**遥测接口 `ec.read_telemetry() -> Telemetry{ cpu_temp, gpu_temp, fan1, fan2, power_w }`（一次 WMI 往返） | 5 次往返 = 5 倍延迟与 5 倍超时风险 |
| H-04 | `display.set_refresh(hz) -> AchievedHz`（必须回读实际生效值，而非假设成功） | DRM/全屏独占会拒绝 |
| H-05 | 明确 `IcmProfile` L1/L2 边界（§5.3）：L1 = CMS 关联；L2 是否依赖 `IntelCSC.exe` 需在 `HARDWARE_PROTOCOLS.md` 给出结论 | 决定 Phase 1/2 能力位 |
| H-06 | NVML **必须惰性初始化 + 可显式 `shutdown`**，并提供"是否已唤醒 dGPU"的可观测状态 | §11.5 与"0 显存占用"硬指标 |
| H-07 | `MSI_Event` 的 P-A1..P-A4（§3.8）必须在 Phase 0 结案，并写入 `HARDWARE_PROTOCOLS.md` | 决定"零轮询"是否可再进一步 |
| H-08 | EC 遥测的**单位/符号/有效位**必须与 `ec_maps/ms-17s3.json` 对齐，且未知固件前缀 → 只读降级 | ARCHITECTURE §1.3 |
| H-09 | `SoftwareDisabler` 的每个操作返回可回读的 `ObservedState`（不是"我调用了 API"） | §12.5 step 4 的 VERIFY |
| H-10 | `EcRegisterIo::write_byte` 必须是**不可直接调用**的（sealed trait / 令牌参数），只有 `WriteGuard` 能铸造写令牌 | 否则任何后续 HAL 代码都能绕过 `WriteGuard` 直写 EC（§1 A3/A4 的结构性保证） |
| H-11 | `WriteGuard` 的**恢复协议**必须文档化：谁在什么证据下重建 guard（现只有 `recovery_required()` 只读，无清除路径） | `Indeterminate` 之后 guard 永久失效，必须与 §2.2 的 3 次只读探活接上 |
| H-12 | 若 EC 现有基线本身**违反安全下限**（例如上次被别的工具写成 0% duty），`WriteGuard` 必须仍允许「把曲线调高」的写入，并把回滚基线 clamp 到下限，而不是整体拒绝 | 当前 `baseline.validate_against(&floor)` 会让「不安全的机器永远无法被修好」（详见协作帖 Q-11） |

### 13.3 需要 `@Pi-UI` 提供

| ID | 需求 | 理由 |
|---|---|---|
| U-01 | 准星预览必须使用 §11.6 的**同一套几何参数**（可以自带渲染，但数值语义必须一致），并在 UI 里提供 1:1 预览 | 用户看到的 ≠ 游戏里看到的，是体验灾难 |
| U-02 | 规则列表要显示**诊断三列**：`命中/未命中原因`、`被更高优先级抑制`、`能力缺失（原因）` | 可解释性 = 超越 LLT 的关键 |
| U-03 | 规则编辑必须提供 `DryRun`：用当前 `WorldSnapshot` 试算命中结果（IPC `automation.dryrun`） | 避免"配了不生效" |
| U-04 | 规则字段枚举**从 IPC `automation.schema` 拉取**，禁止在 Slint 侧硬编码 `FieldId`/`Action` 列表 | 双份枚举必然漂移 |
| U-05 | 停用器页面必须逐项显示 `对象 / 当前状态 / 目标状态 / 影响面`，并有"一键还原"同页入口 | 危险操作的可理解性 |
| U-06 | 不得自行创建 OSD/准星窗口（那是 daemon 的独占职责），只能通过 IPC 触发预览 | 双窗口 = 双份内存 + 双份反作弊行为面 |

---

## 14. 测试计划与验收矩阵

| 编号 | 用例 | 方法 | 通过标准 |
|---|---|---|---|
| T-01 | 零轮询证明 | 静置 60s + `powercfg /energy` + ETW `Kernel-Power` 唤醒计数 | 唤醒 ≈ 0/s，CPU 增量 < 60ms |
| T-02 | 规则求值正确性 | 1000 条随机规则 + 随机事件序列，与"朴素解释器"比对 | 决策序列 100% 相同 |
| T-03 | 抖动手册 | 1 Hz / 2 Hz AC 插拔 60s（软件模拟注入，不真插拔） | EC 写 ≤ 24 次；`flap` 冻结生效 |
| T-04 | 防抖时序 | 注入 799ms/801ms/1500ms/1501ms 边界 | 与 §4.3 定义逐毫秒一致 |
| T-05 | 租约栈 | 3 条规则 + 用户手动 + 场景入栈/出栈 | 出栈后精确回到入场前值 |
| T-06 | HAL 超时 | Mock HAL 注入 5s 阻塞 | daemon 消息泵延迟 < 5ms，`hal=Degraded` |
| T-07 | 队列背压 | 10k 事件毫秒级涌入 | 无 OOM，丢弃有计数，无 panic |
| T-08 | OSD 内存 | 显示/隐藏 × 100 次 | handle 回到基线，WS Δ ≤ 64 KiB，无 GDI 泄漏 |
| T-09 | OSD 视觉 | 100%/150%/200% DPI 逐像素对比基准图 | 差异 < 2%（抗锯齿容差） |
| T-10 | 静态导入表 | `dumpbin /imports msilt-daemon.exe` | **不得**出现 `d2d1/d3d11/dxgi/d3d9`；不得出现 `EasyHook/Detours` |
| T-11 | 反作弊回归 | 3 款在线游戏各 30min | 无封禁/无检测日志；`[待证]` |
| T-12 | 停用器白名单 | 遍历全机服务名，断言 `PROTECTED` 与 `msiserver/MSiSCSI` 不被命中 | 0 误伤 |
| T-13 | 停用/还原幂等 | apply→apply→restore→restore | 第二次为全 `Noop`，系统状态字节级一致 |
| T-14 | 断电恢复 | `apply` 进程中途 `TerminateProcess` | 重启后 journal 恢复至一致状态 |
| T-15 | 脚本沙箱 | 恶意脚本（删文件/提权/长跑/大输出） | 全部被拒或降权隔离；daemon 完整性级别不变 |
| T-16 | 配置迁移 | v1→v2 迁移 + 损坏配置 | 旧配置保留 + `.bak` + 不崩溃 |

---

## 附录 A：`msilt-automation` 公共 API 草案

```rust
// ---- 对外唯一入口（daemon 持有）
pub struct AutomationEngine { /* 私有 */ }

impl AutomationEngine {
    pub fn new(cfg: EngineConfig, hal: HalHandle, sink: EventSink) -> Result<Self>;
    pub fn load(&self, toml: &str) -> Result<LoadReport>;      // 校验 + 规范化 + 迁移
    pub fn post(&self, ev: RawEvent);                          // 非阻塞，内部有界
    pub fn snapshot(&self) -> Arc<WorldSnapshot>;              // 三缓冲读
    pub fn stats(&self) -> Stats;
    pub fn dryrun(&self, rule: &RuleId, world: &WorldSnapshot) -> DryRunReport;
    pub fn set_user_lease(&self, field: FieldId, v: FieldValue) -> Result<()>;
    pub fn release_user_lease(&self, field: FieldId) -> Result<()>;
    pub fn scenario_push(&self, s: ScenarioSpec) -> ScenarioHandle;
    pub fn scenario_pop(&self, h: ScenarioHandle) -> Result<()>;
}

// ---- HAL 边界（由 msilt-hal 实现，async）
#[async_trait]
pub trait HalPort: Send + Sync {
    async fn apply(&self, field: FieldId, value: FieldValue, budget: Duration) -> Result<Observed>;
    async fn read(&self, field: FieldId) -> Result<Observed>;
    async fn telemetry(&self) -> Result<Telemetry>;
    fn caps(&self) -> &CapabilityReport;
}

// ---- 事件输出（daemon 消费）
pub enum EngineEvent {
    RuleFired { rule: RuleId, at: Instant, trigger: TriggerKind },
    Toast(ToastSpec),
    StateChanged { field: FieldId, from: FieldValue, to: FieldValue, by: Owner },
    Conflict { field: FieldId, contenders: Vec<(Owner, i16)> },
    Unstable { field: FieldId },
    ConfigInvalid { error: String, line: Option<u32> },
    HalHealth(HalHealth),
}
```

**依赖约束**：`msilt-automation` 只 `use msilt_core::*` 与 `msilt_hal::traits::*`；**禁止**直接 `use windows::*` 之外的任何 Win32 调用（事件源注册属于 `msilt-daemon`，不在引擎内）。引擎本身应能在 `cargo test` 中完全脱离 Windows 运行（事件由测试注入）。

---

## 附录 B：常量表

| 常量 | 值 |
|---|---|
| `GUID_ACDC_POWER_SOURCE` | `{5D3E9A59-E9D5-4B00-A6BD-FF34FF516548}` |
| `GUID_BATTERY_PERCENTAGE_REMAINING` | `{A7AD8041-B45A-4CAE-87A3-EECBB468A9E1}` |
| `GUID_POWER_SAVING_STATUS` | `{E00958C0-C213-4ACE-AC77-FECCED2EEEA5}` |
| `GUID_CONSOLE_DISPLAY_STATE` | `{6FE69556-704A-47A0-8F24-C28D936FDA47}` |
| `GUID_MONITOR_POWER_ON` | `{02731015-4510-4526-99E6-E5A17EBD1AEA}` |
| `GUID_LIDSWITCH_STATE_CHANGE` | `{BA3E0F4D-B817-4094-A2D1-D56379E6A0F3}` |
| IPC 管道名 | `\\.\pipe\MSIToolkit-IPC-v1` |
| OSD 窗口类 | `MSILT_OSD` / `MSILT_CROSSHAIR` |
| ETW 会话名 | `MSILT-FPS` |
| ETW DxgKrnl `Present` 关键字 | `0x08000000` |
| 配置目录 | `%LOCALAPPDATA%\MSI-Toolkit\` |
| 停用器 Vault 根 | `HKLM\SOFTWARE\MSILT\Vault\` |
| 默认 Toast 时长 | 1800 ms |

---

## 附录 C：本次实测证据摘要（可复跑）

**C.1 服务/任务/Run 清单** → §12.1（PowerShell：`Get-Service`、`Get-ScheduledTask`、`Get-ItemProperty HKLM:\...\Run`、`HKLM:\SYSTEM\CurrentControlSet\Services\<name>` 的 `Start`)
关键原始值：7 个服务全 `Start=2`、`Type=16`；`OmApSvcBroker` = `LogonTrigger` + `ExecutionTimeLimit=PT0S` + `RunLevel=Highest` + `State=Running`（`LastTaskResult=0x41301` 仍在运行）；`OneDC_Updater` = `Ready`；`HKLM Run "MSI True Color"` 存在；UWP `MSI_Center_UWP\State=2`。

**C.2 显示与 HID** → `EnumDisplaySettings` 枚举 2560×1600 仅 `60,240`；`Get-PnpDevice` 见 `VID_1462&PID_1603` 两个节点 `OK`；`spool\drivers\color` 中 `TrueColor_*.icm` 共 **6** 个。

**C.3 `MSI_Event` 订阅尝试（失败，待证）** → 
```powershell
$q = New-Object System.Management.WqlEventQuery("SELECT * FROM MSI_Event")
$w = New-Object System.Management.ManagementEventWatcher("root\WMI", $q)
$w.WaitForNextEvent()   # → "无法分析的查询。"
```
在**非提权**会话下失败。Phase 0 必须用提权 daemon/probe 重跑，并给出 "可订阅 / 不可订阅 + 原因" 的结论。

**C.4 提供者可用性** → `Microsoft-Windows-DxgKrnl`（关键字 `Present=0x08000000`、`HistoryBuffer=0x4000`）与 `Microsoft-Windows-Kernel-Process`（`WINEVENT_KEYWORD_PROCESS=0x10`）均存在。

**C.5 会话权限** → `whoami /groups` 显示 `BUILTIN\Administrators … deny only`，即当前 shell 非提权；由此**实测到**：本机的自动化/停用器必须由 `Highest` 计划任务承载，而不能依赖用户手动提权。

---

*本文档由 `@Pi-Auto` 独占维护。任何其它模块对其内容的修改必须通过 `docs/collab/04-pi-auto.md` 提出。*
