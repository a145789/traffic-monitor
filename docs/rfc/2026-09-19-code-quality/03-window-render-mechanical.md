# Agent Note：窗口渲染与工程机械收敛打包

Status: proposed

## 问题

六项低风险机械项分散在窗口、渲染、托盘与工程配置，单拎每项都不值得开 PR 但合在一起是「读起来像谁写的」的主要来源：B2【P2】是 DPI 缩放数学两处正向独立实现（`src/window.rs:190-194` 对 `DISPLAY_WIDTH/HEIGHT/GAP` 缩放、`src/renderer.rs:431-434` 对宽高与 `FONT_BASE_SIZE` 缩放）加一处宽度反推（`src/renderer.rs:516-520` 用 `width / DISPLAY_WIDTH` 反推 scale），两处正向一旦有一处改舍入策略就会出现「窗口与位图差一像素」的极难排查错位（宽度反推自实现时被证实不能纳入 `dpi_scaled`：经整数 DPI 中转在 96–384 范围实测 77 处差一像素，故保持原公式，见提案其一）；A3【P2】是 `src/renderer.rs:562-564` 字体名与 `src/tray.rs:58-60` tip 的定长数组截断复制不补尾 NUL，SAFETY 注释声明「`lfFaceName` 含尾 NUL」而代码并不保证，不变量成立只因当前字符串恰好很短，属文档与实现漂移；B3【P2】是 `main.rs` 三静态量加 `window.rs` 两静态量共 8 处 `HWND ↔ isize` 裸转换（`store(hwnd.0 as isize)` / `load() → raw != 0 → HWND(raw as *mut c_void)`），「0 是空位」这个约定每遍重写；C1【P2】剩余可调域常量散落（版本文件重试 `500` 在 `src/update/mod.rs:291`、更新线程栈 `64 * 1024` 在 `src/update/mod.rs:181`），`src/update/mod.rs:658` 另有本地 `const CREATE_NO_WINDOW`；E4【P3】是 `src/main.rs:600` 唯一的 match guard 与兄弟 arm 的函数体内 if 风格不一致、`src/collector/cpu_mem.rs:28-32` 的 `&mut idle_time as *mut u64 as *mut _` 双重 cast；F2【P3】是 `Cargo.toml` 缺 `rust-version`（edition 2024 ⇒ ≥1.85）与 `[lints]` 表。

## 提案

一个 PR 按序六个提交，前两个（B2、A3）收益最高、可单独提前成一个 PR，后四个属机械打包项：其一，`config.rs` 或 `util.rs` 新增全项目唯一的 `dpi_scaled(base: i32, dpi: u32) -> i32`，实现为 `((base as f64) * (dpi as f64) / 96.0).round() as i32`，两处正向调用点数值逐点对账（`Layout::new` 的宽度反推不纳入：它从已舍入的实际宽度取比例，经整数 DPI 中转会二次舍入差一像素，实施时以 96–384 全范围对账证实 77 处差异，故保持原宽度公式不动，落点为 `util::dpi_scaled`）；其二，两处截断复制改为先算 `let copy_len = (s.len() + 1).min(arr.len()) - 1;`（`arr` 分别取 `lf.lfFaceName` 与 `nid.szTip`），再 `arr[..copy_len].copy_from_slice(&s[..copy_len]); arr[copy_len] = 0;`——即「完整放入 + 1 个尾 NUL，放不下就截断到 `len-1` 并手动补 NUL」，正常长度字符串输出不变；其三，新增 `AtomicHwnd` newtype（`store` 用 Release、`load` 用 Acquire、`take` 等于今天的 `swap(0)` 配 AcqRel），`live_main_hwnd` 与 `watchdog_hwnd` 退化为 `load` 加 `IsWindow`，`unregister_session_notification` 与 `rebuild_main_window` 的取走语义用 `take`，`POWER_NOTIFY_HANDLE`（`HPOWERNOTIFY`）同构单列或做泛型 `AtomicHandle<T>`；其四，可调常量进 `config.rs`（`UPDATE_FETCH_RETRY_DELAY_MS`、`UPDATE_WORKER_STACK_BYTES`），`CREATE_NO_WINDOW` 改为直接导入 windows crate 自带常量（`Win32_System_Threading` 已在 `Cargo.toml:25` 启用；其类型若为 `PROCESS_CREATION_FLAGS` newtype 则取 `.0` 适配 `creation_flags(u32)`，以编译为准），容量 hint（`with_capacity(16/32)`）属实现细节，明确不搬——口径是「可调域参数进 `config.rs`，API 语义常量留在使用点」，硬搬只会让 `config.rs` 膨胀；其五，E4 两项等价改写（`TIMER_ID_AUTO_UPDATE` 的 match guard 改函数体内 if 与兄弟 arm 统一——实施时新版 clippy（1.98）的 `collapsible_match` 禁止 arm 体只剩单个 if，落点为条件先命名 `let active` 再体内 if，无 guard 且过门禁、`FILETIME` 类型直传消双重 cast）；其六，`Cargo.toml` 补 `rust-version = "1.85"` 与 `[lints]`（`rust.unsafe_op_in_unsafe_fn = "deny"`、`clippy.enum_glob_use = "warn"`、`clippy.items_after_statements = "warn"`；`clippy.undocumented_unsafe_blocks` 暂不入——实施时实测 8 个文件共 42 处存量 unsafe 块无 SAFETY 注释，全补属独立任务，硬塞会撑爆本 PR 的评审面，见验收），把 CI 已兑现的纪律固化进仓库本身。性能与内存：六项均不新增运行时分配，`AtomicHwnd` 与现有原子读写完全同构。

## 明确不在本次范围

`Layout::new` 保持原宽度反推公式不动（输入输出皆不变；经整数 DPI 中转会二次舍入，实施对账证实差一像素，故不换实现）；`EMBEDDED` 单点真值与嵌入时序不动；`suspend.rs:297` 的 `WM_SETTINGCHANGE` 每次都 `encode_utf16().collect()` 一个 Vec，但属低频路径（一天至多数次），权衡后维持现状；D3（`src/window.rs:54` 的七参 `create_window` 加 `#[allow(clippy::too_many_arguments)]`）归 04 号 RFC，本 PR 不动；E4 的全限定路径部分（`src/main.rs:235-252` 消息循环里重复出现的 `GetMessageW`/`TranslateMessage`/`DispatchMessage`/`MSG`）随 04 号 RFC 的 E3 一并解决——提取 `run_message_loop()` 时把这些名字加进 `use` 即可，本 PR 只做 match guard 与 `FILETIME` 两项；`HTTP_TIMEOUT_MS`（原 `15000`）由 01 号 RFC 负责，不在本 PR；以下两处现有设计属禁止倒退资产，本轮不得以「整洁」之名简化——`renderer.rs` 的热路径零分配纪律（`self.buf` 复用、`write_u32` 手写整数格式化、`LAST_RENDERED_VALUES` 变化门控，空闲时零 GDI 绘制）、`build.rs` 的 DELAYLOAD 等内存卫生配置；AGENTS.md 第 1-7 条钉死的 Win32 时序不动；任何 Win32 API 行为变更都不含；容量 hint 与 API 语义常量的区分口径若要写进 AGENTS.md 另议，不在本 PR。

## 为什么不保留？

最强反方是「`AtomicHwnd` 把五处简单代码换成新类型是抽象税」。回应：税只交一次，买的是把「0 为空位加 `AcqRel/Acquire` 配对」从 8 遍手写变成一处定义，且 `take` 与 `load` 的语义差（重建路径必须取走、查询路径只读）在今天全靠阅读者分辨，类型直接让误用编译失败；性能上与现有原子读写完全同构。次强反方是「`[lints]` 的 `undocumented_unsafe_blocks` 可能被旧工具链拒绝」。回应：`rust-version = "1.85"` 恰是 edition 2024 下限声明，若某 lint 在 MSRV 不存在则以编译实测为准删减，不硬凑。

## 验收标准

`cargo test --locked`、`cargo build --release --locked`、`cargo clippy --all-targets --locked -- -D warnings`、`cargo fmt` 全绿；`grep -rn "hwnd\.0 as isize\|HWND(.*c_void" src/main.rs src/window.rs` 裸 HWND 转换零残留（`AtomicHwnd` 内部实现除外；注：原口径 `as isize` 会误伤 `window.rs` 两处 `SetWindowLongPtrW` 必需的样式标志转换，那是 API 要求的 `isize` 而非句柄转换，不在本项）；`grep -rn "TIMER_ID_AUTO_UPDATE if" src` 与 `grep -rn "as \*mut u64" src` 零命中；两处正向 DPI 调用点数值 diff 证明不变（`Layout` 未动，天然不变）；`grep -n "UPDATE_FETCH_RETRY_DELAY_MS\|UPDATE_WORKER_STACK_BYTES" src/config.rs` 全部命中，`grep -n "const CREATE_NO_WINDOW" src/update/mod.rs` 零命中（本地定义删除；注：原口径字面 `CREATE_NO_WINDOW` 零命中不可能——导入与使用行必含该名；改由 windows crate 导入）；`[lints]` 落三项，`undocumented_unsafe_blocks` 未入（见提案其六）；本 PR 会改 `Cargo.toml`（F2 的 `rust-version`/`[lints]`），无依赖变化故 `Cargo.lock` 无 diff、无需附带提交（`--locked` 门禁已验证全绿）。

## 风险

真实残留风险是 `AtomicHwnd` 逐点语义贴错（`load` 误换 `take` 会导致重建路径丢句柄或查询路径清零）。证伪：核对单强制逐点对照——`unregister_session_notification` 与 `rebuild_main_window` 用 `take`，`live_main_hwnd`、`watchdog_hwnd`、`get_taskbar_hwnd` 缓存读用 `load`，评审 diff 逐行勾选；`CREATE_NO_WINDOW` 类型适配以编译器为准，不猜。
