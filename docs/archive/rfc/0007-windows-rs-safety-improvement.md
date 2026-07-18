# RFC 0007: 深度引入 windows-rs 生态提升类型安全与健壮性

- **状态**: Draft (草案)
- **创建时间**: 2026-07-14
- **更新记录**:
  - 2026-07-14: 初稿
  - 2026-07-14: 修正 2.3 节关于 `SYSTEM_BATTERY_STATE` 结构体可用性的事实错误；将 2.2 降级为可选；重排优先级
- **关联文件**: `Cargo.toml`, `src/util.rs`, `src/tray.rs`, `src/main.rs`, `src/thermal.rs`, `src/ffi_guard.rs`, `src/renderer.rs`

---

## 1. 背景与现状 (Context)

本项目作为常驻 Windows 系统托盘的小组件，其核心依赖于与底层 Win32 API 交互。当前项目虽然已经引入了 `windows` crate (v0.62)，但出于历史原因与局部实现的简便性，还存在大量**自维护声明**与**低效 FFI 数据加工**：

1. **注册表读写样板代码堆叠**：通过手写 `to_wide` UTF-16 缓冲区、手动分配栈空间指针转换（`from_raw_parts`）以及自维护的句柄析构容器（`ffi_guard::RegKey`）来进行基本的注册表操作，带来了极高的 `unsafe` 风险和悬空指针隐患。涉及文件约 125 行代码。
2. **手写 extern DLL 绑定**：[thermal.rs](file:///D:/work_space/life/traffic-monitor/src/thermal.rs) 内部手动导出了 `powrprof.dll` 的 `CallNtPowerInformation`，并手写了用于对齐字节布局的 `SystemBatteryState` / `ProcessorInformation` 结构体。**经验证**，`windows::Win32::System::Power` 模块已提供 `CallNtPowerInformation` 函数（含类型安全的 `POWER_INFORMATION_LEVEL` 枚举），但结构体定义仍需自行维护（详见 2.3 节）。
3. **句柄守卫零散自维护**：自定义了 `MutexGuard`、`MenuGuard` 等 RAII 包装，而未能完全使用微软官方推出的原生所有权包装机制。

本 RFC 旨在制定一个循序渐进的 Windows API 重构方案，通过深度利用官方成熟的原生投影与子 crate 生态，来**增强内存安全性、收窄 unsafe 范围并杜绝潜在句柄泄漏**。同时严格遵循"不为做而做"的原则：每项提案必须有可量化的收益（消除 unsafe 行数、减少手工维护代码量），且不引入额外的运行时开销或依赖膨胀。

---

## 2. 核心重构与优化项

### 2.1 引入 `windows-registry` 库实现 100% 安全注册表操作

**方案**：在 `Cargo.toml` 中引入微型官方子 crate `windows-registry`。利用其提供的高层 Safe API 替换目前在 `src/util.rs` 和 `src/tray.rs` 中手工维护的底层句柄管理与多字节指针转换。

* **依赖体积分析**：
  `windows-registry` 是微软官方维护的微型 crate（源码约 105KB，2.5K SLoC）。其核心依赖为 `windows-core`、`windows-strings`、`windows-result`，这些 crate **已经是 `windows` v0.62 的传递依赖**——引入 `windows-registry` 不会新增任何第三方 crate，二进制体积增量接近零。

* **优化前**（来自 `util.rs` + `tray.rs`，合计约 125 行）：
  ```rust
  // 手动管理 HKEY 生命周期的 RAII 守卫
  let mut hkey = Default::default();
  unsafe {
      RegOpenKeyExW(hkey_root, PCWSTR(key_path.as_ptr()), Some(0), KEY_READ, &mut hkey).is_ok()
  };
  let _key_guard = crate::ffi_guard::RegKey::new(hkey);
  // 后续复杂的字节缓冲区指针提取...
  ```

* **优化后**（DWORD 读写）：
  ```rust
  use windows_registry::CURRENT_USER;

  pub fn reg_read_dword(subkey: &str, value_name: &str) -> Option<u32> {
      CURRENT_USER.open(subkey)
          .and_then(|key| key.get_u32(value_name))
          .ok()
  }
  ```

* **需要验证的边界场景**：
  - `tray.rs:369-396` 中的 **REG_SZ 写入**（自启动路径）：需确认 `windows-registry` 的 `set_string` API 行为与当前的 `RegSetValueExW` + `REG_SZ` 一致。
  - `tray.rs:371-372` 中的 **RegDeleteValueW**：需确认 `windows-registry` 提供了值删除 API（对应 `key.remove_value(name)` 或等价方法）。

* **收益**：**完全消除注册表读写中的 `unsafe` 块**（当前共 7 处 unsafe），删去自维护的 `RegKey` 守卫（`ffi_guard.rs` 16 行）和 `to_wide` 分配（每次注册表操作节省一次堆分配）。转换后的代码天然异常安全（Panic-safe），完全由 Rust 编译器提供生命周期保障。

### 2.2 迁移至 `Win32_System_Power` 的原生 `CallNtPowerInformation` 投影

**方案**：废除 [thermal.rs](file:///D:/work_space/life/traffic-monitor/src/thermal.rs) 中手写的 `#[link(name = "powrprof")]` extern 块，改用 `windows::Win32::System::Power::CallNtPowerInformation` 和类型安全的 `POWER_INFORMATION_LEVEL` 枚举替换魔法数字。

* **已验证的事实**（基于 windows v0.62 源码）：
  - ✅ `CallNtPowerInformation` **已存在于** `windows::Win32::System::Power` 模块中，无需手动链接 `powrprof.dll`。
  - ✅ `POWER_INFORMATION_LEVEL` **是一个类型安全的枚举**（非 raw `u32`），可替换当前代码中的 `const SYS_BATT_STATE_LEVEL: u32 = 5` 和硬编码的 `11`。
  - ❌ `SYSTEM_BATTERY_STATE` 和 `PROCESSOR_POWER_INFORMATION` 结构体**未在该模块中找到**。`CallNtPowerInformation` 的签名使用 raw output buffer（`*mut u8`），windows crate 并不为每个 info level 生成专用的输出结构体。

* **这意味着什么**：`SystemBatteryState` 和 `ProcessorInformation` 两个 `#[repr(C)]` 结构体**仍需在 thermal.rs 中自行维护**。收益主要集中在消除 `extern` 块（7 行）和用类型安全的枚举替换魔法数字，而非 RFC 初稿中乐观估计的"完全消除手写结构体"。

* **优化前**：
  ```rust
  #[link(name = "powrprof")]
  unsafe extern "system" {
      fn CallNtPowerInformation(
          level: u32,
          in_buf: *const u8,
          in_len: u32,
          out_buf: *mut u8,
          out_len: u32,
      ) -> i32;
  }
  const SYS_BATT_STATE_LEVEL: u32 = 5;
  // ...调用时传 raw u32...
  ```

* **优化后**：
  ```rust
  use windows::Win32::System::Power::{
      CallNtPowerInformation,
      POWER_INFORMATION_LEVEL,
      SystemBatteryState as SysBattLevel,  // 枚举变体，非结构体
  };
  // 调用时传 POWER_INFORMATION_LEVEL::SystemBatteryState，编译期类型检查
  // SystemBatteryState / ProcessorInformation 结构体保留，不做改动
  ```

* **核心验证与隐式约束**（⚠️ 关键验证）：
  在 `SYSTEM_BATTERY_STATE` 中，放电/充电速率 `Rate` 代表着本项目的热容推断基础。当笔记本拔电（放电）时，其 `Rate` 在 Windows SDK 中是一个带符号的负值（例如 -12000mW）。
  * **校验策略**：迁移后必须立即通过真机插拔电调试，确保引入的原生 `CallNtPowerInformation` 函数签名（返回值类型可能与手写 `i32` 不同）在不同平台下依然能正确填充 `SystemBatteryState` 结构体，`Rate` 字段的符号不被截断或误读。
  * 由于结构体仍由本项目维护（不受 windows crate 版本影响），**布局兼容性风险较初稿的乐观估计更低**——结构体字段布局不做改变，只有函数调用方式改变。

### 2.3 （可选）`Owned<T>` 接管内核句柄析构

**方案**：对于 `CreateMutexW` 创建的单实例互斥量句柄，评估用 `windows::core::Owned<HANDLE>` 替换自建的 `ffi_guard::MutexGuard`。

* **当前状态**：`ffi_guard::MutexGuard` 共 11 行代码，功能明确——在 drop 时调用 `CloseHandle`。代码正确性已验证，`SAFETY` 注释完整。

* **评估**：
  - `Owned<HANDLE>` 确实在 drop 时调用 `CloseHandle`，与当前行为一致。
  - **但这不会消除任何 `unsafe` 块**：`CreateMutexW` 本身就是 unsafe FFI 调用，`Owned<T>` 只接管析构，不解决创建时的安全性。
  - 代码节省量约 5 行（替换一个 11 行的 struct），边际收益极小。
  - `MenuGuard`（`HMENU` → `DestroyMenu`）是否能用 `Owned<HMENU>` 取决于 windows crate 是否为该句柄类型实现了对应的 drop trait，**迁移前需逐一验证**。

* **决策**：此项标记为**可选（P3）**。除非后续 windows crate 对 `Owned<T>` 的生态支持有显著增强（如自动推导更多句柄类型的析构行为），否则不建议为此投入工程时间。当前的 `MutexGuard` / `MenuGuard` / `RegKey` 实现简洁、正确、符合项目规模。

---

## 3. 工程红线与非目标 (Engineering Redlines & Non-Goals)

本 RFC 在追求"代码更 Rust 风格"时，严格遵守以下性能与空间红线限制，避免过渡重构：

### 3.1 ❌ 严禁替换 WinHTTP 核心架构

* **核心约束**：本项目目前的网络自动更新校验运行在子进程中（`traffic-monitor.exe --check-update`），且在主进程配置中通过 `/DELAYLOAD` 将 `winhttp.dll` / `bcrypt.dll` 等进行了延迟加载。子进程在请求校验完毕后立刻退出。
* **原因**：这使得主进程常驻内存零开销（不加载 TLS 栈和 TLS 模块的 DLL，如 `schannel`）。
* **红线**：禁止将此网络部分重构为诸如 `reqwest` 或 `ureq` 的 Rust 第三方 HTTP 库。引入它们会直接打破 `/DELAYLOAD` 优化，迫使主进程载入庞大的 TLS 支持和异步运行时，导致稳态后台内存占用成倍暴增，这属于严重的**反向重构**。

### 3.2 ❌ 不引入 `windows` crate 之外的重型依赖

* 任何新增 crate 必须是微软官方维护的 `windows-rs` 生态子 crate，且其依赖链不得超出当前 `windows` v0.62 已有的传递依赖范围。
* `windows-registry` 符合此约束（详见 2.1 节分析）；其他社区 Windows 绑定库（如 `winreg`）不符合——它们会引入 `winapi` 等冗余依赖。

### 3.3 ❌ 不为消除 unsafe 而消除 unsafe

* 仅当满足以下条件之一时才执行迁移：(a) 消除的 `unsafe` 块 ≥ 3 处；(b) 删除的自维护代码 ≥ 20 行；(c) 修复了一个已知或潜在的内存安全/句柄泄漏 bug。
* 仅节省 1-2 个 unsafe 块或 < 10 行代码的改动（如 2.3 节的 `Owned<T>` 替换）不满足门槛，标记为可选。

---

## 4. 优先级与实施步骤建议

| 优先级 | 任务名称 | 预期收益 | 关联文件 |
| :---: | :--- | :--- | :--- |
| 1 | **引入 `windows-registry`** | 消除 7 处 unsafe，删除 `RegKey` 守卫 + `to_wide` 分配合计约 140 行代码 | `util.rs`, `tray.rs`, `ffi_guard.rs`, `Cargo.toml` |
| 2 | **重构电源 API 调用投影** | 消除手写 `extern` 块（7 行），用 `POWER_INFORMATION_LEVEL` enum 替换魔法数字；结构体保留不动 | `thermal.rs` |
| 3 | **（可选）内核句柄 Owned 接管** | 节省约 5 行代码，不消除 unsafe；仅当 windows crate 对该特性有显著增强时再评估 | `main.rs`, `ffi_guard.rs` |

### 4.1 实施检查清单

- [ ] **P1 — windows-registry 迁移**
  - [ ] `Cargo.toml` 添加 `windows-registry` 依赖
  - [ ] 替换 `util.rs:reg_read_dword` → `windows_registry` API
  - [ ] 替换 `util.rs:reg_write_dword` → `windows_registry` API
  - [ ] 替换 `tray.rs:is_autostart_enabled` (REG_SZ 读取) → `windows_registry` API
  - [ ] 替换 `tray.rs:toggle_autostart` (REG_SZ 写入 + 删除) → `windows_registry` API
  - [ ] 删除 `ffi_guard::RegKey`，移除 `util.rs:to_wide` 中仅用于注册表的调用点
  - [ ] 验证 `renderer.rs:is_system_light_theme()` 行为不变
  - [ ] 全量 `cargo test` + 真机注册表读写验证

- [ ] **P2 — 电源 API 迁移**
  - [ ] 替换 `#[link(name = "powrprof")]` extern 块为 `use windows::Win32::System::Power::CallNtPowerInformation`
  - [ ] 替换 `SYS_BATT_STATE_LEVEL: u32 = 5` 为 `POWER_INFORMATION_LEVEL::SystemBatteryState`
  - [ ] 替换硬编码 `11` (ProcessorInformation level) 为对应的 `POWER_INFORMATION_LEVEL` 枚举变体
  - [ ] **保留** `SystemBatteryState` 和 `ProcessorInformation` 结构体定义不变
  - [ ] 真机插拔电验证：`Rate` 负值读取正确，热模型`collect_thermal` 输出与迁移前一致

- [ ] **P3 — Owned\<T\> 接管（可选）**
  - [ ] 评估 windows crate 后续版本对 `HMENU`/`HANDLE` 的 `Owned` drop trait 支持
  - [ ] 如满足门槛，替换 `MutexGuard` 并删除 `ffi_guard::MutexGuard`

---

## 5. 附录：调研记录

### 5.1 windows crate v0.62 `Win32_System_Power` 模块 API 覆盖情况

| API 符号 | 存在于 windows crate? | 备注 |
|---|---|---|
| `CallNtPowerInformation` | ✅ 是 | 签名使用 `POWER_INFORMATION_LEVEL` enum + raw output buffer |
| `POWER_INFORMATION_LEVEL` | ✅ 是 | 类型安全的枚举，含 `SystemBatteryState`(5) / `ProcessorInformation`(11) 等变体 |
| `SYSTEM_BATTERY_STATE` (结构体) | ❌ 否 | 需自行维护 `#[repr(C)]` 结构体 |
| `PROCESSOR_POWER_INFORMATION` (结构体) | ❌ 否 | 需自行维护 `#[repr(C)]` 结构体 |
| `RegisterPowerSettingNotification` | ✅ 是 | 已在 `main.rs` 中使用 |
| `UnregisterPowerSettingNotification` | ✅ 是 | 已在 `main.rs` 中使用 |

### 5.2 `windows-registry` 依赖链分析

```
windows-registry
├── windows-core       ← 已由 windows v0.62 传递引入
├── windows-strings    ← 已由 windows v0.62 传递引入
└── windows-result     ← 已由 windows v0.62 传递引入
```

**结论**：零新增传递依赖。
