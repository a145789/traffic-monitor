# Agent Note：删除主窗口不可达的 WM_COMMAND 分支与启动路径无效果托盘清理
Status: proposed

## 问题

`src/main.rs` 的窗口过程与启动编排各有一处死代码。第一处是 `wnd_proc` 的 `WM_COMMAND` 分发臂（`src/main.rs:509-513`，连带 `src/main.rs:30` 的 `WM_COMMAND` 导入）：托盘菜单的唯一构造点是 `src/tray.rs:166-216`，`TrackPopupMenu` 明确带 `TPM_RETURNCMD | TPM_NONOTIFY`（`src/tray.rs:195`），命令 ID 由返回值直接带回并在 `src/tray.rs:213-215` 分发，`TPM_NONOTIFY` 语义下系统根本不会为此菜单投递 `WM_COMMAND`；全仓检索 `WM_COMMAND` 仅命中 `src/main.rs:30,509,511` 三处（含该臂自身一处与该导入一处，无任何发送方，窗口是嵌入任务栏的 `WS_CHILD` 且无菜单无加速键，不存在其他生产者），`handle_menu_command` 的检索仅命中定义（`src/tray.rs:218`）、活调用（`src/tray.rs:214`）与死调用（`src/main.rs:511`）三处，因此该分支永不触发，还迫使 `handle_menu_command` 保持 `pub` 可见性。第二处是 `Renderer::new` 失败臂里的 `remove_tray_icon()`（`src/main.rs:186`）：启动顺序是 `Renderer::new`（`src/main.rs:182-189`）先于 `bind_display_and_timers`（`src/main.rs:191`，其内部 `src/main.rs:304` 才首次调用 `create_tray_icon`），fresh 进程的 `TRAY_DATA` 初值为 `None`，而 `remove_tray_icon`（`src/tray.rs:72-81`）对 `None` 直接跳过，故该调用恒为无操作，还会让读者误以为此时托盘图标已存在。

## 提案

删除 `src/main.rs:509-513` 的 `WM_COMMAND` 分支共 5 行，`src/main.rs:30` 去掉 `WM_COMMAND,` 导入（净 0 行），`src/tray.rs:218` 的 `pub fn handle_menu_command` 收窄为 `fn`（仅剩模块内调用 `src/tray.rs:214`）；删除 `src/main.rs:186` 的无效果 `remove_tray_icon();` 一行；合计净删约 6 行加一处可见性收窄，行为无变化（删的是从未被取到的分支与恒空操作的调用）。

## 明确不在本次范围

`LOWORD_MASK`（`src/main.rs:516` 的托盘事件 `WM_APP_TRAY` 解析仍用）不得一起删；`remove_tray_icon` 的实现与三处真实调用（`src/main.rs:363` 重建路径、`src/main.rs:502` 的 `WM_CLOSE`、`src/update/mod.rs:653`）不动，本条只删 `src/main.rs:186` 这一个无效果调用点；托盘菜单构造、TPM 前台权交还任务栏逻辑（`src/tray.rs:191-209`，受保护接缝）不动；`handle_menu_command` 末尾的 `_ => {}` 通配臂是 Rust 穷尽匹配必需的样板，不动。

## 为什么不保留？

反方一：留着 `WM_COMMAND` 臂做防御，未来菜单改回非 RETURNCMD 或 OS 投递杂散 `WM_COMMAND` 时仍能处理，成本仅 5 行。回应：flag 硬编码在相邻的 `src/tray.rs:195`，改 flag 必联动 review，不存在静默失效；死分支反而误导读者以为命令走消息循环，掩盖真正的 RETURNCMD 直接分发路径，去误导的收益明确大于 5 行防御价值。反方二：`src/main.rs:186` 的清理是防御性预埋，若未来调整顺序（托盘先建、渲染后建）或复用该错误臂时可兜底，且幂等无操作留着无害。回应：顺序若真调整，编译器不会提醒补回这行，靠预埋做防御并不可靠，应在调整时再加；且旧臂对低 16 位不等于四个菜单 ID 的杂散命令落到 `handle_menu_command` 的 `_` 忽略分支，与删后落到 `DefWindowProcW` 一致；唯一行为差异是低字恰为 `MENU_ID_AUTOSTART`/`MENU_ID_EXIT`/`MENU_ID_AUTO_UPDATE_TOGGLE`/`MENU_ID_CHECK_UPDATE_MANUAL`（`src/config.rs:97-100`）的杂散命令——旧臂会真触发对应动作、删后不触发——但该输入不可达：全仓无 `WM_COMMAND` 生产者（检索仅自身 3 处引用，亦无 `0x0111` 硬编码与 `SendMessageW`），窗口是 `hMenu` 为空、无控件无加速键的嵌入子窗口，故差异只存在于不可达输入上，不构成“删分支改行为”。

## 验收标准

`grep -rn "WM_COMMAND" src` 零命中（`WM_CLOSE`、`WM_CONTEXTMENU` 等不受影响）；`grep -rn "handle_menu_command" src` 仅剩 `src/tray.rs:218` 定义与 `src/tray.rs:214` 调用两处；`grep -rn "remove_tray_icon" src` 命中 6 行且全部为预期：定义 `src/tray.rs:72`、import `src/main.rs:48` 与 `src/update/mod.rs:41`、调用 `src/main.rs:363,502` 与 `src/update/mod.rs:653`（不再有其它调用点）；`cargo test --locked`、`cargo build --release --locked`、`cargo clippy --all-targets --locked -- -D warnings` 全绿；托盘菜单四个命令（开机自启、自动检查更新、手动检查更新、退出）手动验证行为不变。

## 风险

真实残留风险接近零：若未来 Win32 向该 `WS_CHILD` 窗口投递杂散 `WM_COMMAND`，此前臂内行为是对任意 ID 调 `handle_menu_command`（非四个菜单 ID 即落 `_` 忽略），删后行为是 `DefWindowProcW` 默认处理，两者仅在“低字恰为四个菜单 ID 之一”时有别，而该输入今日不可达（无生产者、窗口无菜单与控件），故不构成可观察差异；`handle_menu_command` 由 `pub` 收窄为私有后，若未来有跨模块调用方会出现编译错误（fail-fast，非静默退化），属期望的显式卡点而非风险。
