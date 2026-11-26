# MSI-Toolkit

面向 **MSI Vector 17 HX AI A2XWJG（`MS-17S3`）** 的超轻量、零第三方驱动、Windows 11 原生硬件控制套件，用于全面替代臃肿的 MSI Center / Mystic Light / True Color 套件。

## 核心亮点

- **极致轻量（实机对比）**：
  - 后台常驻守护进程 `msilt-daemon.exe` 静置工作集 **0.32 MiB** / 私有内存 **3.20 MiB** / CPU 增量 **0 ms**（对比 MSI Center 790 MiB 降低 **99.6%**）；
  - 全部可执行文件 **0 禁止 GPU/dGPU DLL 导入**，纯软件光栅 UI（Slint + DWM Mica）不唤醒独立显卡；
  - 零第三方内核驱动，完全兼容 Secure Boot / HVCI / Microsoft Vulnerable Driver Blocklist。
- **完整硬件与系统控制**：
  - **性能与风扇**：性能模式切换、强冷散热、CPU/GPU 双通道 6 节点风扇曲线编辑与回读校验；
  - **显卡与显示**：面板刷新率（60/240 Hz）、背光亮度、出厂 14 套 MHC2 校色档一键切换、GPU MUX 状态视图；
  - **电池与电源**：真实 IOCTL 电池健康度/容量/电压/充放电功率、充电上限保护；
  - **键盘灯效**：Wootility 风格全尺寸 24 区 Per-Zone RGB 物理键盘视图 + 可拖拽 2D HSV 调色盘与色相条；
  - **自动化与 OSD**：事件驱动规则持久化/预设/干跑试跑、纯软件 GDI+ 屏幕右下角悬浮提示；
  - **内置开机自启 & MSI 官软控制器**：应用内一键管理 `\MSI-Toolkit\Daemon` 免 UAC 开机自启，支持对 MSI Center / SDK / True Color 禁自启、停止、可逆备份恢复与两步确认卸载（锁定保护 `msiapcfg.dll`、`WmiAcpi`、`Nahimic*`、`Killer*` 等底层驱动）；
  - **Windows 标准原生安装器与安全在线更新**：基于 Inno Setup 6 的标准中文安装向导 `MSI-Toolkit-Setup.exe`，支持自定义安装位置（自动对所选目录施加管理员级 NTFS 访问控制保护）、自定义开始菜单文件夹或「不创建开始菜单」、独立勾选「开机自启」与「桌面快捷方式」；应用内经 Windows 原生 WinHTTP + CNG `SHA-256` 校验后静默升级。

## 安装、更新与卸载

1. **安装（标准 Windows 向导）**：前往 [Releases](https://github.com/pkeran/MSI-Toolkit/releases/latest) 下载 **`MSI-Toolkit-Setup.exe`** 并双击运行：
   - **选择目标位置**：默认 `C:\Program Files\MSI-Toolkit`，可自由更改为任意本地固定磁盘专用目录（安装程序会自动加固目录权限，确保高权限后台安全与免 UAC 运行）；
   - **选择开始菜单文件夹**：可自定义文件夹名或勾选「不创建开始菜单文件夹」；
   - **选择附加任务**：可独立勾选「开机登录时自动启动后台守护进程（免 UAC）」与「创建桌面快捷方式」；
   - 安装完成后自动启动后台服务并进入主界面，常用设置（主题、自启、自动检查更新、桌面 OSD 提示、MSI 官软控制）均集中在 **「系统与工具」**。
2. **在线一键更新**：
   - 保持「启动时自动检查更新」开启，有新版本时在 **「系统与工具」** 点击 **「立即更新」**，自动下载、校验 `SHA-256`、沿用原有安装位置与自启设置完成静默升级并重启。
3. **彻底卸载**：
   - 在 **Windows 设置 → 应用 → MSI-Toolkit → 卸载**，或在应用内 **「系统与工具 → 卸载程序」** 启动标准卸载向导；
   - 自动彻底停止后台并清除全部程序文件、安装目录、快捷方式、`\MSI-Toolkit\Daemon` 计划任务（含任务文件夹）与注册表项；
   - 卸载时会主动询问是否一并清除个人配置与自动化规则（`%LOCALAPPDATA%\MSI-Toolkit`，静默卸载可传 `/PURGE`），**绝不触碰 `msiapcfg.dll`、`WmiAcpi`、`Nahimic`、`Killer` 等任何系统驱动**。

> 如需脚本化或便携审计部署，也可下载同版本的 `MSI-Toolkit-win64.zip`。

## 源码构建与发版

```powershell
# 运行全工作区测试、体积预算、零 GPU 导入与实机基准验证
.\scripts\verify-all.ps1

# 本地打包 MSI-Toolkit-Setup.exe（需 Inno Setup 6）、MSI-Toolkit-win64.zip 与 SHA256SUMS.txt
.\scripts\package.ps1

# 推送新版本 Tag 触发 GitHub Actions 自动构建并发布 Release
git tag v0.1.4
git push origin v0.1.4
```
