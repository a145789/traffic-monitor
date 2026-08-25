# RFC 0008: 删除死面与手写 ABI 副本

- **状态**: Draft (草案)
- **创建时间**: 2026-08-25
- **更新记录**:
  - 2026-08-25: 初稿。由全仓简化审计拆分而来（本 RFC 覆盖「删除无用表面 / 换用已存在等价定义」类候选；结构收敛类候选见 RFC 0009 与 RFC 0010）
- **关联文件**: `src/update/http.rs`, `src/update/mod.rs`, `src/suspend.rs`, `src/collector/cpu_mem.rs`, `src/main.rs`, `src/renderer.rs`

---

## 1. 背景与现状 (Context)

2026-08-25 对全仓库做了消费方证据驱动的简化审计（逐符号用 `rg` 区分生产消费方与测试/文档消费方，并对「手写代码是否已有外部等价物」直接核对了本地 cargo registry 中 `windows 0.62.2` 源码）。本 RFC 只收编**零行为变化、无新依赖**的删除/替换类候选：死参数、死返回值、死字段、手写 ABI 副本。所有改动互相独立，可与 RFC 0009 并行落地、各自成 PR。

## 2. 核心重构项

### 2.1 `fetch_url` 的 `secure` 参数是死分支

**现状**：`src/update/http.rs:62-67` 的 `fetch_url(host, path, secure, max_response_bytes)` 带 `secure: bool` 形参，据此在 106-110 行分支选择 HTTP/HTTPS 端口、在 137-141 行分支 `WINHTTP_FLAG_SECURE`。全部 4 个调用点（`src/update/mod.rs` 248、252、319、323 行）恒传 `true`；`secure=false` 的明文 HTTP 分支无任何生产消费方，也无测试覆盖。

**方案**：删除 `secure` 形参，端口固定 `INTERNET_DEFAULT_HTTPS_PORT`、flag 固定 `WINHTTP_FLAG_SECURE`；4 个调用点同步改三参。

**放弃什么**：将来请求明文 HTTP 源需加回参数。仓库唯一远端是 `github.com` 与 `ghproxy.cn`，均为 HTTPS，无现实需求。

### 2.2 suspend.rs 手写 ABI 副本换用 crate 定义

**现状**：`src/suspend.rs` 30-31 行手写 `const WTS_SESSION_LOCK: usize = 0x7 / WTS_SESSION_UNLOCK: usize = 0x8`；33-39 行手写 `#[repr(C)] #[allow(non_snake_case)] struct POWERBROADCAST_SETTING`。已核实 `windows 0.62.2` 均已提供（所需 feature `Win32_System_Power`、`Win32_UI_WindowsAndMessaging` 已在 Cargo.toml 启用）：

- `windows::Win32::System::Power::POWERBROADCAST_SETTING`：字段布局完全一致（`PowerSetting: GUID`、`DataLength: u32`、`Data: [u8; 1]`），见 registry 源 `Windows/Win32/System/Power/mod.rs` 1245-1251 行。
- `windows::Win32::UI::WindowsAndMessaging::WTS_SESSION_LOCK = 7u32 / WTS_SESSION_UNLOCK = 8u32`，见 `Windows/Win32/UI/WindowsAndMessaging/mod.rs` 7314/7319 行。

**方案**：删除本地结构体与两个常量，`use` crate 定义；`handle_session_change` 的 `match wparam.0 as u32` 适配 crate 的 u32 常量。这是 RFC 0007 第 2.2 节「手写 ABI 副本换官方投影」思路的延续收尾。

**放弃什么**：无。手写 ABI 布局与 OS 头文件存在漂移风险（crate 升级会同步修正，本地副本不会）。

### 2.3 `collect_cpu() -> bool` 返回值零消费方

**现状**：`src/collector/cpu_mem.rs:17` 的 `pub fn collect_cpu() -> bool`，唯一调用点 `src/main.rs:391` 为 `let _ = collect_cpu();` 显式丢弃；模块内无测试消费它；文档注释声明的「返回 true 表示本周期得到有效差分」语义没有任何读者。

**方案**：返回类型改为 `()`，注释改为内部行为说明；内部逻辑不动。

**放弃什么**：将来若要在 UI 呈现「采样失败」状态需加回（成本一行）。

### 2.4 `Renderer.font_size` 字段只写不读

**现状**：`src/renderer.rs` 的 `Renderer.font_size` 字段（71 行声明、273 行初始化、475 行赋值）全文件零读取：`update_dpi` 内的同名局部变量（436、455 行）自产自销，字体实际尺寸完全由局部变量传入 `create_font(font_size)` 决定；字段值恒等于 `FONT_BASE_SIZE × DPI 缩放`，可由 `width` 反推，无调试价值。

**方案**：删除字段声明与两处赋值。

**放弃什么**：无。

### 2.5 顺带小项

- `renderer::is_system_light_theme`（`src/renderer.rs:572`）为 `pub` 但唯一消费方是同文件的 `update_text_color`：降为私有。
- `renderer::push_ascii`（`src/renderer.rs:578-582` 及测试 684-690 行）：`buf.extend(s.encode_utf16())` 对 ASCII 输入输出完全相同且零分配，删除函数改用标准库。

## 3. 验收标准

- `fetch_url` 签名收为三参；`INTERNET_DEFAULT_HTTP_PORT` 不再被引用。
- `src/suspend.rs` 不再有自定义 `POWERBROADCAST_SETTING` 与 WTS 常量。
- `collect_cpu` 无返回值；调用点去掉 `let _ =`。
- 全仓检索 `self.font_size`、`push_ascii` 零命中；`is_system_light_theme` 非 `pub`。
- `cargo test`、`cargo clippy -- -D warnings`、`cargo fmt -- --check`、`cargo build --release` 全部通过。

## 4. 实施检查清单

- [ ] **2.1 fetch_url 死分支**：删 `secure` 形参；端口/flag 固定 HTTPS；改 4 个调用点
- [ ] **2.2 ABI 副本**：删本地 `POWERBROADCAST_SETTING` 与 `WTS_SESSION_LOCK/UNLOCK`；`use` crate 定义；`match wparam.0 as u32`
- [ ] **2.3 collect_cpu**：返回 `()`；调用点去 `let _ =`；更新注释
- [ ] **2.4 font_size**：删字段与 273/475 行赋值
- [ ] **2.5 小项**：`is_system_light_theme` 降私有；删 `push_ascii` 及其测试，调用点改 `extend(...encode_utf16())`
- [ ] 全量 `cargo test` + `cargo clippy -- -D warnings` + `cargo fmt -- --check` + `cargo build --release`

## 5. 非目标与保留项

- **`--quit` 命令行参数**：仓库内唯一消费方是 `README.md:59` 的文档；安装器实际用 `taskkill /F /T /IM traffic-monitor.exe`（`installer.iss:47`）关闭进程，`scripts/*.ts` 与 `.github` 工作流亦无引用。删除属**移除已文档化的用户面**，是产品决策而非清理，**待产品拍板**，不纳入本 RFC。
- **WinHTTP 手写 HTTP（http.rs）不换 `reqwest/ureq`**、**BCrypt 手写 SHA-256（crypto.rs）不换 `sha2` crate**：AGENTS.md 决策 4 的 `/DELAYLOAD` + re-exec 子进程隔离是记录在案的设计，换库会破坏 DLL 常驻约束。
- **手写版本解析（version.rs）不换 `semver` crate**：现行解析刻意收紧为「恰好 major.minor.patch + 可选后缀 + 恰好两行」，`semver` 反而放行更多格式，交换不净赚。
- **`util::utf16` const 函数保留**：与 `windows::core::w!` 语义重复，但已核实 windows-core 0.62 未提供 `as_wide`/常量宽串切片助手，替换不减少 unsafe 扫描复杂度。
- **`config::WINDOW_TITLE` 不纳入**：删除收益薄，保留可辅助调试。

## 6. 风险

全部条目均为删除无用表面或换用逐字段一致的定义，无行为变化；唯一需留意的是 2.2 中 `POWERBROADCAST_SETTING` 替换后 `PBT_POWERSETTINGCHANGE` 处理路径的编译通过（布局已比对一致）。
