# MSI-Toolkit 硬件协议与 Windows HAL

适用基线：Vector 17 HX AI A2XWJG，`MS-17S3 REV:1.0`，BIOS `E17S3IMS.112`，EC `17S3EMS1.107`，WMI `2.8`。EC 只走 Windows 自带的 ACPI-WMI 映射器 `wmiacpi.sys` 和现有 MSI MOF；不分发 MSI DLL，不使用端口 I/O、第三方内核驱动或其他 EC 旁路。

| 证据 | 能证明什么 |
|---|---|
| **本机 MOF** | `Get-CimClass` 读到的类、签名、方法 ID 和包大小；方法声明本身不表示固件有实现。 |
| **本机 AML** | 从 `GetSystemFirmwareTable('ACPI', 'DSDT')` 取得 DSDT，在内存中静态核对固件分支、偏移和副作用；未求值 AML。DSDT SHA-256：`d85ba1cf4eef70adcc2e98b7bf413670e90f39f815140c9c5f06c58f839d5a64`。 |
| **本机 OEM** | 官软静态 PE/IL、历史日志、只读注册表配置。日志可印证输出与状态，但缓存值不代表当前硬件状态。 |
| **本机 API** | 本轮只读枚举核实的 SMBIOS、HID 描述符、亮度实例及 ICM 文件。当前非提权会话能读 MOF，枚举 `MSI_ACPI` 实例返回 `AccessDenied`；Rust COM 的实例路径和运行时返回仍需 Phase 0 实测。 |
| **公开参考** | 跨型号的协议事实；只用于交叉核对或候选设计。所有实现独立编写。 |

## 1. ACPI-WMI 字节协议

### 1.1 命名空间、方法 ID 和包类型

`root\WMI:MSI_ACPI` 使用 `WmiProv`，GUID `{ABBC0F6E-8EA1-11D1-00A0-C90629100000}`。本机 DSDT `_WDG` 中该 GUID 的对象 ID 为 `AM`，对应 `WMAM(Arg0=实例索引, Arg1=方法 ID, Arg2=输入)`；该方法定义在 DSDT 偏移 `0x89BA6`。本机 `WMAM` 分派 ID 1–29，未匹配时返回清零的 32 字节缓冲。MOF 声明的 32、33 和 `_64` 方法不能据此当作已支持。COM 以方法**名称**调用；官软 .NET `Methods` 枚举是库内索引，不能当作 WMI 方法 ID。

| 包类 | `Bytes` 类型与精确范围 | 用途 |
|---|---|---|
| `Package` | 一维 `uint8[16]`，`0..15` | `GetPackage`（1，出参）和 `SetPackage`（2，入参）的嵌入对象。 |
| `Package_32` | 一维 `uint8[32]`，`0..31` | 本节目标八个方法均使用此类。 |
| `Package_64` | 一维 `uint8[80]`，`0..79` | 本机 MOF 的 `Get/Set_BIOS_64`（69/70）、`Get/Set_SMBUS_64`（71/72）、`Get/Set_Thermal_64`（79/80）。官软 `_64` 发送端用 `Bytes[0]` 作选择器，`Bytes[1..80]` 放方法数据；本机没有相应 `WMAM` 分支，不定义本机返回语义，也不启用。 |

下文 `b[n]` 为原始 `Data.Bytes` 的零基索引，切片如 `b[2..30]` 采用 Rust 半开区间。入参未指定字节全部置 `0x00`。对已验证的 `Package_32` 方法，`b[0] == 0x01` 才接受，有效载荷从 `b[1]` 开始；`0x00` 为拒绝或未分派，任何其他值保留给诊断并拒绝解析。状态是**整字节**，无已知 checksum 或成功位掩码。`HRESULT`、可能存在的 WMI `ReturnValue` 和 `b[0]` 分层记录。CIM 在本机把这些方法的返回类型报告为 `Boolean`，固件成功与否仍以 `Data.Bytes` 的有效长度和状态字节为准。官软包装类型 `InvokeResults.Data[0]` 已经去掉状态字节，相当于原始 `b[1]`。

### 1.2 方法与封包

| 方法 | ID / 方向 | 请求 `b` | 响应 `b` / 证据 |
|---|---|---|---|
| `Get_EC` | 3 / 出参 | COM 的 `pInParams = NULL` | 本机 AML：`b[0]=1`；`b[1]` 为 EC RAM 接口标志，位 `0–3` minor、`4–5` major、`6` page-changed 标志、`7` Tiger Lake 标志；`b[2..30]` 为 28 字节版本/构建日期，`b[30..32]` 为保留。日志记录 `17S3EMS1.1071013202514:29:49`，仅提取经过格式校验的 `17S3EMS1.107` 用于身份匹配。 |
| `Get_WMI` | 29 / 出参 | `NULL` | 本机 AML 与 OEM `gWMI()=1,2,8` 一致：`b[0..3] = 01 02 08`，major/minor 是两个无符号字节。 |
| `Get_EC2` | 32 / 出参 | `NULL` | 本机 MOF 和官软 IL 能证明出参签名；`WMAM` 无此分支，不能假定响应沿用 `Get_EC`。默认不可用；识别只用 `Get_EC`。 |
| `Get_Data` | 27 / 入出参 | `b[0] = ec_offset` | 本机 AML 只读取该地址：`b[0]=1`、`b[1]=value`。`b[1..32]` 在请求中没有地址/长度扩展语义。可用 OEM 日志 `gData(E8)=1,0` 等做回归对照。 |
| `Set_Data` | 28 / 入出参 | `b[0] = ec_offset`，`b[1] = value` | 本机 AML 仅消费这两个字节，调用 `WMWT` 后置 `b[0]=1`；成功为固件确认，不表示写值已保持，也没有有效值回显。本轮及 Phase 0 禁用。 |
| `Get_Fan` | 17 / 入出参 | `b[0] = 00 / 01 / 02` | 三个选择器均由本机 AML 与历史日志印证；逐字节布局见下表。 |
| `Get_Thermal` | 15 / 入出参 | `b[0] = 00 / 01 / 02 / 03` | 四个选择器由本机 AML 证实；`03` 有 OEM 历史日志印证。其余选择器的具体字段语义待证，只作诊断。 |
| `Get_Range` | 33 / 入出参 | 官软客户端布局：`b[0] = selector`、`b[1] = start`、`b[2] = count` | 官方参数合并逻辑能证明地址和计数的位置，但当前 `WMAM` 无 ID 33 分支。选择器数值和响应数据位置不能从本机证实。默认不可用，不扫描选择器；对已知地址逐个 `Get_Data`。 |

`Get_Data` / `Set_Data` 的 `b[0]` **就是 EC 地址**，没有独立子功能号；把地址移到 `b[1]` 会实际访问 `b[0]` 指向的寄存器。多字节的默认实现是对映射中显式允许读取的地址逐项读；后续写入按事务逐项调用 `Set_Data`，不把多个值塞到一包，也不启用 `Set_Range`、`Set_Fan` 等块写替代路径。本机 `Set_AP` 的固定块会写入 `0xD1` 等其他地址，亦不能替代经守卫的单字节场景写入。

`Get_Range` 的 OEM 客户端将 `count=0` 改成 `1`，并在超过地址空间末端时裁剪。HAL 即便为其他已验证固件实现此方法，也应拒绝零长度、地址溢出和超出包有效容量的请求，并单独验证选择器、状态和响应布局。`Get_EC`/`Get_WMI`/`Get_EC2` 按 Windows 出参签名传空指针；[Linux 文档][linux-wmi] 所说的统一 32 字节输入是 Linux 直接求值 AML 时的传输约束，不应因此给 Windows 出参方法伪造输入对象。

| 方法及 `b[0]` | 响应中 `b[0]` 后的有效前缀（顺序对应 EC 地址） | 解码与用途 |
|---|---|---|
| `Get_Fan(0)` | `b[1..9]` ← `C8 C9 CA CB CC CD CE CF` | 四对大端 `u16` 周期。前两对按 OEM 用法对应 CPU/GPU；已确认通道 `period=0` 为 0 RPM，否则整数 `480000 / period`。额外通道保持缺失，直到证实存在。日志 `1,0,7B,0,AB,0,0,0,0` 对应 3902/2807 RPM。 |
| `Get_Fan(1)` / `(2)` | `b[1..9]` ← `71..78` / `89..90`（十六进制地址） | `b[1]` 是 EC 报告的占比，`b[2..8]` 是六个 UI 占比，`b[8]` 是额外固件槽。占比不是 RPM。 |
| `Get_Thermal(0)` | `b[1..3]` ← `33 34` | 原样诊断，不解码为温度或功耗。 |
| `Get_Thermal(1)` / `(2)` | `b[1..8]` ← `79..7F` / `91..97` | 辅助热表，语义待证；不当作 UI 六点，不开放写入。 |
| `Get_Thermal(3)` | `b[1..8]` ← `98..9E` | `b[1] & 0x80` 为 Cooler Boost 候选状态；日志首数据字节 `02` 与 bit 7 清零相容。其余数据只诊断。 |
| `Get_Temperature(0)`，ID 13 | `b[1..4]` ← `68 80 9F` | `b[1]` CPU、`b[2]` GPU 为无符号摄氏度，`b[3]` 只诊断。有效区间先取 `1..=125`；0、越界或传感器停用返回缺失，待 Phase 0 校准。 |
| `Get_Temperature(1)` / `(2)` | `b[1..9]` ← `69 6F 70 6A 6B 6C 6D 6E` / `81 87 88 82 83 84 85 86` | 六点按 `b[1], b[4], b[5], b[6], b[7], b[8]` 重排；保留 `b[2..4]`。官软历史日志与此布局吻合。 |
| `Get_Device(1)`，ID 19 | `b[1..7]` ← `2E 2F 2C 2D 2A 2B` | 本机 `b[2]=5B`，OEM `CheckGPU_Switch_IsSupport` 测的是该字节 bit 6。`b[1]` bit 1 为官软摄像头状态读法。 |
| `Get_AP(0)` / `(1)`，ID 25 | `b[1..7]` ← `D0 D1 D2 D3 D7 DB` / `b[1..4]` ← `D4 D5 D6` | 只读交叉验证 MUX 指示、Shift/Fan、键盘亮度和充电限制原始值。 |
| `Get_Power(0)`，ID 21 | `b[1..4]` ← `30 BE BF` | 见下方只读样例。未验证三字节与 AC/功耗的换算关系。 |

`Get_Power` 的只读测试向已选实例传 `Package_32.Bytes = [0x00; 32]`，预期 `Data.Bytes[0] == 0x01`；将 `Data.Bytes[1..4]` 仅和 `Get_Data(0x30/0xBE/0xBF)` 对照。`MSI_Power` 是独立动态类，GUID `{2D3CBA6C-1C9C-7F41-B54C-F5D5D580D482}`，字段为 `InstanceName: String`、`Active: Boolean`、`Power: UInt8`，不是 `Get_Power` 的入出参类。AC 变化以 Windows 电源通知为准。

### 1.3 Rust COM 调用与资源归属

1. 专用系统线程用 `CoInitializeEx(None, COINIT_MULTITHREADED)`；`S_OK/S_FALSE` 都配对 `CoUninitialize`。遇到 `RPC_E_CHANGED_MODE` 视为线程模型配置错误。若宿主的窗口或回调线程因其他需求是 STA，留在该线程消化消息，`IWbemServices` 不在两者间共享。进程启动时、建立首个代理前调用一次 `CoInitializeSecurity`；`RPC_E_TOO_LATE` 表示已初始化，须用下面的代理设置检查有效安全配置。
2. `CoCreateInstance(WbemLocator, CLSCTX_INPROC_SERVER)` → `IWbemLocator::ConnectServer(BSTR("ROOT\\WMI"), …)` → `CoSetProxyBlanket(RPC_C_AUTHN_WINNT, RPC_C_AUTHZ_NONE, RPC_C_AUTHN_LEVEL_CALL 或更强, RPC_C_IMP_LEVEL_IMPERSONATE, EOAC_NONE)`。仅连接本机，以 daemon 的令牌建立连接；枚举/方法拒绝时回报 `AccessDenied`，不触发 UAC、尝试其它身份或更改 ACL。Rust `windows` 绑定按用到的接口开启 `Win32_System_Wmi`、`Win32_System_Com`、`Win32_System_Ole`、`Win32_System_Variant` 和相关基础模块；COM 句柄与自动化数组用对应 RAII 封装。
3. 对连接做有界 `ExecQuery("WQL", "SELECT * FROM MSI_ACPI", …)`，用 `IEnumWbemClassObject::Next` 检查 `Active`、`InstanceName`、`__RELPATH`；优先采用唯一活动 ACPI 实例。官软引用 `MSI_ACPI.InstanceName='ACPI\PNP0C14\0_0'`，仅用于对照；向 COM 传 WMI 返回的 `__RELPATH`，不自行拼接转义路径。连接代际变化时重取实例。IPC 不接受原始路径、方法名、选择器或 EC 地址作为生产写请求。
4. `IWbemServices::GetObject` 取 `MSI_ACPI` 类和所需 `Package_32`/`Package_64` 类。`IWbemClassObject::GetMethod` 获取入出参数类；对入参方法以 `SpawnInstance` 分别实例化入参对象与包对象。包对象 `Put("Bytes", …)` 的 `VARIANT` 类型为 `VT_ARRAY | VT_UI1`，对应一维、零下界 `SAFEARRAY`，元素数恰为 32 或 80；用 `SafeArrayCreateVector`、`SafeArrayAccessData`/`SafeArrayUnaccessData` 填好。入参对象 `Put("Data", …)` 为 `VT_UNKNOWN`，持有包对象的 `IUnknown` 引用；已有属性的 `Put` CIM 类型参数用 `0`。出参方法不创建包，也不把客户端对象 `PutInstance` 到 WMI 仓库。
5. 调用 `ExecMethod(instance_path, method_name, flags, context, in_params, …)` 或同签名的 `ExecMethodAsync`。从 `IWbemClassObject::Get("Data")` 取嵌入对象，通过 `IUnknown::QueryInterface` 取 `IWbemClassObject`，再 `Get("Bytes")`。校验 `CIM_OBJECT` / `CIM_UINT8 | CIM_FLAG_ARRAY`、`VARTYPE`、对象非空、维数为 1、上下界及长度；只复制已知布局的数据。若提供者返回未预期类型/长度，应失败关闭，诊断记录 HRESULT、方向和包长。释放 `VARIANT`、数组锁和各 COM 引用后才结束线程；避免 `VariantClear` 和显式 `SafeArrayDestroy` 重复释放。

### 1.4 串行执行与不确定写事务

- daemon 交付异步接口：IPC/消息泵/自动化只提交有界请求并等回执；ACPI 专用线程持有 COM 对象，一次最多一个未结案的固件调用。生产优先验证 `ExecMethodAsync` + `IWbemObjectSink`；回调仅复制结果、投递完成信号。`Indicate` 给数据，终结以调用完成通知为准。也可在实测不适合回调时用 `ExecMethod(WBEM_FLAG_RETURN_IMMEDIATELY)` + `IWbemCallResult` 有界等待，保持相同事务语义；同步 `ExecMethod` 只可在专用探测线程验证。`ConnectServer`、提供者连接和取消也可能阻塞，看门狗在执行线程外。显示、HID、NVML、电池各有隔离的阻塞队列；同一资源的事务不并发，不拿跨资源的锁等待 I/O。
- 读默认预算 300 ms/次，写默认 1,200 ms/次，完整曲线事务单列 deadline；这些是配置起点，Phase 0 只量只读 P50/P95，后续授权测试测写入。排队超时是 `Busy/DeadlineExceeded`（尚未提交）；已提交超时是 `Timeout`（读）或 `Indeterminate`（可能生效的写）。`CancelAsyncCall` 或调用方超时不证明 EC 中止，迟到回调按事务/代际隔离；即使取消报告完成，也要有可信的提供者完成或重置证据才能解除隔离。不终止线程、不另起 EC worker 绕开在途调用，不重启 `WmiAcpi`、WMI 或官软服务。
- 读只在上次调用已结案且为暂时性传输错误时最多重试一次；权限、无实例、协议不匹配、固件拒绝不重试。睡眠、恢复、实例更换或官软竞争变化使快照过期。写前重新检查身份、能力、写令牌、冲突、温度/供电等操作前提；每个事务先读变更域与安全基线。位操作以 `(old & !mask) | (encoded & mask)` 保留其他位。只用 Core 生成的 `AuthorizedEcWrite` 发送原始字节，驱动层原语不进 IPC。
- 写完逐项回读目标位/值，必要时整体复验和采集未改字段。若失败且最后调用已结案，只有在所有权、身份、快照和热安全仍可信时才按逆序恢复已知完成的写，并再回读。未结案、冲突、回读不确定或回退失败：锁住全部 EC 写入并标 `RecoveryRequired`，不自动重放或用初始全字节覆盖别的写者。回执提供请求 ID、开始/结束单调时间、实际往返数、确认/未确认字段和恢复状态。`exec` 成功但没有业务回读不算 `Applied`。
- 恢复由 daemon 独占执行：旧调用确定已结案或通道有可信重置边界 → 重建连接/代际 → 三次有限的只读探活（间隔约 300 ms）→ 重新校验完整指纹和竞争状态、重读变更域并核对安全基线。三次探活只证明通道可读；原事务结果仍未知时保持只读，并经用户确认后才接受下一条新写请求，不恢复旧队列。

## 2. MS-17S3 EC 映射与写门禁

[`configs/ec_maps/ms-17s3.json`](../configs/ec_maps/ms-17s3.json) 是本机数据源。根 `schema` 为内嵌 JSON Schema Draft 2020-12；验证输入为整个 JSON 文档。根 `registers`、`telemetry`、`fanCurves` 是带出处的研究/读取元数据；`verified:false` 不开放写。根 `guardMap` 是与现有 `msilt_core::EcMap::parse_json` 对齐的 v1 对象，取该对象单独序列化给 Core；整个信封不是 `EcMap` 输入。`guardMap.writeAllowlist=[]`、`guardMap.fanCurves={}` 是本基线的可执行只读策略。`17S3EMS1.1` 是 [msi-ec 的 `CONF_G2_10`][msi-ec] 候选搜索前缀（同族不同 SKU），不是允许写入的匹配范围；晋级需完整 board/BIOS/EC/WMI 和受审协议指纹精确匹配。

| 项目 | 地址和候选编码 | 实现边界 |
|---|---|---|
| ShiftMode | `0xD2`：`C1` Comfort/Balanced、`C2` Eco、`C4` Turbo/Extreme | 本机历史 `C1/C4`；完整场景是复合状态，勿把单一字节当作 `PerformanceMode` 的全量定义。 |
| FanMode | `0xD4`：`0D` Auto、`1D` Silent、`8D` Advanced | 本机历史 `0D/8D`；Silent 场景组合和其他 Advanced 组合待验证。用户曲线是风扇覆盖项，现有证据不支持独立 EC `Custom` 性能档。 |
| SuperBattery | `0xEB & 0x0F`，候选 `0/0x0F` | 保留高位；未经验证的低位组合返回未知，不自动纠正。 |
| Cooler Boost | `0x98` bit 7 | 用 `Get_Thermal(3)` 与单点读交叉验证，保留 bit 0–6。 |
| 充电上限 | `0xD7`，候选 `128 + percent` | 初始 UX 候选 `60/80/100`（`BC/D0/E4`）。`100` 表示充至 100%，`0x80` 的实际含义待只读关联，不能播报成 0%、100% 或关闭。 |
| 摄像头 | `0x2E` bit 1，`0x2F` 为独立 capability/block 字节 | 官软根据 `Get_Device(1)` 的前一字节读 enable；保持 `0x2F` 原值，区分用户关闭、硬件锁和不可用。 |
| Win/Fn 互换 | `0xE8` bit 4，G2 约定反相 | 清零时为逻辑 `true` 的候选极性；需和当前固件键位行为对照。Win 键锁与此字段分开。 |
| 键盘亮度 | `0xD3`，候选 `0x80 + 0..3` | 本机 AML 和 OEM 读取证实关联，`CONF_G2_10` 对此族未提供键盘 EC 地址；级别编码待证。与 HID RGB 通道分离。 |
| 麦克风/扬声器静音 LED | `0x2C/0x2D` bit 1 | 历史 `0x2D` 有 `24/26`；位 / OS 音频状态需关联。真实静音走 OS 音频端点，LED 不是音频控制。 |
| 触控板、WinKey、关机 USB 充电 | 本机 OEM `TouchPadEnable=1`、`WinKey=1`、`USBsup=1`、`USBstatus=0` | 只证明官软 UI 标记，EC 地址和极性留空。 |

`denyWrite` 由 Core 内置不可覆盖项 `0x50/0x51`（PL1/PL2 保护）与机型表 `guardMap.denyWrite` 取并集。将所有未知/保留地址默认排除在允许集合之外；传感器、额外风扇点、事件字节 `0xD8`、MUX 指示 `0xD1` 不可写。表项出现于研究区、块读取响应或已知固件地址不构成写入授权。

| 通道 | 六点温度寄存器 / 出厂 ℃ | 六点占比寄存器 / 出厂 % |
|---|---|---|
| CPU | `0x69–0x6E` / `0,48,56,62,70,78` | `0x72–0x77` / `0,36,42,52,76,84` |
| GPU | `0x81–0x86` / `0,43,48,56,65,72` | `0x8A–0x8F` / `0,36,46,56,80,88` |

两条六点取自 32 位视图 `HKLM\SOFTWARE\WOW6432Node\MSI\MSI Center\Component\Base Module\Scenario` 的 `Default_Temp`、`Default_Fan`，与历史 `gTemperature/gFan` 可交叉核对，和持久的 `User_Fan` 或当前 EC 状态各自独立。曲线验证 `verified:false`：地址顺序已由本机 AML 确认，但还缺授权探测器对当前快照/模式值的验证和后续可回退的写入验收。`0x69–0x97` 不是可整体写入的区间。

曲线开放后只编辑占比：点 0 固定 `0°C / 0%`，其余温度固定为经验证出厂节点，温度严格递增、占比非递减且各点不低于该点出厂值；产品上限 `150%`（字节 `0x96`，不等于转速线性外推）。仅对已确认节点写差量；读取两个通道和模式的基线，保持额外点与当前占比/辅助热表原样。验证过的 5 个可编辑点按中间曲线也满足安全下限/单调性的顺序写，模式切换和整体验证最后完成。一次提交是 HAL 串行逻辑事务，**不是原子固件写**。不安全的当前基线不能作为回退值；普通编辑拒绝，专门修复必须单独定义已验证的安全目标、所有中间态和回退，不能把 clamp 后的值当作原始快照。

## 3. 24 区键盘 HID

本机以 `CreateFileW(desiredAccess=0)` 查询到唯一匹配集合：`1462:1603` / revision `0500` / `MysticLight MS-1603` / Usage Page `0x00FF` / Usage `0x0001`。`HidP_GetCaps` 的 Input、Output、Feature 都是 64 字节，含开头的 Report ID。`HidP_GetValueCaps/GetButtonCaps` 证实：Feature IDs `01,02,04,05,06,07,08`，Output IDs `01,03,04,08`，Input IDs `03,05,06,07,08`。Mystic Light OEM 日志 `Log\MysticLight_Test\MysticLight.txt` 同时出现该 VID/PID 和 `Check 24 Zone`；`LEDKeeper2.exe` 有 `Device_24Zone` 等符号。本机报告 ID 2 是 Feature，**不能改用 `WriteFile` 发 Output**。

### 3.1 Feature `0x02` 协议候选

[LightShow][lightshow] 对相同 VID/PID 的实机观察确认四字节选择掩码；[msi-kb-ripple][ripple] 用同 VID/PID 的控制器发送四字节小端 zone mask 和双色端点静态帧。公开工具对“粗分区”还是“逐区”及若干格式有差异，[另一份 MS-1603 参考][mystic] 使用额外前置报文和模式变体，不混用。下表是相互吻合的两种报文的字节布局，不是本机 A2XWJG 已获准的发送模板。

| 字节（含 Report ID） | 选择区 | 模式/颜色 |
|---|---|---|
| `0` | `0x02` | `0x02` |
| `1` | `0x01` | `0x02` |
| `2` | 掩码最低字节 | mode：`0` off、`1` static、`2` breathing、`3` cycle、`4` wave（后四者按独立能力验证） |
| `3..=4` | 掩码字节 1、2 | 16 位小端周期，单位 `1/100 s` |
| `5` / `6` | 掩码最高字节 / `0` | 都是 `0` |
| `7` / `8` / `9` | 都是 `0` | 公开共同模板为 `0x0F / 0x01 / direction`，方向限 `0` 或 `1`；本机须验证。 |
| `10..=49` | `0` | 最多十组 `[cycle_percent, R, G, B]`，时间 `0..=100` 非递减。静态候选取起末点同色；未使用槽清零。 |
| `50..=63` | `0` | `0` |

选择掩码 `b[2..6] = u32::to_le_bytes(mask)`，`b[6..64] = 0`。逐区候选只用 bit `0..23`（`z=0` → `01 00 00 00`，`z=23` → `00 00 80 00`）；bit `24..31` 不发送，默认不照搬公开参考的 `FF FF FF FF`。`6×4` 是 UI 逻辑布局提案，实际行列、bit/键位映射、命令间隔和固件版本差异需逐区结果确认后才开放。效果亮度可先软件缩放 RGB，无已证独立硬件亮度字段。设备内部保持选区，两个 Feature 包必须组成一个串行事务；任何断链/超时都使选区缓存失效。`HidD_SetFeature` 返回成功仅表示传输成功；没有验证过的状态回读时不能宣称“灯效已观测”，可分别返回 `Submitted` 和 `Observed/Unavailable`。

### 3.2 Windows API 与双写防护

1. `HidD_GetHidGuid` → `SetupDiGetClassDevsW(DIGCF_PRESENT | DIGCF_DEVICEINTERFACE)` → `SetupDiEnumDeviceInterfaces` → 两次 `SetupDiGetDeviceInterfaceDetailW`（由 SDK 布局填写 `cbSize`，使用 UTF-16 路径）→ `CreateFileW(OPEN_EXISTING)`。读取描述符只需 `dwDesiredAccess=0` 和共享读写；`HidD_GetAttributes`、`HidD_GetProductString`、`HidD_GetPreparsedData`/`HidP_GetCaps`/`HidP_GetValueCaps`/`HidP_GetButtonCaps` 验证唯一设备与描述符，再 `HidD_FreePreparsedData`、`SetupDiDestroyDeviceInfoList`、`CloseHandle`。不试开 HID 键盘输入集合，不输出实例路径或序列号到共享日志。
2. Phase 2 才打开匹配集合的写句柄，`HidD_SetFeature(handle, buffer, 64)` 发送固定 report ID；在阻塞隔离队列上执行。`WriteFile` 只适用于未来**独立验证**的 Output report ID；用 `FILE_FLAG_OVERLAPPED`、事件/IOCP、`GetOverlappedResult` 对实际长度，超时 `CancelIoEx` 后等终结再释放缓冲或重开。不能把任意 Output ID、固件更新命令、未知 Feature 变体暴露给 UI/IPC，也不把 `HidD_GetFeature` 的任意 ID 当成无副作用探测。
3. 本轮读到了 `LEDKeeper2`、`LightKeeperService`、`Mystic_Light_Service` 正在运行。写前检查官软进程、服务状态和设备共享结果，按需监听设备通知；命名互斥只防本工具多开，不能证明官软退出。检测到官软拥有设备、权限不明、共享冲突或在途超时，报告 `Conflict/Unavailable`，保持读描述符能力。任何停用由另行授权的停用器负责，HAL 不自行终止进程或服务。连续灯效默认禁用；设备内已验证的效果可减少主机帧推送。

## 4. GPU MUX 与 NVIDIA 后端

### 4.1 三态 MUX 的证据边界

| 线索 | 本机结论 |
|---|---|
| `gpuControl.exe` | .NET 静态方法/导入元数据有 `SetGPUOC`、字符串有 `GPUOverClocks`；支持把它识别为超频组件，不能把它当成 MUX 写入口。 |
| `UEFIVaribleDll.dll` | 静态 PE 导出 `GetUEFIVariableEx`、`SetUEFIVariableEx`；导入 `Get/SetFirmwareEnvironmentVariableExA` 及 `AdjustTokenPrivileges`，含 `SeSystemEnvironmentPrivilege`。它是通用固件变量封装，不证明实际变量结构。 |
| `OmApSvcBroker.exe` / `API_NB_Base Module.dll` | General Settings 的 MUX 支持由 `Get_Device(1).b[2] & 0x40` 判定。Broker 新式路径由 `watcherGPU_EventArrived` 转到 `Set_BIOS_Flag_Of_New_GPU_Switch`，使用变量 `MsiDCVarData`（供应商 GUID `{DD96BAAF-145E-4F56-B1CF-193256298E99}`）。另一个 `Set_BIOS_Flag_Of_DGPU_Switch` 位于 Battery Boost 回调，操作变量 `byte[7]` bit 2，属于独立控制链。 |
| EC `0xD1` | 本机 AML 的 `Get_AP(0).b[2]`、两类固件事件详情读取它；相邻固定块写方法也能触及它，证明有寄存器访问路径，不证明它是三态切换的独立入口。保持只读。 |
| 当前拓扑 | PnP 当前枚举到 RTX 5090 Laptop、另有虚拟显示适配器；未枚举到活动 Intel GPU。仅说明当前可见设备，不区分已确认直连、重启待生效和运行时电源状态；CCD 与 iGPU/dGPU 实际路由仍需对照。 |

#### 请求位、状态位与能力位

偏移按 `GetFirmwareEnvironmentVariableExW` 返回的变量负载零基计数，`byte[5]` 是第六字节。Broker 静态代码读写的三个字段独立：

| 字段 | 提取方式 | 用途 |
|---|---|---|
| 请求编号 `requestedRaw` | `byte[5] & 0x03` | OEM 三态链提交的目标；`0..=2` 有候选语义，`3` 按保留/未知记录。已写入目标不证明调度已被固件接受。 |
| 状态编号 `statusRaw` | `(byte[5] & 0x0C) >> 2` | Broker 读取的固件状态编号；需要和同一次开机的内屏路由对照才能发布为 `current`。`3` 按未知记录。 |
| 能力 | `byte[5] & 0x10/0x20/0x40` 分别测试 | 新式切换、UMA、独显。与 EC `0x2F` bit 6 来自不同载体；均不能代替模式编号。bit 7 不解释。 |

本机注册表中 `GPU_Switch_Support`、`GPUswitchSP`、`GPUswitchUMA`、`GPUswitchDiscrete` 为 `1`；`GPUswitchCH=6` 是 Broker 初始化哨兵，`GPUswitchST=1` 是官软缓存。结合已见的 NVIDIA 实体 GPU，`1=Discrete` 是强候选；`0=MSHybrid`、`2=Integrated/UMA` 仍待对照。三态分别看混合显卡拓扑、dGPU 驱动内屏、iGPU 独立驱动内屏；UMA 下 dGPU 是否仍可枚举取决于平台。EC `0xD1` 保留原始指示，独立于这两个变量字段。

#### 只读核验与发布

1. 用 `GetFirmwareType` 确认 UEFI。仅在用户另行授权且会话已有 `SeSystemEnvironmentPrivilege` 时只读变量，记录实际长度、属性、Win32 错误及掩码后的编号。至少读到 6 字节才能解包；OEM 的 4096 字节缓冲容量不是实际变量长度。不输出完整变量或实例路径。
2. 在同一次开机和稳定拓扑中记录 `Get_AP(0).b[2]` 与独立 `Get_Data(0xD1)`、`Get_Device(1)` 的支持位、OEM 缓存、PnP GPU 及 CCD 内屏目标的适配器。以 CCD 确认面板由哪块 GPU 驱动；GPU 在 PnP 中存在不能证明它驱动内屏。其他模式仅用已有历史或用户独立安排的开机状态核对。
3. 模式映射、状态位和路由一致时，才发布 `current`。请求位与状态位相等仅说明两个编号相同；请求位不同时，保留两个原始编号，仅在已验证该平台确有跨重启预约且能关联已接受的请求时发布 `pending`。跨正常重启后重取变量与路由验证生效；变量不可读、编号未知或证据矛盾，相关语义保持未知。

Phase 0 `msilt-probe` 默认只报告 CCD 内屏及适配器厂商；`--acpi` 可补 EC 支持位和指示字节，并记录 `Get_AP(0)` 与独立 `Get_Data`。本实现不读取 UEFI 变量，`HardwareState.gpu_mux` 保持 `None`。

Phase 2 写门禁：三态与精确变量长度、属性、字段范围、校验及回读都获验证；安全启动/磁盘加密恢复与可用显示路径有已确认的恢复方案；仅 AC 供电、当前交互会话有效、无冲突写者时允许。向用户显示当前、请求、待重启及可能不可用的显示输出，明确确认后只预约下次正常重启，不强制重启。若最终验证的是 UEFI 路径，才用 `SetFirmwareEnvironmentVariableExW` 更新**已知字段**，遵循已验证的属性与保留字节处理，验证并记录请求字节；不从读取成功自动推导写权限，不写 `0xD1` 作为降级。重启后以新 CCD/PnP 状态与固件读数判定 `current`；取消待生效预约须走同一已验证路径。失败或不可判断时锁住入口并显示恢复说明，不循环写入。

### 4.2 NVML/NVAPI 与偏移

- 用 `GetSystemDirectoryW` + `LoadLibraryExW` 仅加载可信绝对路径（系统目录，或明确允许且验证签名的 NVIDIA 安装目录），`GetProcAddress` 检查逐项导出/版本，RAII 持有模块直到会话/调用结束。已静态确认本机 System32 的 `nvml.dll` 导出 `nvmlInitWithFlags`、`nvmlShutdown`、`nvmlDeviceGet/SetClockOffsets`；`nvapi64.dll` 导出 `nvapi_QueryInterface`。这证明 ABI 入口存在，未证明笔记本许可和驱动数值范围。
- NVML：惰性 `nvmlInitWithFlags(NVML_INIT_FLAG_NO_ATTACH)`（支持时，否则仅显式激活后 `nvmlInit_v2`），`nvmlDeviceGetCount_v2`、`nvmlDeviceGetHandleByPciBusId_v2` / `nvmlDeviceGetHandleByUUID` / `nvmlDeviceGetHandleByIndex_v2`，用 PCI 总线标识/UUID 对齐目标，索引不跨启动保存。`nvmlDeviceGetTemperature(GPU)` → ℃；`nvmlDeviceGetUtilizationRates` → GPU/显存控制器 %；`nvmlDeviceGetMemoryInfo_v2`（或 v1）→ 字节；`nvmlDeviceGetPowerUsage` → mW；`nvmlDeviceGetClockInfo` → MHz；可选 `nvmlDeviceGetPerformanceState`。客户端最后一项租约关闭时 `nvmlShutdown` 并释放句柄。
- NVAPI：用公开 [NVIDIA SDK 接口表][nvapi] 对 `nvapi_QueryInterface` 解析 `NvAPI_Initialize`、`NvAPI_Unload`、`NvAPI_EnumPhysicalGPUs`、`NvAPI_GPU_GetPCIIdentifiers`、`NvAPI_GPU_GetAllClockFrequencies`、`NvAPI_GPU_GetPstates20`。结构用对应 `*_VER` 和正确 C ABI；不能把 GPU 句柄当作 NVML 的句柄。没有已证的公开 `NvAPI_GPU_SetPstates20` 写 ABI；本机首选公开 NVML 写接口，不用未经验证的私有 QueryInterface ID。
- 偏移用 [`nvmlDeviceGetClockOffsets` / `nvmlDeviceSetClockOffsets`][nvml]，结构 `nvmlClockOffset_v1_t`，版本用 SDK 的 `nvmlClockOffset_v1`，域 `NVML_CLOCK_GRAPHICS` 与 `NVML_CLOCK_MEM`，`pstate` 为已核实的驱动态，提交 `clockOffsetMHz`。分别取 `minClockOffsetMHz/maxClockOffsetMHz` 与 `0..=200 MHz` 的交集；拒绝不支持、无权限以及范围外值，不能无声裁剪，也不把显存有效数据率按时钟倍率自行换算。已证兼容旧驱动时才考虑 GPC/Mem `Get*VfOffset` + `Get*MinMaxVfOffset` + `Set*VfOffset`。开启前检查 AC、驱动可用性、冲突超频工具；快照旧值、逐域验证，确定前次完成才能回退。`0` 为重置偏移；不改电压、功耗墙、P-State 强锁，也不自动将旧偏移应用到新驱动/新适配器。
- `NO_ATTACH` 只约束初始化，取句柄/枚举/查询仍可能把休眠 dGPU 唤醒。无订阅时不加载两个库；OSD 默认显示 EC 温度与风扇，不激活 GPU 订阅。状态分为 `Inactive`、`ActiveWakeRisk`、经系统电源轨迹确认的 `ObservedWake`、`Unknown`；不靠 NVML 读数本身证明“没有唤醒”。Phase 0 只有单独允许唤醒的只读探测才调用 NVML/NVAPI 初始化和设备接口，记录驱动状态与前后功耗观察。

## 5. 内屏刷新率、亮度与色彩

| 能力 | API 与生效判据 |
|---|---|
| 内屏识别 | `GetDisplayConfigBufferSizes` → `QueryDisplayConfig`，组合 `QDC_ONLY_ACTIVE_PATHS`、`QDC_VIRTUAL_MODE_AWARE`、`QDC_VIRTUAL_REFRESH_RATE_AWARE`，遇 `ERROR_INSUFFICIENT_BUFFER` 有界重枚举。`DisplayConfigGetDeviceInfo(DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME/GET_SOURCE_NAME)` 对齐 `monitorDevicePath`、EDID `AUO84A6`、内置输出技术、adapter LUID/target ID/source ID 和 `viewGdiDeviceName`。外接为主屏、克隆、MUX 改路或热插拔后必须重选；克隆/歧义下拒绝改变。 |
| `60/240 Hz` | 本机目标模式 `2560×1600`；用 `EnumDisplaySettingsExW` 枚举当前 GDI 源的合法 `DEVMODEW`（包括分辨率、位深、扫描模式和频率），以 `CDS_TEST` 验证目标模式，再 `ChangeDisplaySettingsExW` 应用到已绑定的内屏源；整拓扑 CCD 路径使用 `SetDisplayConfig`，组合 `SDC_VALIDATE`、`SDC_USE_SUPPLIED_DISPLAY_CONFIG`、`SDC_VIRTUAL_MODE_AWARE`、`SDC_VIRTUAL_REFRESH_RATE_AWARE`，只有候选路径和对应 timing 都可用才以 `SDC_APPLY` 替换 `SDC_VALIDATE`，不改其余拓扑。显示切换适用于可访问控制台/交互会话的进程，不从 Session 0 强行应用。实际值重新 `QueryDisplayConfig`，返回目标路径 `refreshRate` 有理数，必要时区分 DRR 虚拟频率与 `DISPLAYCONFIG_TARGET_MODE.targetVideoSignalInfo.vSyncFreq` 的物理刷新。允许硬件报告的等效小数模式，保持原有分辨率/拓扑；与请求不符时返回观测值，已预置超时恢复/黑屏回退路径才允许提交。 |
| 面板亮度 | `root\WMI:WmiMonitorBrightness` 读取同一活动内屏的 `CurrentBrightness: uint8`、`Levels: uint32`、`Level: uint8[]`；本机发现活动实例。用相同 `InstanceName` 匹配 `WmiMonitorBrightnessMethods.WmiSetBrightness(Timeout: uint32, Brightness: uint8) -> uint32`，通常 `Timeout=0`，仅接受面板报告的有效档位。`ReturnValue=0` 后回读实际亮度/档位；`WmiMonitorBrightnessEvent` 可作为经验证的失效通知。本功能不转发给 EC 或外屏。 |

取色彩配置使用 `C:\ProgramData\Portrait Displays\TrueColor\` 下现有文件。本机读取到 14 个 ICC/ICM 显示器 RGB 配置，均有 `acsp` 签名和 `MHC2` 标签、未见 `vcgt`：`TrueColor_Adobe_{5000,6500,9300}.icm`、`TrueColor_DCIP3_6300.icm`、`TrueColor_DisplayP3_6500.icm`、`TrueColor_Native_6500.icm`、`TrueColor_native.icm`、`TrueColor_Rec709_6500.icm`、`TrueColor_sRGB_{4400,4700,5000,5600,6500,9300}.icm`。`Native_6500` 和 `native` 是不同文件；缓存/安装目录与 ProgramData 库的数量可以不同。枚举实际文件，解析大小/ICC 类型及标签边界，禁止接受任意 IPC 路径。

- **L1：Windows 色彩配置关联。** `GetColorDirectoryW`、`InstallColorProfileW`（只在另行授权且源文件尚未安装时）；用 `ColorProfileGetDisplayUserScope`、`ColorProfileGetDisplayList`、`ColorProfileGetDisplayDefault` 读目标显示器关联，返回字符串按 API 要求 `LocalFree`。Win11 SDK 支持 `ColorProfileAddDisplayAssociation` 和 `ColorProfileSetDisplayDefaultAssociation`，使用签名中的目标适配器 LUID 和对应 source ID，以及 `CPT_ICC/CPST_NONE`；以 `ColorProfileGetDisplayDefault` 验收。兼容后端用 `WcsGetUsePerUserProfiles`、`WcsGetDefaultColorProfileSize`/`WcsGetDefaultColorProfile`、`WcsAssociateColorProfileWithDevice`、`WcsSetDefaultColorProfile(scope, deviceName, CPT_ICC, CPST_NONE, 0, profileName)`；按 SDK 所需显示设备名指定设备，禁止传 `NULL` 落到全局默认。使用交互用户作用域，不修改系统默认；快照原关联与默认名，仅回退本工具新增关联。Phase 0 只枚举与读取关联。
- **L2：实际显示管线变化。** `MHC2` 表明这些文件包含 Windows 可识别的硬件色彩变换数据；具体能否应用取决于面板、适配器、驱动、HDR/Advanced Color 和配置关联。用 `DisplayConfigGetDeviceInfo(GET_ADVANCED_COLOR_INFO_2 或 GET_ADVANCED_COLOR_INFO)` 和 `ColorProfileGetDeviceCapabilities`（MHC2）做只读能力探测，随后由已授权切换及外部测量确认效果。L1 的回读只确认配置文件关联，不作为色域/LUT 已生效的证据；`IntelCSC.exe` 无已验证依赖，不执行它，不将 L2 纳入初始自动化。

## 6. 电池、事件与采集节奏

### 6.1 电池元数据和状态

1. `SetupDiGetClassDevsW(GUID_DEVINTERFACE_BATTERY, DIGCF_PRESENT | DIGCF_DEVICEINTERFACE)` → `SetupDiEnumDeviceInterfaces` → `SetupDiGetDeviceInterfaceDetailW` → `CreateFileW(OPEN_EXISTING, FILE_SHARE_READ | FILE_SHARE_WRITE)`。Windows 电池查询 IOCTL 的访问位为 `FILE_READ_ACCESS`，优先申请 `GENERIC_READ`。微软枚举示例用读写访问以兼容堆栈；严格 Phase 0 若读权限不足就报告原因，不扩大权限或发 Set IOCTL。
2. `DeviceIoControl(IOCTL_BATTERY_QUERY_TAG, ULONG wait_ms=0, …, ULONG tag)`；必须返回完整非零 tag。`DeviceIoControl(IOCTL_BATTERY_QUERY_INFORMATION, &BATTERY_QUERY_INFORMATION{BatteryTag, InformationLevel, AtRate: 0}, …)`；取 `BatteryInformation` → `BATTERY_INFORMATION`（设计容量、满充容量、化学性质、循环数），`BatteryTemperature` → `ULONG`（0.1 K），`BatteryEstimatedTime` → `ULONG` 秒。字符串层级只在有需要时取，如 `BatteryDeviceName`；诊断不采集序列号。`IOCTL_BATTERY_QUERY_STATUS` 用 `BATTERY_WAIT_STATUS`（相同 tag，`Timeout=0`），读 `BATTERY_STATUS`（剩余容量、电源标志、mV、电池 `Rate: LONG`，正为充电、负为放电）。三个 IOCTL 分别为 `0x294040`、`0x294044`、`0x29404C`，以 SDK `poclass.h` 为准；校验 IOCTL 返回码、`bytesReturned` 和该层输出结构长度。tag 失效后重新枚举或取 tag 后只重试一次。
3. 仅报告 `BATTERY_SYSTEM_BATTERY` 主电池或其按设备分列的样本。`BATTERY_CAPACITY_RELATIVE` 时不标 mWh/mW；`0xFFFFFFFF` 的容量、电压、估计时间及 `0x80000000` 的速率为缺失；`CycleCount=0` 不能区分不支持与零循环。健康度 `fullChargedCapacity / designedCapacity` 需同单位、有效且非零分母；不要用设计容量计算当前电量。UI 的 OS 综合电量和电源来源可由 `GetSystemPowerStatus` 初始化，随后由 `RegisterPowerSettingNotification` 推送 `GUID_ACDC_POWER_SOURCE`（`PoAc=0, PoDc=1, PoHot=2`）与 `GUID_BATTERY_PERCENTAGE_REMAINING`；不拿该百分比代替 EC 充电阈值。

### 6.2 WMI 事件与轮询边界

本机 `MSI_Event` 继承 `WMIEvent`，GUID `{5B3CC38A-40D9-7245-8AE6-1145B751BE3F}`，属性 `InstanceName`、`Active`、`MSIEvt: UInt32`、`TIME_CREATED`、`SECURITY_DESCRIPTOR`。DSDT `_WDG` 映射 event `0xC0`；`_WED(0xC0)` 读取 EC `0xD8` 和事件详情，**返回前会把 `0xD8` 写为零**。静态映射包含 `0x56/57/5F/7B/84` 分支读取 `0x2E`、`0x62/63` 分支读取 `0xD1`；这些是 AML 分支码，不等于已经捕获的 `MSIEvt`/用户动作。严格只读 Phase 0 不安装可能求值 `_WED` 的 `ExecNotificationQueryAsync` 订阅，不以读到事件类代替事件交付能力。后续验证需单独授权固件事件确认副作用；先验证注册、自然事件复现及资源时序，再在另外获准的环境评估 OEM 依赖。禁用时使用 Windows 原生亮度、电源、显示和会话通知，不退回 `WITHIN n` 的 WMI 轮询。

HAL 提供聚合快照，不假定一次 WMI 往返包含全部遥测。基础温度与 RPM 为 `Get_Temperature(0)` + `Get_Fan(0)` **两次**只读调用；占比另需 `Get_Fan(1/2)`，GPU/CPU 功耗分别来自受控 NVML/有证 OS 计数器，来源或休眠状态不明就保留缺失。每字段有单调采样时间、状态与来源；同一发布批次允许字段时间不同。后台未订阅时不采样，UI/OSD 可见或已启用的遥测规则最多共享一组 EC 采样（初始 1 Hz，受背压），无等待样本时不保留采样定时器。事件触发规则不主动读取固件，过期/缺失输入为 `Unknown`，不补零、不触发动作。

停用器属于另行授权事务。HAL 接口顺序为：只读预检与基线 → 停用期间暂停 EC 队列且不接触 WMI 侧通道 → 只读探活及回读 SCM/任务/注册表实际状态 → 失败还原后再复核。保留 `WmiAcpi` 和 MSI WMI schema 资源，停用器不执行 EC 写；服务依赖验证不混入本轮或严格只读探测。

## 7. Phase 0 `msilt-probe` 验收清单

| 编号 | 只读验收及开放条件 |
|---|---|
| P0-WMI | 在授权会话枚举 `MSI_ACPI`，记录活动实例数、HRESULT、`Data` 类型/长度和耗时。调用 `Get_EC`、`Get_WMI`，以及明确的 `Get_Data` 地址/`Get_Fan`/`Get_Thermal`/`Get_Temperature`/`Get_Device`/`Get_AP`/`Get_Power` 选择器；比对身份、状态字节和本节事实。`Get_EC2`、`Get_Range` 记录为“MOF 有、当前 AML 无”；仅在发现新的、已静态确认无副作用的对应固件分支后单独探测，不扫描。 |
| P0-EC | 稳定状态下对照块读与独立 `Get_Data`；把厂值、临时实际值、额外槽分别保存。关联 `0xD7` 预设语义、`0xD3` 亮度和 General Settings 极性；只读无法确认的位保持锁定，后续单独验收写入/回退。 |
| P0-HID | 保存脱敏描述符指纹、Report IDs、占用/冲突状态和 OEM 最近区数证据。本机描述符已采集；区位、子命令效果与状态回读延后到 Phase 2，不能用空白/未知报告自检。 |
| P0-MUX | 记录平台启动方式、已获授权的变量读取结果/长度/属性、活动内屏路由和 EC/OEM 指示；模式证据不齐时 `current/pending` 各自报告 `Unknown`，不可写。 |
| P0-GPU | 只读核对库出口和驱动 ABI；额外获准唤醒时读取当前偏移与范围并测 dGPU 前后电源状态，不试写偏移。 |
| P0-Windows | CCD 目标、`60/240` 可能模式、当前亮度档位、ICM 当前关联/MHC2 能力、电池 tag/单位分别核对，加入设备更换、恢复后读值的用例。 |
| P-A1–A4 | 记录 `MSI_Event` schema 与 `_WED` 清写事实：P-A1 注册、P-A2 实际事件编码、P-A4 长时资源需另行授权；P-A3 官软服务依赖需独立停用试验。严格只读 Phase 0 不启用固件事件。 |

离线验证固定长度/状态校验、RPM 大端数值、温度节点重排、HID 选择掩码、未知固件/地址拒绝、硬编码拒写、冲突和迟到回调；Mock 应模拟回读不一致、部分完成、超时和恢复。探测报告保留版本与不敏感原始字节；不包含设备序列号、用户路径、完整 DSDT/UEFI 变量内容或可执行命令。

主要参考：[Linux WMI 文档][linux-wmi]、[Linux 平台驱动][linux-driver]、[msi-ec 的机型表][msi-ec]、[GhostDeck 协议记录][ghostdeck]、[LightShow][lightshow]、[MS-1603 实机协议][ripple]、[MSI 24 区分支参考][mystic]、[Microsoft WMI ExecMethod][win-wmi]、[HID 报告 API][win-hid]、[CCD][win-ccd]、[Windows 色彩配置][win-wcs]、[亮度 WMI][win-brightness]、[电池 IOCTL][win-battery]、[NVIDIA NVML][nvml]、[NVIDIA NVAPI][nvapi]。

[linux-wmi]: https://docs.kernel.org/wmi/devices/msi-wmi-platform.html
[linux-driver]: https://github.com/torvalds/linux/blob/master/drivers/platform/x86/msi-wmi-platform.c
[msi-ec]: https://github.com/BeardOverflow/msi-ec/blob/main/msi-ec.c
[ghostdeck]: https://github.com/wygodad/ghostdeck/blob/main/docs/TECHNICAL.md
[lightshow]: https://github.com/nixfred/lightshow/blob/main/docs/PROTOCOL.md
[ripple]: https://github.com/GeBondar/msi-kb-ripple/blob/main/msikb.py
[mystic]: https://github.com/anas-kummali/msi-mysticlight-controller-linux/blob/main/src/protocol.rs
[win-wmi]: https://learn.microsoft.com/windows/win32/api/wbemcli/nf-wbemcli-iwbemservices-execmethod
[win-hid]: https://learn.microsoft.com/windows-hardware/drivers/hid/sending-hid-reports
[win-ccd]: https://learn.microsoft.com/windows/win32/api/winuser/nf-winuser-setdisplayconfig
[win-wcs]: https://learn.microsoft.com/windows/win32/api/icm/nf-icm-wcssetdefaultcolorprofile
[win-brightness]: https://learn.microsoft.com/windows/win32/wmicoreprov/wmisetbrightness-method-in-class-wmimonitorbrightnessmethods
[win-battery]: https://learn.microsoft.com/windows/win32/power/ioctl-battery-query-information
[nvml]: https://github.com/NVIDIA/go-nvml/blob/main/pkg/nvml/nvml.h
[nvapi]: https://github.com/NVIDIA/nvapi/blob/main/nvapi_interface.h
