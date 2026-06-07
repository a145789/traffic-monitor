[Setup]
AppName=Traffic Monitor
AppVersion=0.1.2
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

[Files]
Source: "target\release\traffic-monitor.exe"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{group}\Traffic Monitor"; Filename: "{app}\traffic-monitor.exe"
Name: "{group}\Uninstall Traffic Monitor"; Filename: "{uninstallexe}"
Name: "{autodesktop}\Traffic Monitor"; Filename: "{app}\traffic-monitor.exe"; Tasks: desktopicon

[Tasks]
Name: "desktopicon"; Description: "Create desktop shortcut"; GroupDescription: "Additional icons:"
Name: "startup"; Description: "Run at Windows startup"; GroupDescription: "Startup:"

[Registry]
Root: HKCU; Subkey: "Software\Microsoft\Windows\CurrentVersion\Run"; ValueType: string; ValueName: "TrafficMonitor"; ValueData: """{app}\traffic-monitor.exe"""; Flags: uninsdeletevalue; Tasks: startup

[Run]
Filename: "{app}\traffic-monitor.exe"; Description: "Launch Traffic Monitor"; Flags: nowait postinstall skipifsilent
