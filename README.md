# MSI-Toolkit

面向 **MSI Vector 17 HX AI A2XWJG（`MS-17S3`）** 的超轻量、零第三方驱动、Windows 11 原生硬件控制套件，用于全面替代臃肿的 MSI Center / Mystic Light / True Color 套件。

## 核心亮点

- **极致轻量（实机对比）**：
  - 后台常驻守护进程 `msilt-daemon.exe` 静置工作集 **0.32 MiB** / 私有内存 **3.17 MiB** / CPU 增量 **0 ms**（对比 MSI Center 790 MiB 降低 **99.6%**）；
  - 全部可执行文件 **0 禁止 GPU/dGPU DLL 导入**，纯软件光栅 UI（Slint + DWM Mica）不唤醒独立显卡；
  - 零第三方内核驱动，完全兼容 Secure Boot / HVCI / Microsoft Vulnerable Driver Blocklist。
- **完整硬件与系统控制**：
  - **性能与风扇**：性能模式切换、Cooler Boost、CPU/GPU 双通道 6 节点风扇曲线编辑与回读校验；
  - **显卡与显示**：面板刷新率（60/240 Hz）、背光亮度、出厂 14 套 MHC2 校色档一键切换、GPU MUX 状态视图；
  - **电池与电源**：真实 IOCTL 电池健康度/容量/电压/充放电功率、充电上限保护；
  - **键盘灯效**：Wootility 风格全尺寸 24 区 Per-Zone RGB 物理键盘视图 + 可拖拽 2D HSV 调色盘与色相条；
  - **自动化与 OSD**：事件驱动规则持久化/预设/干跑试跑、纯软件 GDI+ 屏幕右下角悬浮提示；
  - **内置开机自启 & MSI 官软控制器**：应用内一键管理 `\MSI-Toolkit\Daemon` 免 UAC 开机自启，支持对 MSI Center / SDK / True Color 禁自启、停止、可逆备份恢复与两步确认卸载（锁定保护 `msiapcfg.dll`、`WmiAcpi`、`Nahimic*`、`Killer*` 等底层驱动）；
  - **极简单文件安装与自动更新**：单文件原生 Windows 11 安装程序 `MSI-Toolkit-Setup.exe`（支持标准系统应用注册、快捷方式/自启选项、平滑覆盖升级与干净卸载）；应用内自动检测 GitHub Releases 新版本，经 Windows CNG `SHA-256` 严格校验后一键更新。

## 安装、设置与更新

1. **单文件安装（推荐）**：前往 [Releases](https://github.com/pkeran/MSI-Toolkit/releases/latest) 下载 **`MSI-Toolkit-Setup.exe`** 并双击运行：
   - 可按需勾选「开机自动启动后台守护进程（免 UAC）」「开始菜单快捷方式」「桌面快捷方式」「安装后立即启动」；
   - 自动安装至系统安全目录 `C:\Program Files\MSI-Toolkit\` 并注册到 **Windows 设置 → 应用 → 已安装的应用**；
2. **首次启动设置向导**：
   - 首次打开自动弹出精简 1 页引导，确认硬件识别状态、主题、开机自启、自动检查更新、OSD 悬浮通知，以及是否一键禁启并停止冲突的 MSI 官方软件（自动保留可逆备份）；
3. **后续一键更新**：
   - 保持「启动时自动检查更新」开启，有新版本时在顶部提示栏或 **「系统与工具」** 点击 **「立即更新」**，自动下载、校验 `SHA-256`、保留现有配置与自启偏好并无缝重启到新版本；
4. **标准卸载**：
   - 在 **Windows 设置 → 应用 → MSI-Toolkit** 点击卸载，或在软件 **「系统与工具 → 卸载程序」** 启动卸载界面（可选是否保留本地配置；绝不触碰任何系统底层驱动）。

> 如需脚本化或便携审计部署，也可下载同版本的 `MSI-Toolkit-win64.zip`（含 `install.cmd` / `uninstall.cmd`）。

## 源码构建与发版

```powershell
# 运行全工作区测试、体积预算、零 GPU 导入与实机基准验证
.\scripts\verify-all.ps1

# 本地打包 MSI-Toolkit-Setup.exe、MSI-Toolkit-win64.zip 与 SHA256SUMS.txt
.\scripts\package.ps1

# 推送新版本 Tag 触发 GitHub Actions 自动构建并发布 Release
git tag v0.1.0
git push origin v0.1.0
```
