# Traffic Monitor

Windows 11 任务栏小组件，纯 Rust，无配置文件。嵌入任务栏系统托盘左侧，双行文字展示 CPU、内存、网速。

> [!NOTE]
> **更新指南**：仅在引入新的**“容易改错的隐式设计约束”**或**“非直觉的验证发布命令”**时，方可修改此文档。禁止加入易变的代码数值常量。保持高信噪比。

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
3. **单物理网卡流量锁定**
   - **设计决策**：[src/collector.rs](src/collector.rs) 中的网速采集**不累加**所有网卡流量。每个周期独立计算各个 LUID 的流量变化，并在排除了虚拟网卡（通过 `GetAdaptersAddresses` 黑名单关键字过滤）后，选取**当前流量最大的一张单一物理网卡**锁定并展示，规避虚拟机、VPN 或回环网卡的流量干扰。
4. **更新功能完整进程隔离与 DLL 延迟加载**
   - **设计决策**：为避免网络、加密、UI、输入法和 Shell 相关 DLL 常驻主进程，[build.rs](build.rs) 通过 `/DELAYLOAD` 延迟导入 `winhttp.dll` / `bcrypt.dll` / `bcryptprimitives.dll`；[src/update/mod.rs](src/update/mod.rs) 通过 re-exec 自身创建短生命周期子进程，由子进程完整执行 HTTP 下载、SHA-256 校验、更新弹窗、打开网页及提权启动安装器。子进程仅通过 stdout 单行协议（`DONE` / `EXIT_MAIN`）通知主进程继续运行或退出，结束后由操作系统整体回收其 DLL 与内存。更新相关 `MessageBoxW` / `ShellExecuteW` / `ShellExecuteExW` 不得移回主进程。
   - **隐式约束**：`--check-update` 参数拦截**必须在 [src/main.rs](src/main.rs) 的单例 Mutex 锁之前**执行，否则子进程会被当作重复实例直接退出；手动检查必须额外传递 `--manual`，用于决定无更新或检查失败时是否提示。主进程必须在首个窗口创建前调用 `ImmDisableIME(u32::MAX)`，且托盘菜单必须先以 `TPM_RETURNCMD` 取得命令、把前台权交还任务栏后再执行命令，否则更新弹窗关闭后的焦点回落会在主进程中初始化第三方 TSF/IME。`/DELAYLOAD` 配置不可从 build.rs 中删除，否则网络与加密 DLL 会回到标准导入表，进程隔离失去意义。
5. **Explorer 重启与任务栏重建恢复机制**
   - **设计决策**：当资源管理器（Explorer.exe）重启时，任务栏被销毁重建。小组件必须注册并拦截全局广播的 `TaskbarCreated` 消息，并在回调中调用 `invalidate_taskbar_cache()` 清理窗口句柄缓存，重新执行 `embed_in_taskbar` 重置嵌入关系，同时必须重新创建托盘图标（`create_tray_icon`）并重置监测定时器。遗漏此处理将导致 Explorer 重启后组件永久消失。
6. **多显示器与 DPI 动态自适应 (WM_DPICHANGED)**
   - **设计决策**：小组件窗口必须响应 `WM_DPICHANGED` 消息。在 DPI 变动时，除需要通知 `Renderer::update_dpi` 重新计算缩放字体外，**必须**重新执行 `embed_in_taskbar` 以根据最新 DPI 动态重置小组件窗口的物理宽高度和位置，否则会导致高分屏/跨屏移动时组件物理大小不合或排版截断。
7. **挂起/全屏节能与定时器唤醒配对**
   - **设计决策**：为确保后台低功耗常驻，小组件在休眠（`PBT_APMSUSPEND`）、锁屏（`WTS_SESSION_LOCK`）或当前显示器运行全屏应用时会执行 `suspend_system` 销毁监测定时器并冻结计算；在唤醒、解锁或退出全屏时通过 `resume_system` 恢复。任何针对监测周期的修改均必须保证“销毁”与“恢复”操作在逻辑上完全对称，否则会导致唤醒后组件数据永久冻结（卡死假活）。
8. **RAII 句柄守卫归属规则**
   - **设计决策**：[src/ffi_guard.rs](src/ffi_guard.rs) 仅收口「裸句柄 → 单一 `Close*`/`Destroy*`」配对、无业务构造语义的通用守卫（当前为 `MutexGuard`、`MenuGuard`）。业务专属守卫（`Renderer` 的事务式 `ScreenDcGuard`/`OwnedDc`/`OwnedBitmap`/`OwnedFont`/`OwnedBrush`、`update` 的 `WinHttpHandles`/`BcryptHandles`、`collector` 的 `MibTable`）**保留在各自业务文件**，因为它们的创建/选入/释放顺序与该模块的不变量强耦合。新增守卫时按此归属规则选择位置，禁止为追求「集中」而割裂业务上下文。
9. **代码风格（易被后续改动打散）**
   - **用户可见字符串一律中文**（MessageBox、托盘菜单、初始化错误信息）。
   - **业务宽字符串用 `util::to_wide`**；`config` 中已含尾 `\0` 的常量直接 `encode_utf16().collect()`。
   - **生产路径禁止 `unwrap`/`expect`**（测试与 mutex poison 除外）；失败用 `Result`/`Option` 或早退。
   - **`unsafe` 旁注只写不变量 / 失败歧义 / 内存布局**，禁止复述“句柄有效因为 OS 给了我们”。
   - **原子序约定**见 [src/state.rs](src/state.rs) 模块头与各字段注释（Relaxed 展示/开关；Acq/Rel 跨线程握手）。

---

## 构建与发布

### 本地构建与调试

```bash
cargo build --release 2>&1     # 构建并检查警告
Start-Process "target\release\traffic-monitor.exe" -WindowStyle Hidden # 后台启动
Stop-Process -Name "traffic-monitor" -Force # 强退旧进程
```

### 格式化与静态检查校验

在任何代码修改后，提交前必须执行并确保无任何警告或错误：

```bash
cargo test 2>&1   # 验证测试用例通过
cargo build --release 2>&1      # 验证构建无警告
cargo clippy -- -D warnings     # 验证 Clippy 无警告
cargo fmt                       # 格式化代码
```

_注：如果修改涉及到 `unsafe` 代码，必须严格符合 [docs/unsafe-code-policy.md](docs/unsafe-code-policy.md) 中规定的安全要求。上述构建、Clippy 和格式化校验仅在修改了 Rust 相关的源码文件时才需要执行。_

### 安装包与发布

项目使用 Inno Setup 7 打包（安装脚本为 [installer.iss](installer.iss)）。

```bash
bun scripts/release.ts 0.5.2   # 更新版本号 → 编译 → git tag → GitHub Release (CI 打包)
bun scripts/package.ts         # 本地编译并构建打包（版本号取自 Cargo.toml）
bun scripts/package.ts dev     # 生成带 dev 后缀的时间戳补丁版本号并打包
```

---

## 架构与职责 (14 个源文件)

所有的具体常量数值（如像素宽、高、定时器间隔、颜色等）均统定义在 [src/config.rs](src/config.rs) 中。AI 在修改或读取时应直接查阅该文件，避免在其他模块中硬编码。

| 文件                                 | 职责说明                                                                                                        |
| :----------------------------------- | :-------------------------------------------------------------------------------------------------------------- |
| [src/main.rs](src/main.rs)           | 窗口创建、UI 消息循环、单例 Mutex 锁。                                                                          |
| [src/config.rs](src/config.rs)       | 全局常量定义、窗口与字体基准大小、定时器 ID 等配置。                                                            |
| [src/state.rs](src/state.rs)         | 共享多线程无锁原子变量（Atomic）定义与运行时全局状态。                                                          |
| [src/window.rs](src/window.rs)       | 任务栏窗口查找、嵌入位置计算、任务栏嵌入以及窗口位置动态更新。                                                  |
| [src/suspend.rs](src/suspend.rs)     | 系统挂起/恢复处理、全屏检测、Windows 主题（深浅色）变更检测。                                                   |
| [src/collector.rs](src/collector.rs) | CPU 与内存采集、网卡接口过滤、单网卡锁定算法、网络断开与恢复消息发送。                                          |
| [src/renderer.rs](src/renderer.rs)   | GDI 双缓冲绘制（位图缓存 `hdc_mem` -> 窗口 `hdc`）、字体、DPI 缩放、文字排版与对齐。                            |
| [src/tray.rs](src/tray.rs)           | 托盘图标生命周期维护、系统托盘右键菜单响应、开机自启写入与读取。                                                |
| [src/update/mod.rs](src/update/mod.rs) | 自动/手动检查更新业务编排、子进程协议、安装器启动、注册表开关读写。                                            |
| [src/update/version.rs](src/update/version.rs) | 版本号解析与远端 metadata 严格解析（纯字符串/字节处理，无 I/O，支持单测），判断目录类型。                     |
| [src/update/http.rs](src/update/http.rs) | WinHTTP 网络数据抓取与友好的中文错误映射。                                                                     |
| [src/update/crypto.rs](src/update/crypto.rs) | BCrypt SHA-256 哈希计算与 RAII 句柄安全守卫。                                                                  |
| [src/ffi_guard.rs](src/ffi_guard.rs) | 跨模块复用的通用 Win32 句柄 RAII 守卫（`MutexGuard`、`MenuGuard`）。业务专属守卫留在各自业务文件。              |
| [src/util.rs](src/util.rs)           | UTF-16/字符串互转、Windows API MessageBox 弹窗封装、注册表快速读写。                                            |
