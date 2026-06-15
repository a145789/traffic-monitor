[Setup]
AppName=Traffic Monitor
AppVersion=0.6.2
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
function InitializeSetup(): Boolean;
var
  ResultCode: Integer;
begin
  Exec(ExpandConstant('{cmd}'), '/C taskkill /F /T /IM traffic-monitor.exe >nul 2>&1', '', SW_HIDE, ewWaitUntilTerminated, ResultCode);
  Result := True;
end;
