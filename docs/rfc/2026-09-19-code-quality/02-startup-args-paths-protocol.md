# Agent Note：启动参数与更新路径无损化及协议结构体收敛

Status: proposed

## 问题

三处同属「启动与更新交接面」的问题共生，级别为 A1【P1】、D4【P2】、A2【P2】、D2【P2】（**P1＝缺陷**：不修则在某条路径上出错；**P2＝高收益维护项**：不改不会错但持续付利息；严重度与执行顺序是两个独立维度）：其一，`src/main.rs:111` 用 `env::args()` 收参，其文档行为是任一参数非合法 Unicode 即 panic——本程序是 GUI 子系统且接受 `--quit` / `--check-update` 的 CLI 入口，安装器、脚本、用户手工传参都会到达这里，一个含非 UTF-16 可表示字符的无关参数（例如从旧代码页路径拖拽产生）就能让常驻进程带 panic 退出，对「常驻任务栏、更新流程依赖 CLI 协议」的程序这是必修项；同时 `--quit`、`--check-update`、`--manual`、`RELAUNCHED_BY_UPDATE_ARG` 在 `src/main.rs:111-121,220` 分四次线性扫描加字符串比较，无单一事实来源，`--quit` 先于 `--check-update` 的优先级只是散落在 if 顺序里的隐式知识（D4）；其二，路径经 `to_string_lossy()` 有损中转（`src/update/mod.rs:93-94` 的 `LOCALAPPDATA` 读取与 temp 回退、`src/update/mod.rs:623` 的重拉起、`src/update/mod.rs:838` 的安装器启动），该函数把无法解码的字符替换成 U+FFFD，用户名含旧代码页字符时安装包写到「正确」路径却以「被替换字符的路径」传给 `ShellExecuteExW`，启动失败且错误码完全看不出原因，`std::env::var` 同理会因非法 Unicode 返回 `Err` 而走 temp_dir 回退、LOCALAPPDATA 明明存在却当不存在（A2）；其三，`src/update/mod.rs:709` 的 `scan_subprocess_protocol` 返回 `(Option<UpdateAction>, bool, bool, bool)`，三个 `bool` 在位置上无法区分，而同文件 80 行外已有带文档注释的 `SubprocessOutcome` 结构体，同一个模块里两种风格并存是「多人协作痕迹」最明显的一处（D2）。

## 提案

一次改完三处，同一 PR 内三个独立提交：提交一（A1+D4），在 `main.rs` 新增 `CliArgs { quit, check_update, manual, relaunched_by_update }` 并以 `args_os().skip(1)` 加 `OsStr` 等值比较一次性解析（`args_os` 不 panic，也无需参数是合法 Unicode），`main()` 开头显式 `if cli.quit { quit_existing_instance(); return; }` 再 `if cli.check_update { exit(subprocess_main(cli.manual)) }`，优先级由代码顺序钉死并补一条 `--quit` 与 `--check-update` 共存的组合单测，`RELAUNCHED` 检查不再第三次扫描向量；提交二（A2），在 `util.rs` 新增与 `to_wide` 并列的 `os_to_wide(&OsStr) -> Vec<u16>`（`encode_wide` 直转加尾 NUL；Windows 上 `OsStr` 可无损转宽字符），三处调用换无损路径——`get_temp_installer_path` 改 `env::var_os` 加 `PathBuf::from`、回退分支直接取 `std::env::temp_dir()` 的 `OsString`，`relaunch_main_app` 与 `try_launch_installer` 改走 `os_to_wide`，常规路径输出逐字节不变；提交三（D2），新增 `ScanOutcome { action, exit_signalled, read_failed, exit_forwarded }` 带文档注释替换 4 元组，`src/update/mod.rs:928-946` 用 `Cursor` 喂流的 9 个协议测试只把元组解构断言改字段访问，测试逻辑一行不动。性能与内存：三处均不新增运行时分配——A1 是启动期一次性解析，A2 从「有损 String 中转」改为「无损 OsStr 直转」分配次数不变，D2 结构体零运行时成本。

## 明确不在本次范围

单例 Mutex 拦截顺序不动（`--check-update` 拦截必须仍在加锁前，否则子进程被当作重复实例直接退出，见 AGENTS.md 第 4 条隐式约束）；`update/mod.rs` 的安装包 TOCTOU 三层校验（锁句柄上重算哈希、写锁降级只读锁、错误分类决定是否回落代理，配篡改回归测试）不动——这是禁止倒退资产，不得以「整洁」之名简化；看门狗窗口与 `claim_exit_request` 退出幂等门（每条消息路由都注释了「为什么不能转发、为什么落点必须是看门狗」，幂等性做成纯函数并可测）不动；不引入 clap / thiserror（与零依赖定位冲突，四个布尔用不上）；不动 `SubprocessOutcome`；`ImmDisableIME` 时点不动；`CREATE_NO_WINDOW` 归 03 号 RFC。

## 为什么不保留？

最强反方是「A2 触发概率极低，不值得碰更新链」。回应：改动是分配次数不变的有损转无损，且这三处恰是更新失败最难排查的路径（错误码误导而非明确报错），一次收敛永久免除该类现场排查；据此定 P2 而非 P1——触发需路径含不可解码字符，现代 Windows 上概率很低，且失败模式是可见的错误弹窗而非崩溃或数据损坏。次强反方是「`CliArgs` 为四个布尔建结构体小题大做」。回应：结构体买的不是性能（启动期一次性，零影响）而是优先级显式化加组合单测的落点，否则下次加第五个参数仍是第五次扫描。需明确的是 A2 属**有意的行为变化**：非 UTF-8 路径从「损坏后失败」变「正确」，常规路径逐字节不变。

## 验收标准

`cargo test --locked` 全绿且新增组合语义测试（`--quit` 与 `--check-update` 共存时走退出分支）通过、9 个 `scan_subprocess_protocol` 测试语义不变；`grep -rn "env::args()" src` 零命中（只剩 `args_os`）；`grep -rn "to_string_lossy" src/update/` 零命中；`cargo build --release --locked`、`cargo clippy --all-targets --locked -- -D warnings`、`cargo fmt` 全绿；常规 ASCII 路径的更新流程行为逐字节不变（不变量核对单：优先级保持 `--quit` 先于 `--check-update`，常规路径输出不变）。

## 风险

残留风险两处：一是 `OsStr` 等值比较对 `--quit=1` 这类缀接形式仍判否，与今天 `==` 语义一致故非回归，但需在组合测试里把「精确匹配」语义钉死；二是 `ScanOutcome` 字段名与调用点旧解构顺序错位，证伪方式为 diff 评审逐字段对照 `src/update/mod.rs:743` 现有返回顺序 `(parsed_action, exit_signalled, read_failed, exit_forwarded)` 与 `src/update/mod.rs:946` 现有解构 `(_, exit_signalled, _, forwarded)`，字段映射错即打回。
