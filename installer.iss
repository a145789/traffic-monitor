[Setup]
AppName=Traffic Monitor
AppVersion=1.4.0
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
// 安装交接实行“先礼后兵”：拷贝阶段前先请求旧实例优雅退出并等待其消失，
// 仅在超时后才对残留实例做有限次强制终止；强杀永远是兜底而非首选。
// 顺序证据：拷贝阶段前“优雅退出等待→超时才强杀”——CurStepChanged(ssInstall)
// 依次调用 RequestGracefulExit、WaitForSingleInstanceGone，
// 仅当等待超时才调用 ForceKillRemnant。
const
  // 优雅退出等待上限（毫秒），与 src/config.rs 的 MAIN_EXIT_WAIT_TIMEOUT_MS 同量级。
  GracefulWaitTimeoutMs = 5000;
  // 兜底强杀的最大轮次；每轮重新枚举残留进程并复查单例互斥量。
  ForceKillMaxAttempts = 3;

function InitializeSetup(): Boolean;
begin
  // 刻意不在初始化阶段终止任何进程：此时用户尚未确认安装，
  // “打开看看再取消”不得产生任何破坏性后果；是否已有实例运行，
  // 交给 AppMutex 门控在启动时识别并提示用户自行关闭。
  Result := True;
end;

// 仅在非静默安装时更新向导状态栏；静默安装无向导可写。
procedure SetInstallStatus(const Msg: String);
begin
  if not WizardSilent() then
    WizardForm.StatusLabel.Caption := Msg;
end;

// 请求已安装实例优雅退出：调用安装目录下旧版 exe 的 --quit 入口，
// 由其看门狗窗口执行托盘清理与消息循环退出。全新安装（目录下尚无旧版）
// 或启动失败时直接返回，后续等待会自然通过。
procedure RequestGracefulExit;
var
  ExePath: String;
  ResultCode: Integer;
begin
  ExePath := ExpandConstant('{app}\traffic-monitor.exe');
  if not FileExists(ExePath) then
    Exit;
  SetInstallStatus('正在等待旧版本退出…');
  Exec(ExePath, '--quit', '', SW_HIDE, ewWaitUntilTerminated, ResultCode);
end;

// 轮询单例互斥量直到其消失（旧实例完全退出）或超时；
// 返回 True 表示可安全覆写文件。
function WaitForSingleInstanceGone(TimeoutMs: Integer): Boolean;
var
  Elapsed: Integer;
begin
  Elapsed := 0;
  while Elapsed < TimeoutMs do
  begin
    if not CheckForMutexes('TrafficMonitor_Mutex_Instance') then
    begin
      Result := True;
      Exit;
    end;
    Sleep(100);
    Elapsed := Elapsed + 100;
  end;
  Result := not CheckForMutexes('TrafficMonitor_Mutex_Instance');
end;

// 超时兜底：按进程名定位残留实例并逐 PID 强制终止，最多重复 ForceKillMaxAttempts 轮。
// 两处收窄缺一不可：不用映像名全杀以免误伤同名进程，
// 且以命令行排除更新子进程（--check-update）——它正是发出 EXIT_MAIN 后
// 等待主进程退出的协调者；刻意不用 /T 树杀，避免连带其子进程。
// 每轮后复查互斥量即为终止结果检查；若最终仍有残留则直接返回，
// 交由安装器原生文件占用提示处理，本过程不弹框。
procedure ForceKillRemnant;
var
  Attempt: Integer;
  ResultCode: Integer;
  KillCmd: String;
begin
  KillCmd :=
    '/C powershell -NoProfile -ExecutionPolicy Bypass -Command "Get-CimInstance Win32_Process ' +
    '| Where-Object { $_.Name -eq ''traffic-monitor.exe'' -and $_.CommandLine -notlike ''*--check-update*'' } ' +
    '| ForEach-Object { taskkill /F /PID $_.ProcessId }"';
  for Attempt := 1 to ForceKillMaxAttempts do
  begin
    SetInstallStatus('旧版本无响应，正在结束残留进程…');
    Exec(ExpandConstant('{cmd}'), KillCmd, '', SW_HIDE, ewWaitUntilTerminated, ResultCode);
    if WaitForSingleInstanceGone(1000) then
      Exit;
  end;
end;

procedure CurStepChanged(CurStep: TSetupStep);
begin
  // 终止逻辑只允许发生在用户确认安装、即将覆写文件的前一刻；
  // 在此之前取消安装不会触碰正在运行的实例。
  if CurStep = ssInstall then
  begin
    RequestGracefulExit;
    if WaitForSingleInstanceGone(GracefulWaitTimeoutMs) then
      Exit;
    ForceKillRemnant;
  end;
end;
