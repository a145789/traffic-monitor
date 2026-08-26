[Setup]
AppName=Traffic Monitor
AppVersion=1.1.1
AppPublisher=Traffic Monitor
AppMutex=TrafficMonitor_Mutex_Instance
DefaultDirName={autopf}\Traffic Monitor
DefaultGroupName=Traffic Monitor
OutputBaseFilename=TrafficMonitor-Setup
Compression=lzma2
SolidCompression=yes
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
UninstallDisplayIcon={app}\traffic-monitor.exe
SetupIconFile=assets\icon.ico
WizardStyle=modern
CloseApplications=no
RestartApplications=no
; 安装器以 admin 运行但有意写 HKCU Run 键（startup 任务=安装者本人开机自启），
; 单管理员场景下 UAC 提权账户与登录账户一致；此为知情选择，抑制 IS7 新增警告。
UsedUserAreasWarning=no

[Files]
Source: "target\release\traffic-monitor.exe"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{group}\Traffic Monitor"; Filename: "{app}\traffic-monitor.exe"
Name: "{group}\Uninstall Traffic Monitor"; Filename: "{uninstallexe}"
Name: "{autodesktop}\Traffic Monitor"; Filename: "{app}\traffic-monitor.exe"; Tasks: desktopicon

[Languages]
Name: "chinesesimp"; MessagesFile: "compiler:Languages\ChineseSimplified.isl"

[Tasks]
Name: "desktopicon"; Description: "创建桌面快捷方式"; GroupDescription: "附加任务:"
Name: "startup"; Description: "开机自动启动"; GroupDescription: "启动选项:"

[Registry]
Root: HKCU; Subkey: "Software\Microsoft\Windows\CurrentVersion\Run"; ValueType: string; ValueName: "TrafficMonitor"; ValueData: """{app}\traffic-monitor.exe"""; Flags: uninsdeletevalue; Tasks: startup

[Run]
Filename: "{app}\traffic-monitor.exe"; \
Description: "启动 Traffic Monitor"; \
Flags: nowait postinstall

[Code]
// 反复强杀目标进程，直到系统内不存在 traffic-monitor.exe（taskkill 返回 128
// 表示未找到匹配进程）或达到重试上限后放行。保证进入拷贝阶段时旧版 exe 的
// 映像句柄已全部释放，避免 Inno 弹「文件正在使用」错误框。
function InitializeSetup(): Boolean;
var
  ResultCode: Integer;
  Attempt: Integer;
begin
  for Attempt := 1 to 24 do
  begin
    Exec(ExpandConstant('{cmd}'), '/C taskkill /F /T /IM traffic-monitor.exe >nul 2>&1', '', SW_HIDE, ewWaitUntilTerminated, ResultCode);
    if ResultCode = 128 then
      Break;
    Sleep(250);
  end;
  Result := True;
end;
