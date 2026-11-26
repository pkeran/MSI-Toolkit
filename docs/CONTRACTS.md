# MSI-Toolkit 核心契约 v1

本规范定义已落地的 Rust 接口、EC 事务与 IPC 编解码，以及 Windows 宿主约束。`msilt-cli` 的 Windows 客户端约定见 §5.1。

## 1. Crate 边界与状态所有权

| Crate | 依赖与职责 |
|---|---|
| `msilt-core` | `serde`、`serde_json`、`thiserror`；领域模型、配置、事务护栏、同步 Trait、纯内存 IPC 编解码。无平台调用、运行时或后台线程。 |
| `msilt-hal` | 实现 Core Trait；独占 Windows/COM/WMI/HID/CCD/NVML 句柄、固件映射和错误归一化。 |
| `msilt-automation` | 实现 `AutomationEngine`；计算规则、租约提案和诊断，仅依赖 Core。 |
| `msilt-daemon` | 依赖 Core、HAL 和 Automation；唯一硬件写入仲裁者、配置持久化者和对外快照发布者。 |
| `msilt-ui` / `msilt-cli` | 只依赖 Core 契约，通过管道读写语义状态，不直接访问 HAL。UI 渲染后端另行选型。 |
| `msilt-probe` | 依赖 Core/HAL，只使用已确认无副作用的读取入口；不创建 `WriteGuard` 或调用系统变更接口。 |

所有 UI、CLI、托盘、热键和自动化写入在 daemon 汇合。HAL 回读值是事实来源；请求参数、正在写入的目标和 OEM 候选数据不能作为已生效值发布。`SystemSnapshot` 初始 `revision=0`、`status=starting`、`ecWriteStatus=unavailable`，能力位为零，未知或确定已失效的字段为 `null`。宿主按需在本地保存各域的观测时间、来源和失效理由，v1 不给每个标量附元数据。任何“出厂曲线”预览须单独标注，不能写入当前实态字段。同一 daemon 进程内，状态、能力或用户租约变更后递增 `revision`，遥测刷新不递增；重连重新以握手快照为基准。配置另有独立修订号，不能与快照修订号混用。

`capabilities` 表示已确认可读或可用的功能，`writable` 是其中当前允许作为写入目标的子集。`hardware` 持有实际值和候选集：`supportedPerformanceModes`、`supportedChargeLimits`、`supportedRefreshRates`、`supportedColorProfiles` 为空即没有可用写入选项；同一功能内再按通道与对应约束判定。`manualOverrides` 列出当前人工占用的 `ControlTarget`。`CapabilityFlags` 在线上是 `u32`，客户端保留未知位并仅解释已知位。

| 位 | Rust 常量 | 语义 |
|---:|---|---|
| 0 | `PERFORMANCE_MODE` | 场景档位 |
| 1 | `FAN_CURVE` | 至少一个 CPU/GPU 风扇曲线通道；分别检查 `limits` |
| 2 | `COOLER_BOOST` | 强冷开关 |
| 3 | `CHARGE_LIMIT` | 电池充电上限 |
| 4 | `DISPLAY_REFRESH_RATE` | 内屏刷新率 |
| 5 | `DISPLAY_BRIGHTNESS` | 内屏亮度 |
| 6 | `COLOR_PROFILE` | 已安装且允许的 WCS 配置关联 |
| 7 | `KEYBOARD_LIGHTING` | 24 区 RGB |
| 8 | `GPU_MUX` | 读取已验证的当前/待重启模式；v1 无写命令 |
| 9 | `TELEMETRY` | UI 可订阅不唤醒独显的传感器采样 |
| 10 | `OSD` | daemon 可提供通知 |
| 11 | `AUTOMATION` | daemon 具备规则引擎 |

| `PerformanceMode` | JSON 与 `MSILT_ACTIVE_MODE` | UI 示例文案 | 固件值 |
|---|---|---|---|
| `SuperBattery` | `superBattery` | 超级省电 | HAL 按板型/固件映射 |
| `Silent` | `silent` | 静音 | 同上 |
| `Balanced` | `balanced` | 平衡 | 同上 |
| `ExtremePerformance` | `extremePerformance` | 极致性能 | 同上 |

自定义风扇曲线独立于场景档位；改曲线不意味着切为 `Balanced` 或新建 `Custom` 档。仅在 HAL 证实独立档位后扩展。充电上限 `100` 表示不设低于 100% 的策略上限，仅在 HAL 验证并列入候选集后开放；不表示绕过充电控制器。亮度 `0` 是面板提供的最低百分比，不代表关屏。色彩配置使用最长 64 字节的 ASCII 字母、数字、`_` 或 `-` 标识，由 HAL 解析成发行/系统认可的配置；输入不是文件路径或色域命令。

## 2. Rust Trait 与调度契约

精确签名以 `crates/msilt-core/src/traits.rs` 为准：

| 接口 | 调用与返回约定 |
|---|---|
| `HardwareRead::inspect(&mut self) -> HalResult<HardwareReport>` | 初次连接、设备/显示变更、挂起恢复、命令完成时按需采集。`readable`、`controls_detected` 和 `state` 是探测结果，`controls_detected` 不授予写权限。禁用状态或数据缺失时保留 `None`。 |
| `HardwareRead::sample_telemetry(&mut self) -> HalResult<TelemetrySample>` | 一次调度返回一批实际读数；内部使用已验证的最少 WMI 往返，不承诺固件只有一次调用。温度 `i16` 摄氏度、RPM `u16`、EC 报告档位 `u8` 百分比、GPU 功耗 `u32` 毫瓦、UTC Unix 毫秒；未知为 `None`。EC 报告档位和 RPM 分开，零是可能的读数。 |
| `EcRegisterIo::read_byte` / `write_byte(&AuthorizedEcWrite)` | 仅 HalExecutor。`write_byte` 借用不可公开构造的令牌，令牌绑定地址和值；HAL 不得另行公开原始写入口。要求读为真实回读，非上次请求缓存。 |
| `PlatformControl` | 只接非 EC 的固定语义操作：刷新率、亮度、色彩关联和键盘灯。刷新率与亮度返回实测值；色彩与灯光成功也需可验证的反馈或读回，否则保留只读/不支持。绝不在这里绕过 EC 护栏。 |
| `AutomationEngine::on_event(&mut self, &AutomationEvent, AutomationContext, &mut Vec<AutomationAction>)` | 在协调器执行，Win32 回调与消息泵只投递。输入含只读快照和统一进程单调时间/UTC；输出容器复用、最多接收 32 项，由协调器限制并记录丢弃，不持 HAL。 |
| `AutomationEngine::next_deadline_monotonic_ms(&self) -> Option<u64>` | 与 `AutomationContext::monotonic_ms` 同时基、同纪元，`None` 取消调度。`ClockChanged` 用于重新计算日历规则；`TimerElapsed` 唤醒引擎，引擎按当前时基处理所有到期项目和过时编号。 |

`AutomationAction::{Acquire, Release, Notify}` 用户态内存传递，不是 IPC 接口。租约由规则/场景 ID 和 `ControlTarget` 标识，CPU 与 GPU 各自是目标；手动租约由协调器另管，默认显式释放，优先于自动化。释放即用最新快照重新仲裁，选择现存最佳租约或首次覆盖时读回的基线；仍需能力、当前约束和护栏验证，不能直接重放旧寄存器。`ControlSettled` 携带语义结果，具体已生效值读发布快照。事件 payload 使用不透明进程实例键、安装/白名单匹配后 App ID、外接屏数量；原始 HANDLE、程序路径和 ETW 数据不进入快照或线上协议。

HAL 唯一 worker 串行执行，回调只能报告结果。`HalError::Busy` 表示未启动 I/O；`Io` / `NativeIo(u32)` 表示调用已完成但失败，可能有副作用；`TimedOut` 表示实际调用可能仍在途，禁止默认重试、接续调用或并发补回滚。Win32/HRESULT 原始码仅留本地诊断。WMI 可在 HAL 内用 `ExecMethodAsync`/回调和有界等待，Trait 保持同步；建议首轮预算 EC 单次读 300 ms、写 1200 ms，整事务预算按实际往返和实测分位数另定。取消请求不是执行已终止的证据，必须跟踪原操作完成。

## 3. EC 映射与写入事务

`EcMap::parse_json` 读取 UTF-8 严格 v1 对象（≤64 KiB）。`configs/ec_maps/ms-17s3.json` 是 HAL 的研究封套；HAL/daemon 的 loader 验证外层并仅将 **`guardMap`** 子对象交给解析器。`verified`、研究地址和能力证据不能授权写入；发布产物只从受保护、审核过的映射加载写白名单。开写前必须匹配已批准的板型和板修、BIOS、完整 EC 固件、WMI 版本及协议指纹；前缀匹配只是 Core 的拒绝门，不单独授权。首机型交付的 `guardMap` 白名单和曲线为空，Phase 0 保持只读。

| 字段 | 规范 |
|---|---|
| `schemaVersion` | `1` |
| `boardId` | 规范化主板 ID，1–32 字节大写 ASCII、数字和连字符，精确匹配，不使用人类显示名或 `REV` 字符串。 |
| `firmwarePrefixes` | 1–16 个 `<大写族名>.<数字版本>`。族名 4–16 字节精确匹配，数字版本 1–8 字节从开头匹配。`17S3EMS1.1` 可覆盖 `17S3EMS1.107`，不覆盖另一固件族或 `.2xx`。上线前须核实覆盖范围；首机型选择完整版本 `17S3EMS1.107`，宿主另核对批准的板修/BIOS/完整固件组合。 |
| `denyWrite` | 可省略；地址为 JSON `0..255` 整数或 `"0xHH"`。与内置 `0x50`、`0x51` 取并集，黑名单始终优先，覆盖直接写、曲线和回滚。首机型 map 还拦 `0xD1`、`0xD8` 等。 |
| `writeAllowlist` | 可省略，默认为空；条目 `{address, values}`，值为非空且无重复的字节数组。只适用于已验证的整字节写语义；寄存器原值也必须在列表中才可安全恢复。未验证的掩码读改写、WMI 包写与 MUX 不走此接口。 |
| `fanCurves.cpu` / `.gpu` | 可省略。每通道有 `points`（恰好六项 `{temperatureC, minimumDutyPercent, dutyRegister}`）、可选 `maxDutyPercent`（默认 100，范围 1–150）、可选 `fixedAnchor`（默认 false；true 时首点档位必须等于该点映射值）。一个地址只能有一种写绑定，曲线不能通过普通寄存器入口改写。 |

曲线要求固定温度结点严格递增，足够安全的下限由已验证的工厂策略提供，最后一点下限必须非零。请求档位非递减，每点满足映射下限和上限；首次覆盖须验证原曲线也满足安全要求。`100..150` 之间可选的 OEM 档位是 firmware 定义的控制量，并非 RPM 或散热能力保证。只有验证档位编码是同序、独立可回读的单字节映射、并证明固件层安全行为后才发布写能力；需原子 package 的机型应另定受护栏约束的事务，不能拆成未经验证的字节操作。

1. **授权与栏杆。** 宿主首先验证能力、会话状态、固件身份、映射来源、官软互斥和节流，之后在唯一 worker 上使用与该设备绑定的 `WriteGuard`。`prepare` 先验证固件、黑名单、白名单或风扇约束；未知固件返回 `ReadOnlyFirmware`。没有地址或曲线可写时保持 `unverifiedMapping`。
2. **写前快照。** 原值在同一事务内读取；`PreparedWrite` 独占同一 I/O 对象，`snapshot()` 暴露 `(address, before, after)`，供宿主写有界事务日志，`rollback_plan()` 生成逆序恢复数据，不授予写权限。宿主如要求跨崩溃恢复，必须在 `commit` 前持久化安全快照与未结标志；重启后先对账，不自动重放。放弃 `PreparedWrite` 不产生写入。
3. **回读与回退。** 风扇升档从热端到冷端、降档从冷端到热端；在已验证的独立字节语义下每个中间曲线仍单调。跳过原值相同项，每次写后回读，最后复核本事务全部曲线点。实际机型若需同时核对双通道、模式或其他联动状态，由 HAL/daemon 在发布前验证，未验证不得发布能力。已结束的失败按反序尝试还原已尝试写入项（包括可能有副作用的失败项）并回读。若回滚失败、基线不安全或一次调用超时结果不明，锁住后续 EC 写。超时时不启动回滚。`WriteReceipt` 持有已验证的字节变更和快照；HalExecutor 另记录 `prepare` 前至 `commit` 后的单调时长、实际往返和请求/实测语义结果，不给 Core 注入系统时钟。

**恢复条件：** `RecoveryRequired` 不可由 IPC 或重复探活直接解除。HalExecutor 必须确认原调用全部退出且未出现替代写者，重新核对主板/固件/映射，对受影响的全部字段重读并确定当前值/曲线安全；必要时让用户在受支持的 OEM/固件恢复路径恢复安全基线。完成该过程后 daemon 才能重建 guard。三次读成功可作为链路健康信息，不能替代完成证据或安全基线。不会用 `clamp` 生成一个从未读到的“原值”。

`DaemonStatus`：`starting`（初始化）→ `ready`（提供支持的功能）或 `degraded`（后端/安全门引起的受限状态），故障解决后可恢复，关停进入 `stopping`。`EcWriteStatus` 分别报告 `unavailable`、`readOnlyFirmware`、`unverifiedMapping`、`conflictDetected`、`enabled`、`recoveryRequired`；不可写状态立即清除相应 `writable` 位，不屏蔽独立的非 EC 功能。固件/后端换代或挂起恢复触发只读重查，在状态不明期间不写。

## 4. Named Pipe IPC v1

### 4.1 帧、顺序与握手

端点 `\\.\pipe\MSIToolkit-IPC-v1`，本地 duplex、byte mode、overlapped I/O。一条连接双向持续收发：客户端只发 Request，daemon 发 Response 和 Event。每帧为 4 字节无符号小端长度 `N`，紧跟 `N` 个 UTF-8 JSON 字节（不含长度头、无 BOM）；`1 ≤ N ≤ 65536`。入站先验长度再扩展缓冲，允许分片、粘包。`decode_frame` 返回第一帧及消费字节数，`None` 时继续收；`encode_frame_into` 复用缓冲并阻止输出超限。方向错误、非法长度、无法解析的帧直接关闭，不尝试寻找下一帧。

```json
{"type":"request","message":{"id":1,"command":{"op":"hello","version":1,"client":"ui"}}}
{"type":"request","message":{"id":2,"command":{"op":"setSubscriptions","topics":{"state":true,"telemetry":true,"osd":true}}}}
{"type":"request","message":{"id":3,"ttlMs":700,"command":{"op":"control","expectedRevision":12,"change":{"kind":"setPerformanceMode","mode":"balanced"}}}}
{"type":"response","message":{"id":3,"result":{"status":"error","error":{"code":"revisionConflict"}}}}
{"type":"event","message":{"event":"osdNotify","revision":13,"kind":"performanceMode"}}
```

JSON 层 `type` 为 `request | response | event`，`message` 为对应内容。`id` 是每连接从正数起严格递增的 `u64`，Response 使用原 ID；Event 无请求 ID。客户端至多一条请求在途，等待时仍接收事件。`ttlMs` 可省略（默认 `5000`，有效范围 `1..=10000`），基于服务端接收完整请求时的单调时间计算排队期限。过期且尚未执行的请求返回 `requestExpired`；已开始的事务依服务器安全预算完成或隔离。无需第二套全局帧序号；有序管道负责传输，客户端只应用比当前更新的快照修订号，以握手区分 daemon 代际。`config` 的 `revision` 独立比较。支持同修订号的 `osdNotify`；遥测坐标用 `sampledAtUnixMs` 显示时间，系统调时可能回拨，连接内到达顺序仍有效。

连接状态：`Connected` → `Authenticating`（5 s 内收首个 `hello`，先核验 token）→ `Idle`（可收命令；默认无订阅）↔ `Queued/Executing`（事件仍可推送）→ `Closing`。版本必须为 1，不匹配返回 `incompatibleVersion` 后关闭。不重复握手；异常断开清除该连接订阅和未开始的命令；已受理 EC 事务按原流程完成/回滚。断线的客户端先重连读新状态，不盲重放写请求。需要不兼容字段或命令时迁移至 v2 命名管道，不在 v1 暗换编码。

### 4.2 命令、推送与背压

| `command.op` | 参数 | 成功的 `result.reply.kind` |
|---|---|---|
| `hello` | `version`, `client`（`ui` / `cli`） | `hello`: `version` 与当前 `snapshot` |
| `getSnapshot` | 无 | `snapshot`: 当前 `snapshot` |
| `setSubscriptions` | `topics`: `{state, telemetry, osd}`，缺项为 false，全量替换 | `subscriptions`: 已生效 `topics` 与当前 `snapshot`；先排应答，再发送相应推送 |
| `control` | `expectedRevision`, `change` | `applied`: 回读、提交且已发布的 `snapshot` |
| `releaseManualOverride` | `expectedRevision`, `target` | `applied`: 释放并重新仲裁后的 `snapshot` |
| `getConfig` | 无 | `config`: `config` 与配置 `revision` |
| `setConfig` | `expectedRevision`（配置修订号）, 完整 `config` | `config`: 校验并原子落盘的 `config` 与新配置 `revision` |

`change.kind` 的字段：`setPerformanceMode{mode}`、`setFanCurve{channel,curve:{points:[{temperatureC,dutyPercent};6]}}`、`setCoolerBoost{enabled}`、`setChargeLimit{percent}`、`setDisplayRefreshRate{hz}`、`setDisplayBrightness{percent}`、`setColorProfile{profileId}`、`setKeyboardLighting{zones:[{r,g,b};24]}`。目标 `kind` 为去掉 `set` 的对应 lower camel case 名，只有 `fanCurve` 额外带 `channel`（`cpu` / `gpu`）。写入仲裁点重新比较修订号与能力、候选值、竞争和安全条件；过时返回 `revisionConflict`，客户端从新快照重算。拖动时本地预览，提交时发送一次，控件用回读值结束事务。v1 没有原始寄存器、进程/脚本执行、写映射、任意路径、服务更改或 MUX 改动。后续 MUX 必须独立验证且只能预约下次重启，不自动重启。

| `message.event` | 载荷与投递 |
|---|---|
| `stateChanged` | `{snapshot}`，完整已提交快照，可合并未发送的旧快照。无变化不推。 |
| `telemetryTick` | `{sample}`，最近一批；允许覆盖未发送旧样本，采样失败字段为 `null`，不伪造或插值。 |
| `osdNotify` | `{revision,kind}`，类型 `performanceMode | coolerBoost | displayRefreshRate | automationApplied`。用快照值渲染本地模板；可丢弃积压/过期通知。 |

**订阅分级。** 无订阅为事件待机；`state`/`osd` 不启动传感器。仅至少一个已认证、被识别为已安装 UI 的连接成功订阅 `telemetry` 时启动唯一的 1 s 采样节奏，CLI 不能激活；订阅者共享一次采样。连接级订阅先更新再确认，最后一个订阅者取消或断开时立即撤销定时器、移除待采任务并丢弃在途采样结果，以代际号隔离迟到回调。暂停/恢复也应用同一规则；下一 tick 若已有采样在途则跳过，HAL 忙时不堆积。默认 GPU 数据限无唤醒读链路，不初始化 NVML 来满足 1 Hz；需要主动唤醒独显的监控功能另立能力与授权。

**有界传输。** 全局 ≤8 连接、≤32 待处理事务，回应优先、每连接最多一个已编码的发送帧，另保存一条待回应结果和各推送 topic 的最新引用。回调不阻塞，消息泵/协调器不等待 HAL 或管道写完成。缓冲按需增长，处理后回收异常大容量，等待头/半帧/在途发送设 5 s 进度期限（空闲健康长连接不定期 ping）；消费者不读则断开，不能悄悄丢已经受理的请求回应或阻塞其他连接。没有引擎锁跨越传输等待。

### 4.3 稳定错误码

错误载荷只有 `code`，由 UI/CLI 本地翻译。HAL 原生码、路径、寄存器地址、固件回包不进入错误响应。

| `code` | 含义与处理 |
|---|---|
| `invalidRequest` | ID/TTL/连接状态非法；修正请求。无可用 ID 或非法帧则关闭。 |
| `incompatibleVersion` | 版本不等于 1；升级客户端与 daemon 至兼容版本。 |
| `accessDenied` | 会话/身份校验不通过；拒绝或关闭连接，不提供快照。 |
| `notReady` | 初始或关闭中的写入通道；从状态得知可操作时机。 |
| `unsupported` | 功能、通道、档位、面板候选值或该客户端订阅不支持。 |
| `readOnlyFirmware` | 板型或固件不匹配；保留可用读取。 |
| `deniedWrite` / `writeNotAllowed` | 地址处于禁止并集，或值/地址未授权；不重试。 |
| `validationFailed` | 参数范围、曲线安全约束或配置校验失败，修正参数。 |
| `revisionConflict` | 快照或配置修订号不匹配；读取最新值再决定。 |
| `requestExpired` / `busy` | 排队超时或预算/队列满，未执行；允许在重新取状态后发起新请求。 |
| `conflictDetected` | 外部 EC 写者或状态冲突，等待用户解决且复核。 |
| `timedOut` | HAL 超时，不能从这个错误推断未生效；EC 写入隔离。 |
| `backendUnavailable` | I/O 通道无法服务，读取/探测仅在安全时重试。 |
| `writeFailed` / `verificationFailed` | EC 已结束的写/回读失败，且事务已验证还原；显示错误、读取实际值，不自动重放。非 EC 后端也须发布可确认的实态。 |
| `rollbackFailed` / `recoveryRequired` | 回退不可确认或基线不安全，禁用 EC 写并进入恢复流程。 |
| `internal` | 服务端内部故障，以本地诊断定位。 |

## 5. Windows 权限与单实例

管道不是管理员授权代理。使用 `PIPE_REJECT_REMOTE_CLIENTS`，禁止匿名、网络、低完整性和其他登录会话。读取和写入同样先做身份核验；不使用默认安全描述符，也不以 `Hello.client`、进程名或用户自报 SID 提权。daemon 以当前交互用户的已提升 token 运行，不迁移到 SYSTEM 或别的管理员账号。

使用运行时组装并验证的 SDDL 模板：`UserSid` 是 daemon 与交互用户共同的用户 SID，`ClientLogonSid` 是目标会话未提升 token 的 Logon SID（`SE_GROUP_LOGON_ID`），`ServerLogonSid` 是 daemon token 的 Logon SID。应从受信 token 查询 `TokenUser`、`TokenSessionId`、`TokenGroups` 与完整性级别，不从环境或 IPC 字符串取 SID。优先从有效的本进程/linked token 定位对应身份并核对会话；不可证明普通交互身份时，不开启管道。

```text
O:BAD:P(A;;0x00100083;;;<ClientLogonSid>)(XA;;GA;;;<UserSid>;(Member_of {SID(BA),SID(<ServerLogonSid>)}))S:(ML;;NWNR;;;ME)
```

`O:BA` 让提升方的管理组持有管道对象，普通同用户 token 不因对象所有者权利获得修改 DACL 的权限；`D:P` 关闭继承。普通端精确请求 `FILE_READ_DATA | FILE_WRITE_DATA | FILE_READ_ATTRIBUTES | SYNCHRONIZE`（`0x00100083`）；不授予 `FILE_APPEND_DATA`，因为它和 `FILE_CREATE_PIPE_INSTANCE` 共享 `0x0004`，因此不能授予 `GENERIC_WRITE`。提升方条件 ACE 同时要求本用户、启用的管理员组和 server Logon SID；只有受信提升主体能创建额外实例。`S:(ML;;NWNR;;;ME)` 拒绝低完整性读写。创建后验证实际 owner、DACL 和标签；条件 ACE、访问检查或标签失败则关闭，不回退宽松 ACL。ACL 能区分安全主体，不能区分持同一个提升 token 的进程。

首个 `CreateNamedPipeW` 使用 `FILE_FLAG_FIRST_PIPE_INSTANCE`，失败即退出该端点，不使用已存在的管道。后续实例由已持有首实例的协调器创建，始终保持服务器实例开放。首帧 `hello` 之前不执行任何要求的操作：收到首帧后 `ImpersonateNamedPipeClient`，核验线程 token 的用户 SID、登录 SID、SessionId、完整性级别和非 AppContainer；与 Windows 返回的命名管道客户端会话交叉校验。普通客户端使用 `SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION`，daemon 仅为识别打开线程 token；在取证后用作用域守卫 `RevertToSelf`，失败时不执行任何高权限任务并结束进程。只在身份一致后通过 `GetNamedPipeClientProcessId` 和受保护发行路径的进程镜像核验 UI 类别（限遥测），不依赖客户端明文角色。客户端发送请求前校验服务端 PID、已提升的同用户/会话和受保护的发行镜像；不接受其他端点或无身份回退。

每会话使用有受控 ACL 的 `Local\MSIToolkit-Daemon-v1-<user SID>` mutex，固定管道名在系统上仍只容纳一个宿主。采用“当前已授权交互会话独占”的策略：切换/退出时停止接纳并关闭旧实例；新任务若拿不到端点则保持未就绪，不共享跨会话守护进程。预占端点只会导致不可用，不授予特权。发布验收用同用户中完整性、匹配的提升 token、另一用户/另一会话、低完整性和远程六类端点验证实际连接和创建实例权限。

计划任务在安装时显式授权一次，使用该用户的 `LogonTrigger`、`InteractiveToken`、`RunLevel=Highest` 和“已有实例则忽略”，程序与工作目录指向普通用户不可写的安装路径，启动参数固定。不注册 Windows 服务、不保存账户口令。日常登录无需另弹 UAC 的前提是账户和已注册任务具备该提升权限；标准账号不能靠此获得管理员权限。高权限宿主不得从用户可写目录加载 DLL、映射代码、脚本或任务程序；以后如果开放用户脚本，必须由非提权交互用户组件执行或使用可验证的本用户 Medium token，失败就不运行。`WTSQueryUserToken` 需要 LocalSystem 与 `SeTcbPrivilege`，不能当作本架构的常规回退。

### 5.1 CLI 发行路径与一次性请求

- v1 Windows CLI 只连接固定本地端点；在发送 `hello` 前读取服务端 PID，核对进程令牌已提升、用户 SID 和交互会话均与调用者相同，并验证进程镜像为 `FOLDERID_ProgramFiles\MSI-Toolkit\msilt-daemon.exe`。发行包将同架构客户端与 daemon 配套，安装器确保该目录、程序及其依赖对普通用户不可写；目录或镜像为重解析点时拒绝连接。开发目录里的 daemon 不作为受信发行镜像。
- CLI 等待繁忙管道最长 1 s，连接后握手 5 s、命令应答 20 s；后二者是客户端本地收发期限，与服务端默认 5000 ms 排队 `ttlMs` 分开。CLI 不订阅，不自动重放请求。控制和释放使用 `hello` 返回的快照修订号；断线、等待超时或 `revisionConflict` 后，重新读取实际状态再决定是否发起新请求。
- `--json` 按 §4 的 `response` 结构向 stdout 输出一行 JSON，不含帧头；接收 daemon 错误时也输出应答。CLI 诊断写 stderr，连接或本地错误不伪造 IPC JSON。

## 6. 常驻预算、构建与验收

- **T0 Win32 Pump：** `GetMessageW` 阻塞等待，隐藏顶层窗口接收会话、电源、设备、托盘、热键消息。回调只投递有界通知，无 COM I/O、规则求值或渲染。OSD 按需创建分层窗，用 GDI/GDI+ 软件位图，隐藏释放 DIB；不用 GPU 图形栈、目标进程注入或图形 hook。
- **T1 Coordinator：** 一条原生 OS 线程等待 IOCP，串行处理管道、快照发布、授权、租约和 `on_event`。多个 OS 事件可合并，规则只设置最近的单次容忍延迟定时器，信号到达由回调转投 IOCP；`next_deadline_monotonic_ms()==None` 取消规则定时。Win32 pump 与 IOCP 阻塞等待，无 Tokio、无轮询线程，也不为每连接开一条线程。
- **T2 HalExecutor：** 首次确实需要时启动一条可阻塞的 worker；COM/硬件对象仅在该线程，超时不封死消息泵；外部提供者无法终止时隔离而不扩建 worker 池。可选 ETW 仅在已授权的可见 OSD/规则需要时启用并在结束后关闭，空闲不运行，导入表不含 Slint、Skia、D3D/DXGI 或 WebView2。
- **QoS：** 完成初始化、队列排空、处于后台时启用 `IDLE_PRIORITY_CLASS` 和 `SetProcessInformation(ProcessPowerThrottling)` 的 `EXECUTION_SPEED | IGNORE_TIMER_RESOLUTION`（仅支持的标志）；交互启动、用户写入和受时限的恢复时短暂恢复 `NORMAL_PRIORITY_CLASS`/默认速度策略，完成后回后台。不要申请高精度周期定时器。无 UI 遥测、无定时规则、无待办事务时不设任何轮询定时器。
- **编译：** release 统一 `opt-level="z"`、fat LTO、`codegen-units=1`、`strip="symbols"`、`panic="abort"`、无调试信息/增量，为交付产物优先缩小体积。`release-diagnostics` 同样 abort，保留行表/符号供匹配构建定位；`catch_unwind` 不能隔离发布版 panic。IPC/硬件路径用可恢复 `Result`；关键状态存有界日志/未结标志。UI 热路径的优化级允许在同机渲染帧时间数据支持后单独调整。启用必要的 Win32 feature，按需加载 COM/采样后端，并核查二进制与依赖树。
- **测量：** 设计目标为冷却至无 UI、无 OSD、无启用规则且后台静置后的 daemon 进程 4–10 MiB Working Set；硬指标 Working Set ≤12 MiB、Private ≤10 MiB。Phase 1 采集冷启动、静置 60 s、双 UI 订阅、最后断开、OSD 关闭的 Working Set、PrivateUsage、线程/句柄和 CPU 时间（静置 60 s 增量目标 <60 ms）。另记录 WMI provider 增量和 dGPU 电源/显存，验证非唤醒；把可见 OSD 峰值单独列出。当前只有占位进程，不能把它的占用当作此预算达标。

## 7. 配置、诊断与持久化

| 数据 | Phase 1 存放 | 上限与写入方 |
|---|---|---|
| `app.json` | 交互用户真实 `LocalAppData\MSI-Toolkit` | Core 上限 16 KiB；daemon/受控配置存储落盘。`schemaVersion=1` 必填；`theme=system`，OSD 默认启用、1800 ms（800–5000 ms），自动化默认禁用、`manualOverride={"kind":"untilReleased"}`，可显式设置 1–86400 s 定时覆盖。 |
| `automation.toml` | 同一用户配置目录 | Automation 单独预算 ≤512 KiB、512 规则；建议池化字符串。视为不可信，仅允许经过验证的语义动作，无高权限脚本或任意进程操作。 |
| `crosshair.toml` | 同一用户配置目录 | 独立 schema/上限建议 16 KiB，默认关闭；daemon 独占实际 OSD 窗口。 |
| `configs/ec_maps/<board>.json` | 发行包的受保护安装目录 | Core 对 `guardMap` 限 64 KiB。完整证据封套由 HAL schema 管理，普通用户配置不能覆盖。 |
| `msi_backup.json` / journal | 仅管理员和已授权 daemon 可写的机器状态目录 | 后续停用器专用，备份/日志各自有界与轮转，建议单份 ≤1 MiB。不可将用户可写文件当作还原指令。 |

daemon 以已核对会话用户的 `SHGetKnownFolderPath` 结果构造用户路径；不依赖环境变量或随意使用 SYSTEM profile。文件宿主提供可替换的原子落盘实现：同目录临时文件、flush、同卷原子替换，失败保留上一版，修订号只在成功后提交。用户可写目录把配置解析为不可信数据；受保护状态拒绝重解析点并限定路径、范围和大小。启动遇到损坏/未知版本时保留原文件并保持只读或加载明确默认值，不用新 schema 覆写旧文件。事务日志至少记录操作源、语义目标、映射/固件身份、起止单调时间、回读和恢复状态；限制容量、轮转，不经 IPC 泄漏原始系统对象或用户路径。
