#define AppVersion GetVersionNumbersString(Payload + "\msilt-ui.exe")
[Setup]
AppId=MSI-Toolkit
AppName=MSI-Toolkit
AppVersion={#ReleaseVersion}
VersionInfoVersion={#AppVersion}
AppPublisher=MSI-Toolkit
AppPublisherURL=https://github.com/pkeran/MSI-Toolkit
AppSupportURL=https://github.com/pkeran/MSI-Toolkit/issues
DefaultDirName={commonpf64}\MSI-Toolkit
AppendDefaultDirName=yes
UsePreviousAppDir=yes
DisableDirPage=no
DefaultGroupName=MSI-Toolkit
UsePreviousGroup=yes
DisableProgramGroupPage=no
AllowNoIcons=yes
UsePreviousTasks=yes
DisableReadyPage=no
DisableWelcomePage=yes
WizardStyle=modern
ShowLanguageDialog=no
LanguageDetectionMethod=uilanguage
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
PrivilegesRequired=admin
OutputBaseFilename=MSI-Toolkit-Setup
SetupIconFile=..\assets\msilt.ico
UninstallDisplayName=MSI-Toolkit
UninstallDisplayIcon={app}\msilt-ui.exe
Compression=lzma2
SolidCompression=yes
CloseApplications=no
RestartApplications=no

[Languages]
Name: "zhcn"; MessagesFile: "{#ChineseMessages}"
Name: "en"; MessagesFile: "compiler:Default.isl"

[CustomMessages]
zhcn.StartFailure=后台服务未能启动，请检查权限后重试安装。
en.StartFailure=The background service did not start. Please retry installation.
zhcn.StopFailure=无法关闭正在运行的 MSI-Toolkit。
en.StopFailure=Could not close running MSI-Toolkit processes.
zhcn.InvalidDir=请选择本地固定磁盘上的专用文件夹（例如 C:\Program Files\MSI-Toolkit 或 D:\MSI-Toolkit），不能直接安装到磁盘根目录、可移动磁盘或网络路径。
en.InvalidDir=Please choose a dedicated folder on a local fixed drive (such as C:\Program Files\MSI-Toolkit or D:\MSI-Toolkit). Drive roots, removable drives, and network paths are not supported.
zhcn.TaskGroupStartup=启动选项：
en.TaskGroupStartup=Startup options:
zhcn.TaskAutoStart=开机登录时自动启动后台守护进程（免 UAC，推荐）
en.TaskAutoStart=Start background service automatically at sign-in (zero-UAC, recommended)
zhcn.PurgePrompt=是否同时删除个人配置、自动化规则与官软状态备份（%LOCALAPPDATA%\MSI-Toolkit）？%n%n选择「否」将保留个人设置以便日后重装恢复；程序文件、快捷方式、注册表与开机任务都会自动彻底清除。
en.PurgePrompt=Also remove your personal settings, automation rules, and OEM state backup (%LOCALAPPDATA%\MSI-Toolkit)?%n%nChoose No to keep your settings for future reinstallation. Program files, shortcuts, registry entries, and startup tasks are always removed cleanly.

[Tasks]
Name: "autostart"; Description: "{cm:TaskAutoStart}"; GroupDescription: "{cm:TaskGroupStartup}"
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked

[Registry]
Root: HKLM64; Subkey: "SOFTWARE\MSI-Toolkit"; ValueType: string; ValueName: "InstallDir"; ValueData: "{app}"; Flags: uninsdeletekey

[Files]
Source: "{#Payload}\uninstall.exe"; DestName: "msilt-maintenance.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#Payload}\uninstall.exe"; DestName: "msilt-preflight.exe"; Flags: dontcopy
Source: "{#Payload}\msilt-cli.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#Payload}\msilt-probe.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#Payload}\msilt-daemon.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#Payload}\msilt-ui.exe"; DestDir: "{app}"; Flags: ignoreversion; AfterInstall: StartBackend

[Icons]
Name: "{group}\MSI-Toolkit"; Filename: "{app}\msilt-ui.exe"; WorkingDir: "{app}"
Name: "{autodesktop}\MSI-Toolkit"; Filename: "{app}\msilt-ui.exe"; WorkingDir: "{app}"; Tasks: desktopicon

[Run]
Filename: "{app}\msilt-ui.exe"; Description: "{cm:LaunchProgram,MSI-Toolkit}"; Flags: nowait postinstall skipifsilent runasoriginaluser
Filename: "{app}\msilt-ui.exe"; Flags: nowait runasoriginaluser; Check: IsAppUpdate

[UninstallDelete]
Type: files; Name: "{app}\uninstall.exe"
Type: files; Name: "{app}\install.ps1"
Type: files; Name: "{app}\install.cmd"
Type: files; Name: "{app}\uninstall.ps1"
Type: files; Name: "{app}\uninstall.cmd"
Type: files; Name: "{app}\README.txt"
Type: files; Name: "{app}\startup-check-*.txt"
Type: files; Name: "{app}\*.new"
Type: files; Name: "{app}\*.tmp"
Type: dirifempty; Name: "{app}"

[Code]
const
  DRIVE_FIXED = 3;

var
  PurgeUserData: Boolean;

function GetDriveTypeW(lpRootPathName: string): UINT;
  external 'GetDriveTypeW@kernel32.dll stdcall';

function HasCmdSwitch(SwitchName: String): Boolean;
var
  I: Integer;
begin
  Result := False;
  for I := 1 to ParamCount do
    if CompareText(ParamStr(I), SwitchName) = 0 then
    begin
      Result := True;
      Exit;
    end;
end;

function IsAppUpdate: Boolean;
begin
  Result := WizardSilent and HasCmdSwitch('/MSILTUPDATE');
end;

function IsValidLocalInstallDir(Dir: String): Boolean;
var
  Clean, Root: String;
begin
  Result := False;
  Clean := RemoveBackslashUnlessRoot(Trim(Dir));
  if Length(Clean) < 4 then Exit;
  if (Clean[2] <> ':') or (Clean[3] <> '\') then Exit;
  if Pos('\\', Clean) > 0 then Exit;
  Root := Copy(Clean, 1, 3);
  if GetDriveTypeW(Root) <> DRIVE_FIXED then Exit;
  Result := True;
end;

function NextButtonClick(CurPageID: Integer): Boolean;
begin
  Result := True;
  if CurPageID = wpSelectDir then
  begin
    if not IsValidLocalInstallDir(WizardDirValue) then
    begin
      MsgBox(CustomMessage('InvalidDir'), mbError, MB_OK);
      Result := False;
    end;
  end;
end;

function RunHelper(Path, Params: String): Boolean;
var
  Code: Integer;
begin
  Result := Exec(Path, Params, '', SW_HIDE, ewWaitUntilTerminated, Code) and (Code = 0);
end;

function PrepareToInstall(var NeedsRestart: Boolean): String;
begin
  if not IsValidLocalInstallDir(ExpandConstant('{app}')) then
  begin
    Result := CustomMessage('InvalidDir');
    Exit;
  end;
  ExtractTemporaryFile('msilt-preflight.exe');
  if not RunHelper(ExpandConstant('{tmp}\msilt-preflight.exe'), '--wizard-stop') then
    Result := CustomMessage('StopFailure');
end;

procedure StartBackend;
var
  AutoMode, Params: String;
begin
  if IsAppUpdate then
    AutoMode := 'preserve'
  else if WizardIsTaskSelected('autostart') then
    AutoMode := '1'
  else
    AutoMode := '0';
  Params := '--wizard-start --dir "' + RemoveBackslashUnlessRoot(ExpandConstant('{app}')) + '" --autostart=' + AutoMode;
  if not RunHelper(ExpandConstant('{app}\msilt-maintenance.exe'), Params) then
    RaiseException(CustomMessage('StartFailure'));
end;

function InitializeUninstall(): Boolean;
begin
  Result := True;
  PurgeUserData := HasCmdSwitch('/PURGE');
  if (not UninstallSilent) and (not PurgeUserData) then
  begin
    PurgeUserData := (MsgBox(CustomMessage('PurgePrompt'), mbConfirmation, MB_YESNO or MB_DEFBUTTON2) = IDYES);
  end;
end;

procedure CurUninstallStepChanged(Step: TUninstallStep);
var
  Params: String;
begin
  if Step = usUninstall then
  begin
    Params := '--wizard-remove';
    if PurgeUserData then
      Params := Params + ' --purge';
    if not RunHelper(ExpandConstant('{app}\msilt-maintenance.exe'), Params) then
      RaiseException(CustomMessage('StopFailure'));
  end;
end;
