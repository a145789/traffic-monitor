# Traffic Monitor

Windows 11 任务栏小组件，纯 Rust，无配置文件。嵌入任务栏系统托盘左侧，双行文字展示 CPU、内存、网速、鼠标电量/DPI。

> [!NOTE]
> **更新指南**：仅在引入新的**“容易改错的隐式设计约束”**或**“非直觉的验证发布命令”**时，方可修改此文档。禁止加入易变的代码数值常量。保持高信噪比。

## 核心开发约束与设计决策 (AI 必读防坑指南)

> [!IMPORTANT]
> 修改代码时必须遵循以下既定设计决策，切勿违背：

1. **鼠标 HID 轮询与 `HidApi` 实例化**
   - **设计决策**：`HidApi` 实例在 [src/mouse_hid.rs](src/mouse_hid.rs) 的线程循环中**按需临时创建，用完即丢**。因为轮询间隔极长（在线时 180s，离线时 300s），重建开销可忽略，且能避免 `hidapi` 库常驻内存。**不要**将其重构为静态长生命周期缓存。
   - **防坑**：任何启动新鼠标线程的操作前，必须先调用 `stop_and_join_mouse_thread()` 确保旧线程已 Join，防止线程残留或并发冲突。
2. **窗口嵌入任务栏的顺序**
   - **设计决策**：[src/main.rs](src/main.rs) 中的 `embed_in_taskbar` 必须严格按照以下 Win32 API 顺序调用：
     1. `SetParent(hwnd, h_taskbar)`（此操作会剥离 `WS_EX_LAYERED` 样式）
     2. `SetWindowLongPtrW(GWL_STYLE, WS_CHILD | WS_VISIBLE)`（直接覆盖样式）
     3. `SetWindowLongPtrW(GWL_EXSTYLE, ... | WS_EX_LAYERED)`（重新应用分层样式）
     4. `SetWindowPos`（更新位置与 Z 序）
     5. `SetLayeredWindowAttributes(hwnd, COLOR_KEY, 0, LWA_COLORKEY)`（重新应用透明 Key）
        **调换或遗漏步骤会导致分层透明失效或小组件被任务栏图标遮挡**。
3. **构建优化与编译 OOM 规避**
   - **设计决策**：由于依赖了庞大的 `windows` crate，在开启 `panic="abort"` 且 `codegen-units=1` 时编译 release 会导致内存 OOM。必须在 [Cargo.toml](Cargo.toml) 中为 `[profile.release.package.windows]` 单独配置较大的 `codegen-units`（如 8）以降低编译峰值内存。
4. **单物理网卡流量锁定**
   - **设计决策**：[src/collector.rs](src/collector.rs) 中的网速采集**不累加**所有网卡流量。每个周期独立计算各个 LUID 的流量变化，并在排除了虚拟网卡（通过 `GetAdaptersAddresses` 黑名单关键字过滤）后，选取**当前流量最大的一张单一物理网卡**锁定并展示，规避虚拟机、VPN 或回环网卡的流量干扰。

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
| [src/renderer.rs](src/renderer.rs)   | GDI 双缓冲绘制（位图缓存 `hdc_mem` -> 窗口 `hdc`）、字体及 DPI 适配、文字排版与对齐。   |
| [src/mouse_hid.rs](src/mouse_hid.rs) | HID 鼠标通信轮询线程，负责查询特定 VID/PID 鼠标的 DPI 和电量，支持中断式快速睡眠。      |
| [src/tray.rs](src/tray.rs)           | 托盘图标生命周期维护、系统托盘右键菜单响应、开机自启写入与读取。                        |
| [src/update.rs](src/update.rs)       | 自动/手动检查更新、下载新版本安装包、SHA-256 安全哈希校验、UAC 提权覆盖安装。           |
| [src/ffi_guard.rs](src/ffi_guard.rs) | RAII 资源守卫类型（`MutexGuard`、`RegKey`、`MenuGuard` 等），保证 FFI 资源安全释放。    |
| [src/util.rs](src/util.rs)           | UTF-16/字符串互转、Windows API MessageBox 弹窗封装、注册表快速读写。                    |
