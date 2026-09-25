# Agent Note：任务栏竖排时拒绝嵌入，不再留下几何无效却标记已嵌入的状态

Status: proposed

## 问题

`src/window.rs:208` 的 `calc_widget_rect` 无条件按「屏幕底部横向任务栏」推导几何：`src/window.rs:225` 取 `display_x = rc_tray.left - rc_taskbar.left - gap - display_width`，`src/window.rs:226` 取 `display_y = (rc_taskbar.bottom - rc_taskbar.top - display_height) / 2`。

任务栏竖排（位于屏幕左侧或右侧）时 `rc_tray.left` 与 `rc_taskbar.left` 几乎相等，`display_x` 退化为约 `-(display_width + gap)`，窗口整体落在父窗口客户区之外被裁掉；同一情形下 `display_y` 用的「任务栏高度」等于整屏高度，位置同样无意义。窗口不可见，但 `src/window.rs:288-297` 的 `SetWindowPos` 对子窗口的负坐标不会失败，`src/window.rs:311` 的事后断言只查 `GetParent(hwnd) == h_taskbar`（父子关系确实已改），于是 `src/window.rs:318` 把 `EMBEDDED` 置真。

这违反 `AGENTS.md`「任务栏嵌入与 Explorer 重建」第 1 条：嵌入完成后的最终状态必须是任务栏的**可见** child HWND，不得留下半嵌入状态。当前实现会带着一个谎报为真的 `EMBEDDED` 长期停在无效几何上，而后续依赖该位的路径（`src/window.rs:403` 的定位门、`src/window.rs:327` 的对齐语义）都会按「已嵌入」行事。

检索证据：`grep -rn "ABM_|ABS_|Shell_SecondaryTrayWnd|APPBARDATA" src` 命中 0 处，全仓没有任何任务栏方向判据；`grep -rn "rc_tray.left|rc_taskbar" src` 的几何推导只此一处（`src/window.rs:208-228`）。

生产消费者：`src/window.rs:240`（`embed_in_taskbar` 调用 `calc_widget_rect`），其上游为 `src/main.rs:377`（启动）、`src/main.rs:718`（Explorer 重建）、`src/main.rs:910` 与 `src/main.rs:1139`（DPI 事务、恢复事务）。
非生产消费者：`src/window.rs` 的 `#[cfg(test)]` 只覆盖 `position_flags` 与位置缓存失效（`src/window.rs:441-455`），不覆盖几何推导；`src/smoke.rs:116` 的 `build_and_embed_attaches_to_current_taskbar` 只断言 `is_embedded()` 与 `GetParent == 当前任务栏`（`src/smoke.rs:119-127`），两个断言在坏几何下**同样成立**，故这套唯一的真 Explorer 冒烟抓不到本缺陷。

产品面：`README.md:107` 声明支持「Windows 10/11 64 位」，`README.md:47` 声明嵌入任务栏托盘左侧。Windows 10 允许把任务栏移到左/右/上，是该系统的一等设置；因此这是**合法配置下的静默失效**，且 README 未记载该限制。

## 提案

在 `calc_widget_rect` 内、返回矩形之前加一条前置判据：任务栏矩形高度大于宽度即视为竖排，直接返回 `None`。

`None` 已接进既有失败路径：`src/window.rs:240-241` 会把它转成 `"找不到 Shell_TrayWnd 或 TrayNotifyWnd"` 错误，启动首轮嵌入弹一次提示（`src/main.rs:377`），此后由 `src/window.rs:364` 的 `reembed_if_lost` 每 2 秒静默重试（间隔 `src/config.rs:82`）。因此用户把任务栏移回底部后组件会自行恢复，**无需新增提示机制、无需新增状态位**。

判据抽成纯函数（例如 `is_horizontal_taskbar(rect: RECT) -> bool`）以便单测；该函数的唯一生产消费者就是 `calc_widget_rect`。

`README.md:47` 补一句限制说明（仅支持底部横向任务栏）。

## 明确不在本次范围

**不做四方向几何兼容**（左/右/上任务栏的坐标换算与布局）：170x32 横条在竖排任务栏上没有合法放置位置，用户已按 ROI 明确否决；本篇只消除「静默错版」，不扩大支持范围。

**不引入 `SHAppBarMessage`/`ABM_*` 系列 API**：那会与任务栏争夺屏幕边缘的工作区归属，属架构级改动。

**不改 `src/window.rs:311` 的事后断言**：`GetParent == h_taskbar` 是「是否真的换了父窗口」的唯一可信判据（`SetParent` 返回值有歧义，见 `src/window.rs:253-258`），几何判据必须加在它之前，不得把两者合并或删掉其一。

**不动 `reembed_if_lost` 的静默重试语义**（`AGENTS.md`：同一失败序列只提示一次、周期重试必须静默）。

**不处理多显示器副任务栏**（`Shell_SecondaryTrayWnd`）：`src/window.rs:195` 只找主屏 `Shell_TrayWnd`，扩展多屏属独立候选，本篇不改检索入口。

## 为什么不保留？

最强的反方理由是「这是为没有产品归属者的场景预留的分支」：绝大多数机器上是横向任务栏，判据等于一条永不触发的兜底，按简化审计应删。

逐条回应：第一，触发条件是用户可见的系统设置，不是推测性场景——`README.md:107` 已声明支持 Windows 10，而 Win10 的任务栏位置设置无需第三方工具即可到达。第二，不修就违反仓库自己的不变量（`AGENTS.md` 要求最终状态是**可见** child HWND、不得留半嵌入），而违反的表现是 `EMBEDDED` 谎报为真，比直接失败更难排查。第三，它不新增状态、不新增调用链：判据是函数内一个前置 `return None`，净增约 3 行，删掉它不会让任何调用链变短。

结论：本篇不是「预留通用性」，而是把一个**已存在的错误状态**改成显式失败。若未来审计判定该判据可删，必须先证伪「Win10 竖排任务栏可达」与「负坐标分支会被父窗口裁掉且 `EMBEDDED` 仍置真」这两条事实。

## 验收标准

`grep -n "is_horizontal_taskbar" src/window.rs` 有命中；`grep -rn "is_horizontal_taskbar" src` 的引用只出现在 `src/window.rs`。

新增单测位于 `src/window.rs` 的 `#[cfg(test)]`，用例名 `test_calc_widget_rect_rejects_vertical_taskbar`：构造高度大于宽度的 `RECT`，断言判据为假。

弱测试自查：把判据实现替换为恒 `true`（等价于恢复当前行为），上述用例必须变红。

默认门禁数量不变：`cargo test --locked` 输出 `110 passed; 0 failed; 4 ignored`（实施前基线为 `109 passed`，新增本用例后 +1），其中 `window::tests::dpi_dirty_position_keeps_window_size` 与 `window::tests::invalidate_last_rect_clears_committed_cache` 必须仍通过。

四条门禁按 `AGENTS.md` 逐条执行：`cargo fmt -- --check`、`cargo test --locked`、`cargo build --release --locked`、`cargo clippy --all-targets --locked -- -D warnings`。

## 风险

判据用「高 > 宽」识别竖排，依赖 `GetWindowRect` 在自动隐藏、任务栏预览等状态下给出的取值；若某状态下矩形比例异常，本可嵌入的横向任务栏会被误拒（表现为组件不出现 + 一次错误提示）。该残余风险无法在无真机矩阵的前提下证伪，故判据要保守（仅在高度显著大于宽度时拒绝），实施时需在横向与竖排任务栏上各实跑一次。

Windows 11 是否恢复任务栏位置设置属未验证行为；若恢复，本判据会把该配置变成「拒绝嵌入 + 一次提示」而非继续静默错版——这是可接受的方向，但需在 README 的限制说明里写清「仅支持底部横向任务栏」。
