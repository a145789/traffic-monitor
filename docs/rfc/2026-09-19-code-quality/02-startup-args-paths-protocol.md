# Agent Note：启动参数与更新路径无损化及协议结构体收敛

Status: proposed

## 问题

三处同属“启动与更新交接面”的表达力加健壮性问题共生：其一，`src/main.rs:111` 用 `env::args()` 收参，任一参数非法 Unicode 即 panic，且 `--quit`、`--check-update`、`--manual`、`RELAUNCHED_BY_UPDATE_ARG` 在 `src/main.rs:111-121,220` 分四次线性扫描加字符串比较，无单一事实来源，`--quit` 先于 `--check-update` 的优先级只是 if 顺序里的隐式知识；其二，路径经 `to_string_lossy()` 有损中转（`src/update/mod.rs:94` 的 `LOCALAPPDATA` 回退、`src/update/mod.rs:623` 的重拉起、`src/update/mod.rs:838` 的安装器启动，另 `src/tray.rs:238` 同类调用一并扫），非 UTF-8 路径下安装包写对了地址却以被 U+FFFD 替换的地址启动，失败码完全误导；其三，`src/update/mod.rs:709` 的 `scan_subprocess_protocol` 返回 `(Option<UpdateAction>, bool, bool, bool)`，三布尔位置不可辨，而同文件 80 行外已有带文档注释的 `SubprocessOutcome` 结构体，一模块两种风格。

## 提案

一次改完三处，同一 PR 内三个独立提交：提交一，在 `main.rs` 新增 `CliArgs { quit, check_update, manual, relaunched_by_update }` 并以 `args_os().skip(1)` 加 `OsStr` 等值比较一次性解析，`main()` 开头显式 `if cli.quit { quit_existing_instance(); return; }` 再 `if cli.check_update { exit(subprocess_main(cli.manual)) }`，优先级由代码顺序钉死并补一条 `--quit` 与 `--check-update` 共存的组合单测；提交二，在 `util.rs` 新增与 `to_wide` 并列的 `os_to_wide(&OsStr) -> Vec<u16>`（`encode_wide` 直转加尾 NUL），三处调用换无损路径（`get_temp_installer_path` 改 `var_os` 加 `PathBuf::from`，`tray.rs:238` 同标准处置），常规路径输出逐字节不变；提交三，新增 `ScanOutcome { action, exit_signalled, read_failed, exit_forwarded }` 带文档注释替换 4 元组，`src/update/mod.rs:928-946` 的 9 个 `Cursor` 喂流协议测试只把元组解构断言改字段访问，逻辑一行不动。

## 明确不在本次范围

单例 Mutex 拦截顺序不动（`--check-update` 拦截必须仍在加锁前，见 AGENTS.md 第 4 条隐式约束）；不引入 clap/thiserror；不动 `SubprocessOutcome`；`ImmDisableIME` 时点不动；`CREATE_NO_WINDOW` 归 03 号 RFC。

## 为什么不保留？

最强反方是“A2 触发概率极低，不值得碰更新链”。回应：改动是分配次数不变的有损转无损，且三处恰是更新失败最难排查的路径（错误码误导），一次收敛永久免除该类现场排查；次强反方是“`CliArgs` 为四个布尔建结构体小题大做”。回应：结构体买的不是性能（启动期一次性）而是优先级显式化加组合单测落点，否则下次加第五个参数仍是第五次扫描。

## 验收标准

`cargo test --locked` 全绿且新增组合语义测试（`--quit` 与 `--check-update` 共存时走退出分支）通过、9 个 `scan_subprocess_protocol` 测试语义不变；`grep -rn "env::args()" src` 零命中（只剩 `args_os`）、`grep -rn "to_string_lossy" src` 路径类调用零命中或逐条豁免注释；`cargo build --release --locked`、`cargo clippy --all-targets --locked -- -D warnings`、`cargo fmt` 全绿；常规 ASCII 路径的更新流程行为不变（有意的行为变化仅限非 UTF-8 路径从损坏变正确）。

## 风险

残留风险两处：一是 `OsStr` 等值比较对 `--quit=1` 这类缀接形式仍判否，与今天 `==` 语义一致故非回归，但需在组合测试里把“精确匹配”语义钉死；二是 `ScanOutcome` 字段名与调用点旧注释错位，证伪方式为 diff 评审逐字段对照 `src/update/mod.rs:946` 现有解构顺序（`(_, exit_signalled, _, forwarded)`），字段映射错即打回。
