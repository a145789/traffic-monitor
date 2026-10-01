[Setup]
AppName=Traffic Monitor
AppVersion=1.7.4
AppPublisher=Traffic Monitor
AppMutex=TrafficMonitor_Mutex_Instance
DefaultDirName={autopf}\Traffic Monitor
DefaultGroupName=Traffic Monitor
; 安装包名派生自本节 AppVersion（更新器按 TrafficMonitor-Setup-<version>.exe 拼下载地址）；
; 只可用 {#SetupSetting("AppVersion")}——{#AppVersion} 不是 ISPP 变量、编译直接失败，
; 且 SetupSetting 只读本行之上已解析的指令，故本行须留在 AppVersion 之后，倒序会静默产出 TrafficMonitor-Setup-.exe。
OutputBaseFilename=TrafficMonitor-Setup-{#SetupSetting("AppVersion")}
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
; 兜底强杀筛选器：dontcopy = 不随安装复制到 {app}，只在 ForceKillRemnant 里用
; ExtractTemporaryFile 释放到 {tmp} 后以 -File 调用（见 [Code] 的 ForceKillRemnant）。
Source: "installer\kill-remnant.ps1"; Flags: dontcopy

[Icons]
Name: "{group}\Traffic Monitor"; Filename: "{app}\traffic-monitor.exe"
Name: "{group}\Uninstall Traffic Monitor"; Filename: "{uninstallexe}"
Name: "{autodesktop}\Traffic Monitor"; Filename: "{app}\traffic-monitor.exe"; Tasks: desktopicon

[Languages]
Name: "chinesesimp"; MessagesFile: "compiler:Languages\ChineseSimplified.isl"

[Tasks]
Name: "desktopicon"; Description: "创建桌面快捷方式"; GroupDescription: "附加任务:"
Name: "startup"; Description: "开机自动启动"; GroupDescription: "启动选项:"; Flags: checkedonce

[Registry]
Root: HKCU; Subkey: "Software\Microsoft\Windows\CurrentVersion\Run"; ValueType: string; ValueName: "TrafficMonitor"; ValueData: """{app}\traffic-monitor.exe"""; Flags: uninsdeletevalue; Tasks: startup

[Run]
; 拉起点按安装结果分派，任何时候只有一个真正生效，且身份都是非提权的交互用户：
; - 安装成功收尾：由本条目拉起（向导路径给出 postinstall 复选框 UX；静默路径没有
;   Finished 页，条目照常处理，因此手动静默安装/升级也会自动启动组件）。显式写出
;   runasoriginaluser 把意图钉住（它本就是 postinstall 的默认身份，但只有在 SetupLdr
;   有机会以原始凭据跑过代码时才生效——更新器已改用默认动词启动安装器，见
;   src/update/installer.rs）。
; - 安装器起来了却没能成功收尾：本条目按 Inno 语义根本不执行，改由更新子进程在确认
;   组件不在跑之后补拉起（src/update/mod.rs；判据是单例互斥量，不是退出码）。
; 刻意不写 skipifsilent：那会让手动静默安装/升级也失去唯一的拉起者——临时的更新子进程
; 只存在于自动更新路径——正好是「装完之后组件没了」要消灭的现场。
Filename: "{app}\traffic-monitor.exe"; \
Description: "启动 Traffic Monitor"; \
Flags: nowait postinstall runasoriginaluser

[Code]
// 安装交接实行“先礼后兵”：拷贝阶段前先请求旧实例优雅退出并等待其消失，
// 随后无条件对残留实例做有限次强制终止；强杀永远是兜底而非首选。
// 顺序证据：拷贝阶段前“优雅退出等待→无条件强杀”——CurStepChanged(ssInstall)
// 依次调用 RequestGracefulExit、WaitForSingleInstanceGone、ForceKillRemnant。
// 强杀必须无条件执行而不能以“互斥量仍在”为门：单例互斥量消失只证明主实例退出，
// 不证明 {app} 下的 exe 映像已无人占用——旧版更新协调者（--check-update 子进程）
// 刻意不持有单例互斥量，却与主程序共用同一个 exe 映像，它若在复制阶段仍存活，
// 覆写主程序必然失败（v1.7.1→v1.7.2 升级失败的根因）。新版协调者在把交接移交
// 给临时副本后早已退出，这一步对它是空操作。
// 知情代价：无条件执行意味着每次安装/升级（含全新安装）都多跑一轮强杀脚本
// （无残留时是纯枚举空跑，约 0.5–2 秒）；这是为覆盖旧版协调者残留支付的固定成本。
const
  // 优雅退出等待上限（毫秒）：与 src/config.rs 的 MAIN_EXIT_WAIT_TIMEOUT_MS 同值同源语义，
  // 两处 Rust 等待（main.rs 的 quit_existing_instance、update 的 wait_main_instance_gone）
  // 均以此为上限，超时后由下方 ForceKillRemnant / 安装器内 taskkill 兜底。
  GracefulWaitTimeoutMs = 5000;
  // 兜底强杀的最大轮次；每轮重新枚举残留进程并复查单例互斥量。
  ForceKillMaxAttempts = 3;
  // 兜底强杀筛选器的包内文件名：由 [Files] 的 dontcopy 项提供，
  // ForceKillRemnant 用 ExtractTemporaryFile 释放到 {tmp} 后以 -File 调用。
  KillRemnantScript = 'kill-remnant.ps1';

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

// 超时兜底：调用外置筛选器（installer\kill-remnant.ps1）终止残留实例，最多重复
// ForceKillMaxAttempts 轮。按进程名筛选仍覆盖全机器所有同名进程，真正的收窄来自
// 脚本里的三个必要条件：可执行文件完整路径等于本次升级的 {app}、进程会话等于安装器
// 所在会话、命令行取得到（命令行内容不参与筛选——旧版更新协调者不持单例互斥量却
// 占用着待覆写的 exe 映像，必须在复制阶段前一并清掉，见模块头说明）。
// 本过程只做「少杀」，不做跨会话清理——异会话残留退回安装器原生文件占用提示。
// 刻意不用 /T 树杀，避免连带其子进程。
// 脚本路径与 -Expected 只作为参数传给 -File，不拼进 PowerShell 源码，避免引号、
// %FOO%、中文、8.3 短路径的多层转义。每轮的完成判定来自脚本退出码：脚本强杀后
// 重新枚举自检，仍有匹配残留即非零退出——单例互斥量在这里当不了完成信号（旧版
// 协调者不持互斥量却占着映像，互斥量消失不代表可以复制）。轮次耗尽仍非零也照常
// 放行，交由安装器原生文件占用提示处理，本过程不弹框。
procedure ForceKillRemnant;
var
  Attempt: Integer;
  ResultCode: Integer;
  KillCmd: String;
begin
  // dontcopy 的文件不随安装复制：必须先释放到 {tmp} 才能以 -File 调用。
  ExtractTemporaryFile(KillRemnantScript);
  KillCmd :=
    '/C powershell -NoProfile -ExecutionPolicy Bypass -File "' +
    ExpandConstant('{tmp}\' + KillRemnantScript) + '" -Expected ' +
    AddQuotes(ExpandConstant('{app}\traffic-monitor.exe'));
  for Attempt := 1 to ForceKillMaxAttempts do
  begin
    SetInstallStatus('旧版本无响应，正在结束残留进程…');
    Exec(ExpandConstant('{cmd}'), KillCmd, '', SW_HIDE, ewWaitUntilTerminated, ResultCode);
    if ResultCode = 0 then
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
    // 返回值必须被使用（也顺手把超时告知向导）：优雅等待超时说明旧实例没退干净，
    // 接下来的强杀轮次是真正在做事，不是空跑。
    if not WaitForSingleInstanceGone(GracefulWaitTimeoutMs) then
      SetInstallStatus('旧版本未响应，正在结束残留进程…');
    ForceKillRemnant;
  end;
end;
