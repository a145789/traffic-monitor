# Traffic Monitor

Windows 11 任务栏小组件，纯 Rust，无配置文件。嵌入任务栏系统托盘左侧，双行文字展示 CPU、内存、网速。

> [!NOTE]
> **更新指南**：仅在引入新的**“容易改错的隐式设计约束”**或**“非直觉的验证发布命令”**时，方可修改此文档。禁止加入易变的代码数值常量。保持高信噪比。

## 核心开发约束与设计决策 (AI 必读防坑指南)

> [!IMPORTANT]
> 修改代码时必须遵循以下既定设计决策，切勿违背：

1. **窗口嵌入任务栏的顺序**
   - **设计决策**：[src/main.rs](src/main.rs) 中的 `embed_in_taskbar` 必须严格按照以下 Win32 API 顺序调用：
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
4. **更新检查 re-exec 子进程与 DLL 延迟加载**
   - **设计决策**：为避免 `winhttp.dll` / `bcrypt.dll` / `bcryptprimitives.dll`（及其连带依赖 `schannel` / `ncrypt` 等 TLS 栈）常驻主进程内存，[build.rs](build.rs) 通过 `/DELAYLOAD` 将这三个 DLL 移至延迟导入表；[src/update.rs](src/update.rs) 的更新检查逻辑改为 **re-exec 自身**（`traffic-monitor.exe --check-update`）在子进程中执行 HTTP 下载 + SHA-256 校验，结果通过 stdout 单行协议（`NO_UPDATE` / `PORTABLE|版本` / `INSTALLED|版本|路径` / `ERROR`）回传主进程。子进程退出后这些 DLL 随进程释放，主进程稳态零开销。
   - **隐式约束**：`--check-update` 参数拦截**必须在 [src/main.rs](src/main.rs) 的单例 Mutex 锁之前**执行，否则子进程会被当作重复实例直接退出。`/DELAYLOAD` 配置不可从 build.rs 中删除，否则 DLL 会回到标准导入表，re-exec 方案失去意义。

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

## 架构与职责 (9 个源文件)

所有的具体常量数值（如像素宽、高、定时器间隔、颜色等）均统定义在 [src/config.rs](src/config.rs) 中。AI 在修改或读取时应直接查阅该文件，避免在其他模块中硬编码。

| 文件                                 | 职责说明                                                                                |
| :----------------------------------- | :-------------------------------------------------------------------------------------- |
| [src/main.rs](src/main.rs)           | 窗口创建、UI 消息循环、任务栏嵌入、窗口位置动态更新、系统挂起/恢复处理、单例 Mutex 锁。 |
| [src/config.rs](src/config.rs)       | 全局常量定义、窗口与字体基准大小、定时器 ID、共享多线程无锁原子变量（Atomic）定义。     |
| [src/collector.rs](src/collector.rs) | CPU 与内存采集、网卡接口过滤、单网卡锁定算法、网络断开与恢复消息发送。                  |
| [src/renderer.rs](src/renderer.rs)   | GDI 双缓冲绘制（位图缓存 `hdc_mem` -> 窗口 `hdc`）、字体、DPI 缩放、文字排版与对齐。       |
| [src/tray.rs](src/tray.rs)           | 托盘图标生命周期维护、系统托盘右键菜单响应、开机自启写入与读取。                        |
| [src/update.rs](src/update.rs)       | 自动/手动检查更新、下载新版本安装包、SHA-256 安全哈希校验、UAC 提权覆盖安装。           |
| [src/thermal.rs](src/thermal.rs)     | 设备过热风险推断引擎：电池放电功率直测（拔电）/CPU·内存·内核比多信号推断（插电）、双 EMA 热容模拟、滞回状态机。 |
| [src/ffi_guard.rs](src/ffi_guard.rs) | RAII 资源守卫类型（`MutexGuard`、`RegKey`、`MenuGuard` 等），保证 FFI 资源安全释放。    |
| [src/util.rs](src/util.rs)           | UTF-16/字符串互转、Windows API MessageBox 弹窗封装、注册表快速读写。                    |
