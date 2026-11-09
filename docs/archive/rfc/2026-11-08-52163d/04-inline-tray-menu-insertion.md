# Agent Note：拆除 tray 菜单的两阶段 MenuEntry 机制，改为直接插入

Status: implemented

## 问题

`src/tray.rs:131`-134 的 `MenuEntry` 枚举与 `src/tray.rs:136`-182 的 `build_menu_entries` 为一个固定 6 项的菜单（标题、分隔线、开机自启、自动检查更新、检查更新、退出）建了一套数据驱动中转：先构造 `Vec<MenuEntry>`，再在 `show_context_menu`（`src/tray.rs:224`-232）遍历 match 插入。检索 `grep -rn "MenuEntry\|build_menu_entries" src/` 命中 12 处全部在 tray.rs，枚举与 builder 只有 `show_context_menu` 一个消费者，没有第二个菜单复用该机制；`MenuEntry::Separator` 全仓库只有一个生产者（`src/tray.rs:144`）和一个消费者（`src/tray.rs:227`）。该机制建立在一个过度保守的生命周期前提上：枚举文档（`src/tray.rs:130`）称文本「由调用方持有至菜单关闭」，`src/tray.rs:223` 注释称「entries 须存活至 TrackPopupMenu 返回」；但 `insert_string_item` 的 SAFETY 注释（`src/tray.rs:194`）已按「text 在 InsertMenuItemW 同步返回前保持存活」书写——InsertMenuItemW 对 MIIM_STRING 文本是复制语义，菜单持有自己的副本，两处文档互相矛盾，前者是不必要的保守约束。

消费者分类：

- 生产消费者：`MenuEntry`/`build_menu_entries` 仅 `src/tray.rs:224`-231；四个 `MENU_ID_*` 各有构建点（`src/tray.rs:146`/`:155`/`:164`/`:177`）、处理分支（`src/tray.rs:266`-269）与触发路径（main.rs 的 WM_APP_TRAY → show_context_menu），均非冗余。
- 非生产消费者：无（tray.rs 无内联测试；`src/ffi_guard.rs:57` 注释仅提及 show_context_menu 无单测）。

## 提案

把 `build_menu_entries` 改为接收 `hmenu` 并逐项直接调用 `insert_string_item`/`insert_separator`（pos 递增计数器，顺序即唯一真源），每项的 `to_wide` 局部缓冲在同步插入调用期间存活即可；删除 `MenuEntry` 枚举、`Vec` 中转与 match 循环，修正 `src/tray.rs:130` 与 `:223` 的过度保守文档。补两个无头单测：菜单形状（项数/顺序/ID/文案/勾选态与命令路由对应）与文本复制语义（插入后覆写并释放源缓冲仍取回原文）。菜单外观与命令路由不产生任何变化。

## 明确不在本次范围

- `MENU_ID_*` 常量与 `handle_menu_command`（`src/tray.rs:264`-274）不动：每个 ID 都有真实处理分支与触发路径。
- TPM_RETURNCMD 先取命令、再还前台权给任务栏、最后执行的时序（`src/tray.rs:234`-255）不动：它是 AGENTS.md「更新弹窗不得回流常驻主进程初始化第三方 IME」不变量的承重实现。
- `MenuGuard`、`NIM_SETVERSION` 失败重试（`src/tray.rs:82`-85）与 `tray_owner`（已 `#[cfg(test)]` 门控）不动。
- 菜单项文案与状态源（`UPDATE_IN_PROGRESS`、`ENABLE_AUTO_UPDATE`、`is_autostart_enabled`）不动。

## 为什么不保留？

最强反方：① 现结构把全部文本缓冲的生命周期集中在一处（entries Vec），审查负担低；直接插入依赖「InsertMenuItemW 复制文本」这一 Win32 语义，需要评审确认；② 这是固定小菜单上的机制层，但也是风格边界案例，净删有限。回应：① 生命周期前提已被同文件 SAFETY 注释（`src/tray.rs:194`）按同步复制语义书写，集中持有至菜单关闭是多余的保守——直接插入让每项的真实（更短）生命周期就近显式，反而降低审查负担；复制语义已本机实测并由常驻单测锁定（见风险段），不再依赖文档措辞。② 判据针对的正是「没有第二个消费者的推测性通用机制」：枚举 + Vec + match 循环服务于一个不存在的复用场景，删除后菜单定义与插入动作合一，6 项菜单一目了然。

## 验收标准

- `grep -rn "enum MenuEntry\|MenuEntry::" src/` 0 命中。
- `cargo fmt -- --check`、`cargo test --locked`、`cargo build --release --locked`、`cargo clippy --all-targets --locked -- -D warnings` 全过且无 warning。
- 人工冒烟（真实 Explorer）：右键托盘图标，6 项菜单完整显示；开机自启与自动检查更新的勾选态与注册表/全局标志一致；更新进行中时「检查更新中...」置灰；逐项点击触发对应命令（自启切换、自动更新切换、手工检查、退出）。

## 风险

残留风险已关闭：复制语义在本机（Windows 11）实测成立，并由 `src/tray.rs` 的 `inserted_menu_text_is_copied` 用例常驻锁定——插入后覆写源缓冲为全 X 再释放，GetMenuItemInfoW 两段式取回仍为原文（含 CJK 与长串）。官方 InsertMenuItemW/MENUITEMINFO/SetMenuItemInfoW 文档对 `dwTypeData` 无逐字「复制」承诺，但均无缓冲存活期要求（对比 ListView `LVITEM.pszText` 有显式存活期条款），两段式取文本协议与复制语义一致。菜单形状（项数/顺序/ID/文案/勾选态）由 `menu_shape_matches_command_routes` 用例锁定。本提案不改任何命令路由，回归面限于菜单构建一处。