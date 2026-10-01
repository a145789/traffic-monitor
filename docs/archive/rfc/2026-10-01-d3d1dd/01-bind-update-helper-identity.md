# Agent Note：给安装交接副本绑定启动对象身份

Status: rejected（复核裁决：攻击窗口毫秒级、需本地攻击者卡位竞态，无用户可感知收益，不实施）

## 问题

`src/update/installer.rs:406-454` 的 `spawn_install_helper` 把协调者自身 exe 复制到 `%LOCALAPPDATA%\Traffic Monitor\traffic-monitor-update-helper.exe`（路径见 `src/update/cache.rs:18-25` 与 `:35-37`），随后直接 `Command::spawn` 执行它：关键三行是 `src/update/installer.rs:439` 的 `remove_file`、`:440` 的 `std::fs::copy(...).and_then(|_| command.spawn())`。复制完成后 `fs::copy` 的内部句柄已关闭，副本路径在「复制成功」与「CreateProcess 打开映像」之间是一段**无任何保护**的时间窗；重试回路（`src/update/installer.rs:435-453`）在瞬态占用时还会以 `INSTALLER_LAUNCH_RETRY_DELAY_MS`（`src/config.rs:133`，400ms）重来，把窗口按轮次放大。

这段时间窗内，同用户的任何进程都能对该路径写入、改名或替换，而**执行前没有任何身份校验**：没有哈希（对比安装包走 `src/update/installer.rs:131-149` 的锁定句柄重算 SHA-256）、没有签名、没有属主/ACL 判据。全仓检索 `spawn_install_helper` 命中 2 处（`src/update/installer.rs:406` 定义、`src/update/mod.rs:613` 生产调用），`get_update_helper_path` 命中 2 处（`src/update/cache.rs:35` 定义、`src/update/installer.rs:408` 调用）；该函数**没有任何测试**，即非生产消费者为空。

这是本次交接改造新引入的唯一「用户可写目录里的可执行体」执行点，也是全仓唯一一处「先写盘再执行、却不绑定身份」的动作。

## 提案

1. 复制成功后立刻用**同款只读共享句柄**押住副本，直到 `command.spawn()` 返回：复用 `src/update/cache.rs:39-44` 的 `open_locked_installer`（`FILE_SHARE_READ_ONLY = 0x1`，见 `src/update/cache.rs:15`），把 `let _ = std::fs::remove_file(...)` → `fs::copy` → 加锁 → `command.spawn()` 收进同一作用域，spawn 返回后再 drop。重试回路的下一轮在 `remove_file` **之前**必须先 drop 旧句柄，否则自己的删除会被自己拒绝。
2. 共享模式必须与安装包保持一致（只读共享），理由是同一个：映像加载器以 `FILE_SHARE_READ|FILE_SHARE_DELETE` + `GENERIC_READ` 打开，不与只读共享句柄冲突 —— 这条并存性已由 `src/update/installer.rs:54-57` 的注释在安装包路径上论证并被既有升级路径使用。
3. `unsafe`/注释按 `src/update/installer.rs:127-130` 对安装包用的同一句话写清真正的不变量：**持有该路径的只读共享句柄期间，任何第三方都拿不到写/删除/改名所需的访问权，因此 `CreateProcess` 打开的映像与刚 `fs::copy` 写下的内容是同一对象**。不要把理由写成「路径是我们自己拼的」。

## 明确不在本次范围

- 不改安装包那条既有的锁定句柄链（`src/update/installer.rs:131-149`、`:204-284`、`:529-552`）——它已经正确，本节只补副本这一条。
- 不改副本落盘目录与残留清理门控（`src/update/cache.rs:60-74`、`src/config.rs:190-197`）：目录选在 `%LOCALAPPDATA%` 是与安装包缓存共用，换目录是另一笔改动（且要重评 `%TEMP%` 的权限面）；清理竞态见 `07-close-update-handoff-races.md`。
- 不引入 Authenticode 签名与 `WinVerifyTrust`（见 `04-anchor-installer-trust-with-signature.md`）——本节的成本上限是一行加锁，签名是另一层锚点。
- 不改副本的 `CREATE_NO_WINDOW` / `Stdio::null()`（`src/update/installer.rs:424-429`）：这是防止继承协调者 stdout 管道的必要设置，与身份无关。

## 为什么不保留？

1. 「副本是 medium IL，替换它拿不到提权，不值得修。」—— 只在协调者未提权时成立。`src/main.rs:247-292` 的自去提权有明确失败分支，`:290-291` 逐字写着「经 explorer 中转未成功接管，**继续以提权身份运行**」；此时协调者 spawn 的副本继承 high IL，副本被替换即本地提权。把安全性建立在「另一个功能永远成功」之上，正是本仓反复反对的写法。
2. 「窗口只有微秒级，攻击者赢不了。」—— 复制与 `CreateProcess` 之间没有任何同步原语；目录监视器（`ReadDirectoryChangesW`）能在事件到达时立刻写入，命中不需要运气，而重试回路最多把窗口放大到 3×400ms。
3. 「杀软实时扫描/权限会挡住替换。」—— 这不构成设计保证，且本仓对安装包正是用「锁定句柄」而不是「指望第三方不写」来表达这条不变量（`src/update/installer.rs:137` 的注释：对已锁定句柄哈希，验后换文件无窗口）。
4. 「加锁会让副本起不来（映像加载器要写访问）。」—— 反证在仓内：安装包以完全相同的共享模式持锁启动（`src/update/installer.rs:54-57` 的论证 + `:550` 的 drop 时机），既有升级路径依赖它工作。

## 验收标准

- `grep -n "open_locked_installer" src/update/installer.rs` 命中 ≥ 2 处（既有重验 + 新增副本绑定）。
- 新增确定性用例：把「删残留 → 复制 → 加锁」抽成可测函数，断言**加锁后以写方式打开同一路径必须失败**（`ERROR_SHARING_VIOLATION`，判据与 `src/update/installer.rs:456-466` 的 `is_transient_sharing_error` 同源）；不依赖真实竞态与真机。
- `cargo fmt -- --check`、`cargo test --locked`、`cargo build --release --locked`、`cargo clippy --all-targets --locked -- -D warnings` 全绿；既有 `update::installer::tests::test_cached_reuse_accepts_matching_locked_content`、`test_cached_reuse_rejects_tampered_content`、`test_verify_failure_distinguishes_open_from_content` 不回归。
- 真机走一次完整升级（1.7.3 → 下一版本），确认副本仍能启动、安装成功、组件带新版本号回到任务栏（`src/smoke.rs:22-27` 的人工清单已含这条）。

## 风险

- 若加锁真的挡住 `CreateProcess`，交接退化为「副本起不来 → 恢复主程序」，用户看到「更新没装成、组件回来了」。该风险由「与安装包同款共享模式、且该模式已被既有升级路径使用」压低，但**本机无 ISCC、无真机升级环境，无法在此验证**：这是本项最主要的未验证项。
- 复制前若攻击者已把 helper 路径预置为目录或 junction，`fs::copy` 会失败并走 `Err` 分支（`src/update/installer.rs:444-447`），不执行、不损坏，但会多一次失败恢复；属可接受。
- 本项不校验「副本内容是不是主程序的映像」——副本来自 `src/update/installer.rs:407` 的 `current_exe()`，即正在运行的映像，内容身份由此天然绑定；不需要也不能用哈希替代（哈希的期望值无处可锚，除非引入签名，见 04）。写下这条是为了挡住未来「顺手对副本自身算一次哈希」的误改方向。
