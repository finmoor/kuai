; Kuai 的安装程序（Inno Setup 6.7）。tools/package.ps1 打完包后调 ISCC 编译：
;   ISCC /DAppVersion=1.8.0 /DPackageDir=<打好的 Kuai 文件夹> /DOutputDir=<dist> tools\installer\kuai.iss
; 出来的是 kuai-<版本>-windows-x64-setup.exe。
;
; 装在当前用户的目录下（%LOCALAPPDATA%\Programs\Kuai），不要管理员权限，所以 Kuai 自己能升级。
; 向导只有两三步：第一次装时选位置（默认就行）、要不要桌面图标，装完勾「启动 Kuai」。
; 语言跟系统：中文系统是简体中文，别的是英文。
;
; Kuai 升级时在后台跑新的安装程序（crates/fr-app/src/update.rs 的 run_setup）：
;   kuai-setup.exe /VERYSILENT /SUPPRESSMSGBOXES /NORESTART /relaunch=1 /DIR=<文件夹> /LOG=<文件> /waitpid=<进程号>
; /waitpid：先等老的 Kuai 退出（最多 30 秒）；/relaunch=1：装好后打开新的 Kuai。
;
; 卸载：先确认 Kuai 没在运行；开机自启指向这里的话删掉（kuai.exe --uninstall）；
; 凭据管理器里存着服务器密码的话问一句删不删；设置文件（%APPDATA%\kuai）留着。

#ifndef AppVersion
  #error 要用 /DAppVersion=<版本号> 编译
#endif
#ifndef PackageDir
  #error 要用 /DPackageDir=<打好的 Kuai 文件夹> 编译
#endif
#ifndef OutputDir
  #define OutputDir "."
#endif

[Setup]
; 不能改：Windows 靠它认出装过的 Kuai，升级时装回原来的位置
AppId={{C735581E-2007-4092-88E3-D90D0CDCC820}
AppName=Kuai
AppVersion={#AppVersion}
AppVerName=Kuai {#AppVersion}
AppPublisher=finmoor
AppPublisherURL=https://github.com/finmoor/kuai
AppSupportURL=https://github.com/finmoor/kuai/issues
AppUpdatesURL=https://github.com/finmoor/kuai/releases
VersionInfoVersion={#AppVersion}
; 不要管理员权限时 {autopf} 就是 %LOCALAPPDATA%\Programs
DefaultDirName={autopf}\Kuai
PrivilegesRequired=lowest
DisableProgramGroupPage=yes
DisableDirPage=auto
DisableReadyPage=yes
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
MinVersion=10.0
WizardStyle=modern dynamic
; 最后一页左边的竖条图，按屏幕缩放挑一张（make-wizard-image.py 画的）
WizardImageFile=wizard-125.png,wizard-150.png,wizard-200.png,wizard-250.png,wizard-300.png
WizardSmallImageFile=..\..\crates\fr-app\assets\icon.png
SetupIconFile=..\..\crates\fr-app\assets\icon.ico
UninstallDisplayIcon={app}\kuai.exe
UninstallDisplayName=Kuai
ShowLanguageDialog=no
LanguageDetectionMethod=uilanguage
; Kuai 开着时（比如收在托盘里）先关掉它；装完由下面的 [Run] 打开，不用安装程序自己重开
CloseApplications=force
RestartApplications=no
OutputDir={#OutputDir}
OutputBaseFilename=kuai-{#AppVersion}-windows-x64-setup
Compression=lzma2/max
SolidCompression=yes

[Languages]
; 第一个是认不出系统语言时用的
Name: "en"; MessagesFile: "compiler:Default.isl"
; 取自 Inno Setup 仓库 is-6_7_3 标签下的 Files/Languages/Unofficial/ChineseSimplified.isl
Name: "zh"; MessagesFile: "ChineseSimplified.isl"

[CustomMessages]
en.ForgetPasswords=Also delete the server passwords and key passphrases Kuai saved in Windows Credential Manager?
zh.ForgetPasswords=是否同时删除 Kuai 保存在 Windows 凭据管理器中的服务器密码和私钥口令？
en.CloseKuai=Kuai is running. Quit Kuai (including from the system tray), then click Retry.
zh.CloseKuai=Kuai 正在运行。请先退出 Kuai（包括托盘中的 Kuai），然后点击「重试」。

[Messages]
; 不显示「准备安装」页，任务页上的按钮是「安装」（见 CurPageChanged），说明跟着改
en.SelectTasksLabel2=Select the additional tasks you would like Setup to perform while installing [name], then click Install.
zh.SelectTasksLabel2=选择安装 [name] 时要执行的附加任务，然后点击“安装”。

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked

[Files]
Source: "{#PackageDir}\*"; DestDir: "{app}"; Flags: ignoreversion recursesubdirs createallsubdirs

[Icons]
Name: "{autoprograms}\Kuai"; Filename: "{app}\kuai.exe"
Name: "{autodesktop}\Kuai"; Filename: "{app}\kuai.exe"; Tasks: desktopicon

[Run]
Filename: "{app}\kuai.exe"; Description: "{cm:LaunchProgram,Kuai}"; Flags: nowait postinstall skipifsilent
; Kuai 自己升级时（静默安装）装好后打开新的
Filename: "{app}\kuai.exe"; Flags: nowait; Check: RelaunchAfterUpdate

[UninstallDelete]
; 升级时下载、暂存的东西
Type: filesandordirs; Name: "{localappdata}\kuai\update"
Type: dirifempty; Name: "{localappdata}\kuai"

[Code]
const
  SYNCHRONIZE = $00100000;

function OpenProcess(DesiredAccess: DWORD; InheritHandle: BOOL; ProcessId: DWORD): THandle;
  external 'OpenProcess@kernel32.dll stdcall';
function WaitForSingleObject(Handle: THandle; Milliseconds: DWORD): DWORD;
  external 'WaitForSingleObject@kernel32.dll stdcall';
function CloseHandle(Handle: THandle): BOOL;
  external 'CloseHandle@kernel32.dll stdcall';

{ /waitpid=<进程号>：先等老的 Kuai 退出，免得它还占着 kuai.exe }
function InitializeSetup(): Boolean;
var
  Pid: Integer;
  Process: THandle;
begin
  Result := True;
  Pid := StrToIntDef(ExpandConstant('{param:waitpid|0}'), 0);
  if Pid > 0 then
  begin
    Process := OpenProcess(SYNCHRONIZE, False, Pid);
    if Process <> 0 then
    begin
      WaitForSingleObject(Process, 30000);
      CloseHandle(Process);
    end;
  end;
end;

{ 不显示「准备安装」页，所以任务页上的按钮点了就开始装：写成「安装」 }
procedure CurPageChanged(CurPageID: Integer);
begin
  if CurPageID = wpSelectTasks then
    WizardForm.NextButton.Caption := SetupMessage(msgButtonInstall);
end;

function RelaunchAfterUpdate(): Boolean;
begin
  Result := ExpandConstant('{param:relaunch|0}') = '1';
end;

{ 这个文件夹里的 kuai.exe 还在运行吗（问 WMI；问不成当作没在运行） }
function KuaiRunning(): Boolean;
var
  Locator, Service, Found: Variant;
  Path: String;
begin
  Result := False;
  Path := ExpandConstant('{app}\kuai.exe');
  StringChangeEx(Path, '\', '\\', True);
  StringChangeEx(Path, '''', '\''', True);
  try
    Locator := CreateOleObject('WbemScripting.SWbemLocator');
    Service := Locator.ConnectServer('.', 'root\CIMV2');
    Found := Service.ExecQuery('SELECT ProcessId FROM Win32_Process WHERE ExecutablePath = ''' + Path + '''');
    Result := Found.Count > 0;
  except
    Result := False;
  end;
end;

function InitializeUninstall(): Boolean;
begin
  Result := True;
  while KuaiRunning() do
    if SuppressibleMsgBox(CustomMessage('CloseKuai'), mbError, MB_RETRYCANCEL, IDCANCEL) = IDCANCEL then
    begin
      Result := False;
      Exit;
    end;
end;

{ 删文件之前：让 kuai.exe 自己清开机自启；存着密码的话问一句 }
procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
var
  Exe, Params: String;
  Code: Integer;
begin
  if CurUninstallStep <> usUninstall then
    Exit;
  Exe := ExpandConstant('{app}\kuai.exe');
  if not FileExists(Exe) then
    Exit;
  Params := '--uninstall';
  if Exec(Exe, '--has-saved-passwords', '', SW_HIDE, ewWaitUntilTerminated, Code) and (Code = 0) then
    if SuppressibleMsgBox(CustomMessage('ForgetPasswords'), mbConfirmation, MB_YESNO, IDNO) = IDYES then
      Params := Params + ' --forget-passwords';
  Exec(Exe, Params, '', SW_HIDE, ewWaitUntilTerminated, Code);
end;
