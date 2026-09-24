# Traffic Monitor

Windows 11 任务栏小组件，纯 Rust，无配置文件。嵌入任务栏系统托盘左侧，双行文字展示 CPU、内存、网速。

> [!NOTE]
> **更新指南**：仅在引入新的**“容易改错的隐式设计约束”**或**“非直觉的验证发布命令”**时，方可修改此文档。禁止加入易变的代码数值常量。保持高信噪比。

## 文档阅读策略（AI）

> [!IMPORTANT]
> **默认禁止阅读 `docs/`**（含 `docs/archive/`）。其中为历史 RFC、审计、研究笔记与旧实现说明，**可能过时**，不得当作现行约束。
>
> **例外：`docs/rfc/` 可以读、且应读**。它是现行提案目录（未实施的待办 + 刚实施完的笔记），不是历史备份，不过时。约定见 [docs/rfc/README.md](docs/rfc/README.md)。查待办：`grep -rn "^Status: *proposed" docs/rfc/`。

**何时可以读 archive**：用户明确给出路径/文件名，或明确说「查 docs / 查 archive / 看 RFC / 看审计 / 按 unsafe policy」等。  
**禁止**：为「更全面了解项目」而自行打开 archive；现行不变量以本文件 + 源码为准。

## 核心开发约束与设计决策 (AI 必读防坑指南)

> [!IMPORTANT]
> 修改代码时必须遵循以下既定设计决策，切勿违背：

1. **窗口嵌入任务栏的顺序**
   - **设计决策**：[src/window.rs](src/window.rs) 中的 `embed_in_taskbar` 必须严格按照以下 Win32 API 顺序调用：
     1. `SetParent(hwnd, h_taskbar)`（此操作会剥离 `WS_EX_LAYERED` 样式）
     2. `SetWindowLongPtrW(GWL_STYLE, WS_CHILD | WS_VISIBLE)`（直接覆盖样式）
     3. `SetWindowLongPtrW(GWL_EXSTYLE, ... | WS_EX_LAYERED)`（重新应用分层样式）
     4. `SetWindowPos`（更新位置与 Z 序）
     5. `SetLayeredWindowAttributes(hwnd, COLOR_KEY, 0, LWA_COLORKEY)`（重新应用透明 Key）
        **调换或遗漏步骤会导致分层透明失效或小组件被任务栏图标遮挡**。
2. **构建优化与编译 OOM 规避**
   - **设计决策**：由于依赖了庞大的 `windows` crate，在开启 `panic="abort"` 且 `codegen-units=1` 时编译 release 会导致内存 OOM。必须在 [Cargo.toml](Cargo.toml) 中为 `[profile.release.package.windows]` 单独配置较大的 `codegen-units`（如 8）以降低编译峰值内存。
3. **单物理网卡流量选择（每周期独立择大）**
   - **设计决策**：[src/collector/network.rs](src/collector/network.rs) 中的网速采集**不累加**所有网卡流量。每个周期独立计算各个 LUID 的流量变化，并在排除了虚拟网卡（通过 `GetAdaptersAddresses` 黑名单关键字过滤）后，选取**当前周期流量最大的一张单一物理网卡**展示：单卡速率、不跨周期粘滞、不累加多卡，规避虚拟机、VPN 或回环网卡的流量干扰。双网卡同时活跃时显示逐秒择大的一张卡，这是既有产品语义（加粘滞会引入切换延迟感，见 RFC 04 决策项 5，裁定为澄清措辞、不加粘滞）。
4. **更新功能完整进程隔离与 DLL 延迟加载**
   - **设计决策**：为避免网络、加密相关 DLL 因更新代码常驻主进程，[build.rs](build.rs) 通过 `/DELAYLOAD` 延迟导入 `winhttp.dll` / `bcrypt.dll` / `bcryptprimitives.dll`；[src/update/protocol.rs](src/update/protocol.rs) 通过 re-exec 自身创建短生命周期子进程，由子进程完整执行 HTTP 下载、SHA-256 校验、更新确认弹窗、打开网页及提权启动安装器（安装器管线在 [src/update/installer.rs](src/update/installer.rs)：安装包缓存复用、流式下载校验、主进程退出等待）。主进程仍保留窗口、托盘和基础错误提示所需的 UI API；子进程退出后由操作系统回收其 DLL 与内存。更新流程专属的 `MessageBoxW` / `ShellExecuteW` / `ShellExecuteExW` 不得移回主进程。
   - **隐式约束**：`--check-update` 参数拦截**必须在 [src/main.rs](src/main.rs) 的单例 Mutex 锁之前**执行，否则子进程会被当作重复实例直接退出；手动检查必须额外传递 `--manual`，用于决定无更新或检查失败时是否提示。`EXIT_MAIN` 发出后，子进程必须轮询等待单实例互斥量消失（主进程完全退出）才允许 `ShellExecuteExW` 启动安装器，安装器内的 taskkill 仅是兜底而非主要退出机制；启动失败/UAC 取消时由子进程负责重新拉起主程序，并携带 `--relaunched-by-update` 参数推迟首个自动检查冷却周期（避免立刻再弹同一版本的确认框）。安装包是否可信的**唯一裁决是锁定句柄的重算哈希**（缓存复用与下载后重验共用同一只读共享锁句柄），禁止改回「按路径另开文件验证」——那会重新打开 TOCTOU 窗口。更新交接消息（`WM_USER_UPDATE_ACTION`）的落点是**看门狗窗口**：主窗口会在 Explorer 重建中被替换，快照它的句柄必然丢消息，`UPDATE_IN_PROGRESS` 将被永久占位挡死后续检查。主进程必须在首个窗口创建前调用 `ImmDisableIME(u32::MAX)`，且托盘菜单必须先以 `TPM_RETURNCMD` 取得命令、把前台权交还任务栏后再执行命令，否则更新弹窗关闭后的焦点回落会在主进程中初始化第三方 TSF/IME。`/DELAYLOAD` 配置不可从 build.rs 中删除，否则网络与加密 DLL 会回到标准导入表，进程隔离失去意义。
5. **Explorer 重启与任务栏重建恢复机制**
   - **设计决策**：主窗口 `SetParent` 进任务栏后成为跨进程子窗口，explorer.exe 销毁任务栏时会被 OS 级联销毁；且 `TaskbarCreated` 广播（HWND_BROADCAST）只投递顶层窗口。因此主窗口自身**永远收不到**该消息，必须维持一个**永不嵌入、保持隐藏的顶层看门狗窗口**（[src/window.rs](src/window.rs) 的 `create_watchdog_window`，类名见 `WATCHDOG_CLASS`），由其窗口过程接收 `TaskbarCreated` 并调用 `rebuild_main_window()` 完整重建主窗口，同时把电源/会话通知、托盘图标、监测定时器逐一重绑到新 hwnd（网络采样由 WM_TIMER tick 携带的 hwnd 直接投递，无需重绑）。禁止把 TaskbarCreated 处理挂回主窗口过程，禁止让看门狗窗口参与嵌入或显示。看门狗同时是 `--quit` 退出请求（`WM_USER_QUIT_REQUEST`，`FindWindowW` 按 `WATCHDOG_CLASS` 检索——主窗口嵌入后是子窗口，顶层检索永远 miss）、更新交接与主题广播的稳定落点。
   - **隐式约束（易被改错）**：`TaskbarCreated` 每次任务栏创建**只广播一次**，重建那一刻嵌入失败不会再有第二轮广播，故嵌入必须有周期兜底：`embed_in_taskbar` 返回 `Result` 且**禁止在内部弹框**（首轮与重试复用同一函数，弹框会退化成模态弹窗风暴），由 `reembed_if_lost` 静默重试到成功为止，且**必须挂在每个非挂起状态都存在的 `TIMER_ID_FULLSCREEN` 上**——改挂 `TIMER_ID_NETWORK` 会在全屏时失效，恰是最需要修复的场景。主窗口 `create_main_window` 重建失败同样只有一次广播机会，须在看门狗窗口上用 `TIMER_ID_REBUILD_RETRY` 退避重试（间隔翻倍至上限）直到成功，仅失败序列首次提示用户。`EMBEDDED` 是"整条嵌入序列成功"的单点真值（`create_main_window` 与每次嵌入尝试开头清位），`update_taskbar_position` 必须据此门控：未嵌入时按任务栏客户区坐标 `SetWindowPos` 会把面板钉到与任务栏无关的位置。会话通知则按 WTS 契约要求**先 `WTSUnRegisterSessionNotification` 再 `DestroyWindow`**（句柄经 `SESSION_NOTIFY_HWND` 传递），窗口销毁后补做无效。
6. **多显示器与 DPI 动态自适应 (WM_DPICHANGED)**
   - **设计决策**：小组件窗口必须响应 `WM_DPICHANGED` 消息。在 DPI 变动时，除需要通知 `Renderer::update_dpi` 重新计算缩放字体外，**必须**重新执行 `embed_in_taskbar` 以根据最新 DPI 动态重置小组件窗口的物理宽高度和位置，否则会导致高分屏/跨屏移动时组件物理大小不合或排版截断。
7. **挂起/全屏节能与定时器唤醒配对**
   - **设计决策**：为确保后台低功耗常驻，小组件在休眠（`PBT_APMSUSPEND`）、锁屏（`WTS_SESSION_LOCK`）、显示器关闭（`GUID_MONITOR_POWER_ON` 电源设置通知）或当前显示器运行全屏应用时会执行 `suspend_system` 销毁监测定时器并冻结计算；在唤醒、解锁、显示器开启或退出全屏时通过 `resume_system` 恢复，并在「从暂停回到运行」的边沿上重建网络/CPU 差分基线（暂停原因位集逐个清除时，只有清掉最后一位的那次恢复才重建）。任何针对监测周期的修改均必须保证“销毁”与“恢复”操作在逻辑上完全对称，否则会导致唤醒后组件数据永久冻结（卡死假活）。
8. **RAII 句柄守卫归属规则**
   - **设计决策**：[src/ffi_guard.rs](src/ffi_guard.rs) 仅收口「裸句柄 → 单一 `Close*`/`Destroy*`」配对、无业务构造语义的通用守卫（当前为 `MutexGuard`、`MenuGuard`）。业务专属守卫（`Renderer` 的事务式 `ScreenDcGuard`/泛型 `OwnedGdi<T>`、`update` 的 `WinHttpHandles`/`BcryptHandles`、`collector` 的 `MibTable`）**保留在各自业务文件**，因为它们的创建/选入/释放顺序与该模块的不变量强耦合。新增守卫时按此归属规则选择位置，禁止为追求「集中」而割裂业务上下文。
9. **代码风格（易被后续改动打散）**
   - **用户可见字符串一律中文**（MessageBox、托盘菜单、初始化错误信息）。
   - **业务宽字符串用 `util::to_wide`**；`config` 中已含尾 `\0` 的常量直接 `encode_utf16().collect()`。
   - **生产路径禁止 `unwrap`/`expect`**（测试与 mutex poison 除外）；失败用 `Result`/`Option` 或早退。
   - **`unsafe` 旁注只写不变量 / 失败歧义 / 内存布局**，禁止复述“句柄有效因为 OS 给了我们”。
   - **原子序约定**见 [src/state.rs](src/state.rs) 模块头与各字段注释（Relaxed 展示/开关；Acq/Rel 跨线程握手）。
10. **跨消息/跨线程事实的单点真值源**
   - **设计决策**：新增跨消息/跨线程事实时，必须指认唯一真值源并写明竞争处理（谁写、谁读、何时置位/清位、并发到达如何收敛）；同一事实禁止散落到多个可写位置。注释范式见 `EMBEDDED`、`EXIT_REQUESTED`、`UPDATE_IN_PROGRESS`。

---

## 构建与发布

### 本地构建与调试

```bash
cargo build --release --locked 2>&1     # 构建并检查警告
Start-Process "target\release\traffic-monitor.exe" -WindowStyle Hidden # 后台启动
Stop-Process -Name "traffic-monitor" -Force # 强退旧进程
```

### 格式化与静态检查校验

在任何代码修改后，提交前必须执行并确保无任何警告或错误：

```bash
cargo test --locked 2>&1        # 验证测试用例通过
cargo build --release --locked 2>&1   # 验证构建无警告
cargo clippy --all-targets --locked -- -D warnings   # 验证 Clippy 无警告；--all-targets 不可省，否则漏查 test/bench target 而 CI 红
cargo fmt                       # 格式化代码
```

与 CI（`check.yml`）对齐：三条 cargo 命令都必须带 `--locked`，缺了会静默改写 `Cargo.lock` 而本地绿、CI 红；`cargo fmt` 是就地格式化，等价于 CI 的 `cargo fmt -- --check`。**改了 `Cargo.toml` 必须把 `Cargo.lock` 一并提交**。`rust-version` 钉在 1.95（MSRV，刻意滞后于 stable 的档位），CI 另有 msrv job 用 1.95 工具链跑 `cargo check --locked` 对账：禁止使用超过 MSRV 的语言特性（如 1.96 才稳定的 `assert_matches!`），除非同步升 `rust-version` 并让两个 job 都绿。提级口径：`Cargo.toml`、check.yml 的注释与缓存键与工具链参数、rust-setup action 的 input 描述、本句，六处文本同步改完再提。

_注：`unsafe` 须遵守本文件第 9 节风格约束；完整历史 policy 在 `docs/archive/unsafe-code-policy.md`，**仅当用户要求时再读**。上述构建、Clippy 和格式化校验仅在修改了 Rust 相关的源码文件时才需要执行。CI（check.yml）使用 `dtolnay/rust-toolchain@stable` 即**最新 stable** 工具链，本地工具链落后时 clippy 可能通过而 CI 挂在新 lint 上；提交前若跨过 Rust 小版本，建议 `rustup update stable` 后复跑 clippy。_

### 安装包与发布

项目使用 Inno Setup 7 打包（安装脚本为 [installer.iss](installer.iss)）。安装包文件名由 `AppVersion` 派生（`OutputBaseFilename=TrafficMonitor-Setup-{#SetupSetting("AppVersion")}`），更新器按 `TrafficMonitor-Setup-<version>.exe` 拼下载地址——`Cargo.toml` 的 `version`、`installer.iss` 的 `AppVersion` 与发布 tag 三方必须一致，release 脚本有前置校验。

```bash
bun scripts/release.ts 0.5.2   # 门禁校验（三方版本一致）→ 改版本号 → git tag → 推送后 CI 打包
bun scripts/package.ts         # 本地编译并构建打包（版本号取自 Cargo.toml）
bun scripts/package.ts dev     # 生成带 dev 后缀的时间戳补丁版本号并打包
```

---

## 架构与职责 (20 个源文件)

所有的具体常量数值（如像素宽、高、定时器间隔、颜色等）均统一定义在 [src/config.rs](src/config.rs) 中。AI 在修改或读取时应直接查阅该文件，避免在其他模块中硬编码。

| 文件                                             | 职责说明                                                                                           |
| :----------------------------------------------- | :------------------------------------------------------------------------------------------------- |
| [src/main.rs](src/main.rs)                       | 启动编排、主窗口/看门狗窗口过程与消息路由、退出序列与重建重试、单例 Mutex 锁。                     |
| [src/config.rs](src/config.rs)                   | 全局常量定义（窗口/布局基准、定时器 ID 与间隔、自定义消息号、菜单 ID 等）。                        |
| [src/state.rs](src/state.rs)                     | 共享多线程无锁原子变量（Atomic）与 `SuspendReasons` 暂停位集 API。                                 |
| [src/window.rs](src/window.rs)                   | 窗口类注册与主窗口/看门狗窗口创建、任务栏查找、嵌入位置计算与动态更新。                            |
| [src/suspend.rs](src/suspend.rs)                 | 系统挂起/恢复、全屏检测、主题变更检测、电源/锁屏消息处理。                                         |
| [src/collector/mod.rs](src/collector/mod.rs)     | 采集子模块汇总与再导出。                                                                           |
| [src/collector/cpu_mem.rs](src/collector/cpu_mem.rs) | CPU 与内存使用率采集。                                                                         |
| [src/collector/network.rs](src/collector/network.rs) | 网卡接口过滤、虚拟网卡黑名单缓存、单网卡流量采样、断网/恢复消息发送。                        |
| [src/collector/rate.rs](src/collector/rate.rs)   | 速率归一化与最大流量单网卡选择（纯函数，无 I/O，支持单测）。                                       |
| [src/renderer.rs](src/renderer.rs)               | GDI 双缓冲绘制（位图缓存 `hdc_mem` -> 窗口 `hdc`）、字体、DPI 缩放、文字排版；实例由模块内 thread_local 托管。 |
| [src/tray.rs](src/tray.rs)                       | 托盘图标生命周期维护、右键菜单、开机自启写入与读取。                                               |
| [src/update/mod.rs](src/update/mod.rs)           | 自动/手动检查更新业务编排、用户交互与被拒版本记忆、安装版目录判定、注册表开关读写。                |
| [src/update/version.rs](src/update/version.rs)   | 版本号解析与远端 metadata 严格解析（纯字符串/字节处理，无 I/O，支持单测）。                        |
| [src/update/http.rs](src/update/http.rs)         | WinHTTP 网络数据抓取（元数据整包 / 安装包流式写盘）与友好的中文错误映射。                          |
| [src/update/crypto.rs](src/update/crypto.rs)     | BCrypt SHA-256 增量哈希（`Sha256`）与 RAII 句柄安全守卫。                                          |
| [src/update/cache.rs](src/update/cache.rs)       | 临时安装包缓存：路径、只读共享锁打开/创建、启动期过期清理。                                        |
| [src/update/protocol.rs](src/update/protocol.rs) | 子进程 re-exec 与 stdout 单行协议（`DONE`/`EXIT_MAIN`）扫描、更新交接转发看门狗、进行中标志收尾。  |
| [src/update/installer.rs](src/update/installer.rs) | 安装器管线：安装包缓存复用、流式下载与锁定句柄哈希校验、提权启动（瞬态错误重试）、等待主进程退出与重新拉起。 |
| [src/ffi_guard.rs](src/ffi_guard.rs)             | 跨模块复用的通用 Win32 句柄 RAII 守卫（`MutexGuard`、`MenuGuard`）。业务专属守卫留在各自业务文件。 |
| [src/util.rs](src/util.rs)                       | UTF-16 转换、MessageBox 封装、注册表读写、调试日志（`log_event!`）、HWND/电源通知句柄原子存储、进程内存优先级、工作集修剪。 |
