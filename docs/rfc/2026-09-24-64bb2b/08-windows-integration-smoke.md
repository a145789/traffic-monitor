# Agent Note：补一条 Windows 窗口过程与重建冒烟（只覆盖能自动化的部分）

Status: proposed

**前置依赖：04 篇（定时器缺失集合与 DPI 事务）、02 篇（休眠订阅的配对注销）**。在这两篇合入前，本 note 的用例无法断言它们引入的状态。

## 问题

当前 107 个测试全部是内联在 `src/` 的 `#[cfg(test)]` 单元测试（`grep -rn "#\[test\]" src/` = 107；`tests/`、`benches/`、`examples/` 均不存在，`grep -rn "#\[ignore\]" src/` 命中 0 处）。覆盖的是纯判定与解析：`src/update/protocol.rs:230-387` 用 `Cursor` 喂协议字符串、`src/suspend.rs:382-431` 测 `timer_plan` 计划集合、`src/update/version.rs` 测版本解析、`src/collector/network.rs:313+` 用构造的 `MIB_IF_ROW2` 测过滤判定、`src/renderer.rs:608-683` 测格式化字符串。`src/main.rs:771-773` 的模块文档自己写明「只覆盖纯判定与句柄校验，不创建真实窗口」。CI 侧同样没有兜底：`.github/workflows/check.yml:25-35` 只跑 fmt / test / clippy / build，`:40-54` 的 msrv job 只跑 `cargo check`；`.github/workflows/release.yml:46-47` 只构建、从不运行测试。

**旧稿在落地层有四处硬伤**（本稿据实修正，逐条都有源码依据）：

1. **可见性假设错误。** 旧稿写「不为可测性改动生产代码的可见性（需要访问的符号在 `#[cfg(test)]` 内可见即可）」。不成立：`EMBEDDED` 是 `src/window.rs:31` 的模块私有 static，`POWER_NOTIFY_HANDLE`（`src/main.rs:67`）、`SESSION_NOTIFY_HWND`（`src/main.rs:72`）同理（它们的原子存储类型 `AtomicPowerNotify`/`AtomicHwnd` 在 `src/util.rs`，但 static 本身是模块私有的）；`#[cfg(test)]` 只控制编译，不改变可见性。跨模块断言 `EMBEDDED` 必须新增 `pub(crate)` 的只读 accessor，否则用例编译不过。
2. **托盘副作用被漏掉。** 旧稿称「不注册托盘、不占用单例互斥量」。后半句对（不走 `main()` 就不会拿单例锁），前半句错：`rebuild_main_window` 会 `remove_tray_icon()`（`src/main.rs:469`）并经 `bind_display_and_timers`（`src/main.rs:477`）调 `create_tray_icon`（`src/main.rs:397`）——用例会真的改动系统托盘，清理时必须补 `remove_tray_icon()`。
3. **建窗前必须注册窗口类。** `create_main_window`（`src/window.rs:101`）与 `create_watchdog_window`（`src/window.rs:131`）只调 `create_window`，类注册是 `register_window_class()`/`register_watchdog_class()`（`src/window.rs:97-124`），生产里由 `src/main.rs:209-218` 分别调用。旧稿的用例直接调 `create_*_window()` 必然失败。
4. **验收计数与跳过的语义都错。** 新增 4 个 `#[ignore]` 后，默认 `cargo test` 的输出是 `107 passed; 4 ignored`，不可能是 `107 passed; 0 ignored`；且 Rust 没有原生 skip，提前 `return` 仍记为 passed——「无 Explorer 就跳过」等于给出一条永远绿的假门禁。

此外两个待覆盖的真实缺陷恰好都落在 Win32 集成边界上，纯函数测试无论怎样组合都测不到：`PBT_APMSUSPEND` 依赖顶层广播而主窗口嵌入后是 `WS_CHILD`（02 篇）；`update_dpi` 失败后的回滚会被下个 tick 的 `update_taskbar_position` 撤销（04 篇）。

生产消费者：无（这是新增的验证设施）。非生产消费者：现有 107 个内联测试，它们覆盖的层面与本 note 不重叠，不构成替代。

## 提案

在 `src/` 内新增一个 `#[cfg(test)]` 冒烟模块（**不放 `tests/`**：本仓库是 bin-only crate，没有 lib target，`tests/*.rs` 无法访问 `main.rs` 的内部符号；要在 `tests/` 里测就得先拆分出 lib target，成本远大于收益）。用例全部标 `#[ignore]`，由显式命令运行：`cargo test --locked -- --ignored --test-threads=1`。

**Fixture 契约（必须先立，否则用例之间必然互相污染）**：

- 一个串行的 RAII fixture：`setup()` 依次 `ImmDisableIME(u32::MAX)`（生产在 `src/main.rs:203-207` 要求它在首个顶层窗口之前，测试进程也必须遵守，否则测试进程本身会成为「未禁 IME 就建窗」的反例并加载第三方 TSF）、`register_window_class()`、`register_watchdog_class()`、`Renderer::new()` + `renderer::set_renderer`、建看门狗窗口与主窗口；`Drop` 里按生产同样的顺序清理：先 `unregister_session_notification()`（生产纪律：注销先于 `DestroyWindow`）、`unregister_power_notifications()`（02 篇后此处覆盖全部三项电源/休眠订阅，不再逐个调 Win32 注销）、`remove_tray_icon()`、`DestroyWindow` 两个窗口、`renderer::take_renderer()`。
- 需要的 `pub(crate)` 只读 accessor：`EMBEDDED`、`CURRENT_MAIN_HWND`、`SESSION_NOTIFY_HWND` 与电源三句柄（02 篇把单句柄拆成 `POWER_NOTIFY_HANDLE` / `DISPLAY_NOTIFY_HANDLE` / `SUSPEND_NOTIFY_HANDLE` 三个具名原子，重建断言须覆盖三者才算钉住定向订阅的重绑；每处只加一个 getter，不暴露写入口，符合 AGENTS.md 第 10 条「唯一真值源」）。
- 预处理条件（如「系统里没有 Explorer / 任务栏」）不满足时**显式 panic 并给出可操作的失败信息**（例如「本机无 Explorer，跳过前请先启动 explorer.exe」），不使用提前 `return` 的假 skip；该套件本身就是 opt-in 的 `#[ignore]`，失败比假绿更有价值。

**自动化用例（标 `#[ignore]`）**：

1. 建窗与嵌入：`create_watchdog_window()` + `create_main_window()` 成功后 `get_taskbar_hwnd()` 非空；`embed_in_taskbar(hwnd)` 返回 `Ok`，且 `GetParent(hwnd)` 等于 `get_taskbar_hwnd()`（这条同时是 04 篇后置断言的回归保护）。
2. 重建路径：直接调 `rebuild_main_window(watchdog)`（**刻意绕过** `TaskbarCreated` 路由，只测重建函数本身；路由依赖真实广播，属人工验收），断言 `CURRENT_MAIN_HWND` 已换成新句柄、`EMBEDDED` 为真、`SESSION_NOTIFY_HWND` 与三项电源/休眠订阅句柄非空，且用例结束时托盘无残留（`remove_tray_icon()` 由 fixture 负责）。
3. 定时器收敛与挂起对称：`sync_monitoring_timers(hwnd)` 返回的缺失集合为空；`suspend_system(hwnd, SUSPEND_REASON_SESSION)` → `resume_system(hwnd, SUSPEND_REASON_SESSION)` 各一次后再调仍为空（对应 `src/suspend.rs:169-216` 的对称性与 04 篇的返回值改造）。
4. `WM_DPICHANGED` 消息处理（**最低断言，明确其局限**）：`SendMessageW(hwnd, WM_DPICHANGED, ...)` 后进程不 panic、`DPI_DIRTY` 最终被清。必须写清：处理器用 `GetDpiForWindow`（`src/renderer.rs:439`）取当前 DPI，手工发消息**不会**改变它，所以这条用例**不验证**跨屏尺寸变化；真正的跨屏 DPI 行为留在人工清单里。若实施时发现这条用例只能证明「不 panic」，宁可删掉它，也不要把它写成「已覆盖 DPI」。

**人工清单（发布前过一遍，写进同一个文件顶部注释或随 04 篇的验收）**：合盖睡眠→唤醒后网络数值在 1–2 秒内、CPU/内存在一个 5 秒周期内恢复更新（`src/config.rs:63-65`，且 `src/suspend.rs:59-62` 的恢复边沿只重建基线，首轮不会显示虚高速率）；跨两个不同缩放比显示器的拖动；断开网络后手动检查更新的失败提示；更新确认框点「是」后主程序退出、安装器正常启动；安装器 UAC 取消后主程序被重新拉起；09 篇的「漏收解锁」故障注入。

## 明确不在本次范围

- **不拆分 lib target、不新建 `tests/` 目录**（bin-only 结构是既定事实，改造它属于架构变更）。
- **不把冒烟用例纳入 `check.yml` 的必跑门禁**：它对环境（真实 Explorer、真实任务栏、交互式桌面）有依赖，先作为 opt-in 的手动套件，稳定性数据积累后再定。
- **不为可测性改控制流**；只为各只读状态加一个 `pub(crate)` getter（`EMBEDDED`、`CURRENT_MAIN_HWND`、`SESSION_NOTIFY_HWND`、三项电源/休眠句柄共 6 个，这是上一稿“4 个”按 02 篇拆分后的数量修正）。
- **不覆盖渲染像素级比对**（GDI 位图比对在不同 DPI/主题下噪声太大）。
- **不做 `WM_DPICHANGED` 的「真实跨屏」自动化**：需要在测试进程里改变窗口所在显示器，成本与稳定性都不成立。
- **不做故障注入的自动化**（把后置断言改成恒假之类的动作只作为一次性失效验证，不进代码库）。

## 为什么不保留？

1. 「86 个测试已经不少了」——数量不构成覆盖：本次审计确认真实缺陷落在 Win32 集成边界上，且都是**产品级**故障（组件永久错版式、省电分支永不生效）。现状概括就是「测试证明了解析正确，没证明程序能跑」。
2. 「Win32 集成测试不稳定，容易 flaky」——所以标 `#[ignore]`、不进必跑门禁；不稳定只影响「纳入 CI」这一步，不影响本地手动跑的价值。
3. 「真实窗口测试会干扰用户当前会话」——用例自己建窗、自己销毁，不经过 `main()`，不占用单例互斥量；副作用是任务栏上短暂出现一个面板与一枚托盘图标，fixture 的 `Drop` 负责清掉后者。
4. 「`--ignored --test-threads=1` 命令反直觉，没人会记得跑」——这正属于 `AGENTS.md` 更新指南允许的「非直觉的验证发布命令」；实施时在该文档的构建与发布一节补一行，本 note 已写进验收标准。

## 验收标准

- `grep -rn "#\[ignore\]" src/` 命中 ≥ 4 处（或按最终用例数一致）；命令必须带串行开关：`AGENTS.md` 新增的那一行含 `cargo test --locked -- --ignored --test-threads=1`。
- 默认门禁输出**按实际数字核对**：新增 N 个 `#[ignore]` 后 `cargo test --locked` 输出为 `107 passed; N ignored`（不是 `0 ignored`），且既有 107 条全部仍然通过。
- `cargo test --locked -- --ignored --test-threads=1` 在有 Explorer 的交互式桌面上全绿；没有 Explorer 时**显式失败**并打印「请先启动 explorer.exe」这类可操作信息（不允许静默 passed）。
- 失效验证（必须做一次，这是本 note 唯一能证明「测试真的承重」的手段）：临时把 `embed_in_taskbar` 的后置断言改成恒假 → 用例 1 必须变红；临时删掉 `DPI_DIRTY` 的置位或清位 → 用例 4 必须变红；验证完恢复代码，并在 PR 描述里记录这两次观察。
- `grep -rn "pub(crate) fn.*embedded\|embedded()" src/window.rs` 等 accessor 存在（人工核对：只加 getter，没有暴露写入口）。
- `cargo clippy --all-targets --locked -- -D warnings` 与 `cargo fmt -- --check` 全绿（`--all-targets` 会带上新增的 test 目标）。
- `AGENTS.md` 的「构建与发布」小节新增一行手动命令说明（按该文档自己的更新指南，属「非直觉的验证发布命令」）。

## 风险

- 用例与生产代码共享全局状态（`CURRENT_MAIN_HWND`、`EMBEDDED`、`SESSION_NOTIFY_HWND`、`LAST_RECT` 等），同进程并行会互相污染；靠 `--test-threads=1` + 单一 RAII fixture 收敛，实施时必须把这条写进用例头部注释，否则下一个人加用例时会破坏它。
- `Handler`/`Drop` 的清理顺序写错会留下未配对的 WTS/电源注册（生产纪律要求「注销先于 `DestroyWindow`」），进而影响后续手工运行；fixture 的清理顺序必须与 `src/main.rs:297-308`、`:429-467` 逐条对齐。
- 真实窗口测试在无人值守的 CI runner 上可能根本建不出窗（runner 有会话但 Explorer/任务栏不保证存在）；本 note 按「本地手动 + 显式失败信息」定位，不承诺 CI 可跑。
- 断言「窗口尺寸 == 渲染器位图尺寸」需要渲染器已构造（`Renderer::new` + `set_renderer`，`src/main.rs:258-264`），且 04 篇的 DPI 事务要先合入，否则该断言会命中当前的「窗口新尺寸 + 位图旧尺寸」错配而红——这正是两篇的依赖方向，不能颠倒实施顺序。
- 托盘图标在测试进程里创建/移除会让用户看到一次闪烁；若这被认为不可接受，退路是把用例 2 拆成「只断言 `CURRENT_MAIN_HWND` 与嵌入状态、不调 `bind_display_and_timers`」的更窄版本，代价是重建路径的托盘重绑环节失去覆盖。
