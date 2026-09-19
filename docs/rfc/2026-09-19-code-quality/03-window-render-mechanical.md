# Agent Note：窗口渲染与工程机械收敛打包

Status: proposed

## 问题

六项低风险机械项分散在窗口、渲染、托盘与工程配置，单拎每项都不值得开 PR 但合在一起是“读起来像谁写的”的主要来源：B2 是 DPI 缩放数学三处独立实现（`src/window.rs:190-194` 对宽高 GAP、`src/renderer.rs:431-434` 对宽高字号、`src/renderer.rs:516-520` 反推 scale），改一处舍入即差一像素；A3 是 `src/renderer.rs:563-564` 字体名与 `src/tray.rs:59-60` tip 的定长数组截断复制不补尾 NUL，不变量全靠字符串恰好很短；B3 是 `main.rs` 三静态量加 `window.rs:20-25` 两静态量共 8 处 `HWND ↔ isize` 裸转换（0 为空位约定每遍重写）；C1 剩余可调域常量（版本重试 `500`、更新线程栈 `64*1024`）散落，`src/update/mod.rs:658` 另有本地 `CREATE_NO_WINDOW`；E4 是 `src/main.rs:235-252` 全限定路径、`src/main.rs:600` 唯一 match guard、`src/collector/cpu_mem.rs:28-32` 双重 cast；F2 是 `Cargo.toml` 缺 `rust-version` 与 `[lints]`。

## 提案

一个 PR 按序六个提交：其一，`config.rs` 或 `util.rs` 新增唯一 `dpi_scaled(base, dpi)`（96 基准四舍五入），三处调用点数值逐点对账；其二，两处截断复制改为“留一位尾 NUL 并手动补零”，短字符串输出不变；其三，新增 `AtomicHwnd` newtype（`store/load/take`，`take` 语义等于今天的 `swap(0)`），`live_main_hwnd` 与 `watchdog_hwnd` 退化为 `load` 加 `IsWindow`，`unregister_session_notification` 与 `rebuild_main_window` 的取走语义用 `take`，`POWER_NOTIFY_HANDLE`（`HPOWERNOTIFY`）同构单列；其四，可调常量进 `config.rs`（`UPDATE_FETCH_RETRY_DELAY_MS`、`UPDATE_WORKER_STACK_BYTES`），`CREATE_NO_WINDOW` 改为导入 windows crate 自带常量（`Win32_System_Threading` 已在 `Cargo.toml:25` 启用，注意其类型若为 `PROCESS_CREATION_FLAGS` newtype 则取 `.0` 适配 `creation_flags(u32)`，以编译为准），容量 hint（`with_capacity(16/32)`）明确不搬；其五，E4 三处等价改写（消息循环提 `use`、guard 改函数体内 if、`FILETIME` 直传消双重 cast）；其六，`Cargo.toml` 补 `rust-version = "1.85"` 与 `[lints]`（`unsafe_op_in_unsafe_fn = "deny"`、`undocumented_unsafe_blocks = "warn"` 等），补完本地即全绿。

## 明确不在本次范围

`Layout::new` 反推 scale 的调用语义只换实现不换输入输出；`EMBEDDED` 单点真值与嵌入时序不动；`suspend.rs:299` 的 `WM_SETTINGCHANGE` 低频 `encode_utf16` 维持现状；任何 Win32 API 行为变更都不含；容量 hint 与 API 语义常量的区分口径若要写进 AGENTS.md 另议，不在本 PR。

## 为什么不保留？

最强反方是“`AtomicHwnd` 把五处简单代码换成新类型是抽象税”。回应：税只交一次，买的是把“0 为空位加 `AcqRel/Acquire` 配对”从 8 遍手写变成一处定义，且 `take` 与 `load` 的语义差（重建路径必须取走、查询路径只读）在今天全靠阅读者分辨，类型直接让误用编译失败；次强反方是“`[lints]` 的 `undocumented_unsafe_blocks` 可能被旧工具链拒绝”。回应：`rust-version = "1.85"` 恰是 edition 2024 下限声明，若某 lint 在 MSRV 不存在则以编译实测为准删减，不硬凑。

## 验收标准

`cargo test --locked`、`cargo build --release --locked`、`cargo clippy --all-targets --locked -- -D warnings`、`cargo fmt` 全绿；`grep -rn "as isize\|as \*mut std::ffi::c_void" src/main.rs src/window.rs` 裸 HWND 转换零残留（`AtomicHwnd` 内部实现除外）；`grep -rn "15000\|too_many_arguments\|TIMER_ID_AUTO_UPDATE if" src` 相关散落零命中；三处 DPI 调用点数值 diff 证明不变；`grep -n "HTTP_TIMEOUT_MS\|UPDATE_FETCH_RETRY_DELAY_MS\|UPDATE_WORKER_STACK_BYTES" src/config.rs` 全部命中。

## 风险

真实残留风险是 `AtomicHwnd` 逐点语义贴错（`load` 误换 `take` 会导致重建路径丢句柄或查询路径清零）。证伪：核对单强制逐点对照——`unregister_session_notification` 与 `rebuild_main_window` 用 `take`，`live_main_hwnd`、`watchdog_hwnd`、`get_taskbar_hwnd` 缓存读用 `load`，评审 diff 逐行勾选；`CREATE_NO_WINDOW` 类型适配以编译器为准，不猜。
