# Agent Note:收口 CreateMutexW 单例判别协议的双份实现

Status: proposed

## 问题

`src/main.rs:300`-`329` `init_single_instance` 与 `src/update/protocol.rs:286`-`308` `acquire_named_mutex` 各自内联实现了同一套 Win32 协议:`encode_utf16` → `CreateMutexW(None, true, ...)` → Ok 分支紧接读 `GetLastError` 判 `ERROR_ALREADY_EXISTS`(命中则自行 `CloseHandle` 并返回 `None`,否则把句柄交给 `MutexGuard`)→ Err 分支按各自语义处置后返回 `None`。两处还各带一份内容相同的协议注释——「勿改为读返回的 Err:ERROR_ALREADY_EXISTS 是这条 API 文档指定的带外输出,命中『已有实例』时 CreateMutexW 恰恰返回 Ok,Err 分支上根本没有它」(`src/main.rs:308`-`310` 与 `src/update/protocol.rs:297`-`299`)。同一协议知识两份表示,未来改一处漏一处即 drift(例如有人只在一处补上 `SetLastError(0)` 前置或改错判读方向,另一处不会跟着变)。

检索证据(2026-09-30,`grep -rn "CreateMutexW" src/`,生产调用点恰两处):`src/main.rs:304`(主进程单例锁)与 `src/update/protocol.rs:290`(更新子进程互斥)。生产消费者:`init_single_instance` 由 `src/main.rs:382` 调用(主进程启动);`acquire_named_mutex` 经 `src/update/protocol.rs:277` `acquire_update_mutex` 由 `src/main.rs:348` 调用(`--check-update` 分支)。非生产消费者:`src/update/protocol.rs:721`-`749` `test_named_mutex_reports_busy_then_releases` 直接调 `acquire_named_mutex`(测试专属互斥量名)——这正是它已把 `name` 参数化而 `init_single_instance` 没有的原因。

## 提案

把协议体收口为一份参数化实现,建议放 `src/util.rs`(与既有的互斥量存在性探测 `main_instance_exists`/`wait_main_instance_appear` 同处):`fn acquire_exclusive_mutex(name: &str, on_create_error: impl FnOnce(windows::core::Error)) -> Option<MutexGuard>`,函数体吸收两处的公共协议(encode_utf16 → CreateMutexW → 带外 ALREADY_EXISTS 判定 → 重复时 CloseHandle 返回 None / 否则交 MutexGuard → Err 时调用 `on_create_error` 后返回 None)。`init_single_instance` 与 `acquire_update_mutex` 变为薄调用:失败处置(单例锁的 `show_error` 用户文案、更新互斥的 `log_event`)以闭包留在各自调用点。协议注释(带外错误码判定、勿读 Err)只保留新函数里的一份,SAFETY 说明合并重写为覆盖两个调用点的版本。`test_named_mutex_reports_busy_then_releases` 改调新函数,断言原样。预计净删约 15-20 行,另收口一份协议注释。

## 明确不在本次范围

`MutexGuard`(`src/ffi_guard.rs:18`-`34`)不动:它只管「已取得的句柄 → CloseHandle」的 RAII 配对,与「如何向 Win32 创建并判重」的协议正交;把构造协议塞进守卫会违反 `src/ffi_guard.rs:1`-`4` 模块头「仅收口裸句柄且无业务构造逻辑的类型」的定位,那是个有意的接缝划分。`main_instance_exists` / `wait_main_instance_appear`(`src/util.rs:181`-`218`,OpenMutexW 存在性探测)不动:那是「打开既有互斥量」的探测协议,与「创建并判重」是不同 API、不同判据,合并两者会把「创建」与「探测」两种语义搅进一个函数。`quit_existing_instance` 的 FindWindowW 轮询(`src/main.rs:177`-`183`)与 `wait_main_instance_gone`(`src/update/installer.rs:242`-`280`)不动:三处轮询的探针、错误分类(后者有 ACCESS_DENIED 保守等待与日志去重)与返回语义实质不同,合并需造「轮询器」抽象,属推测性通用性;`src/config.rs:134`-`139` 的注释已声明该组等待只收口了常量对、未收口循环的既有取舍。

## 为什么不保留?

最强反方一:两处失败处置语义刻意不同——单例锁创建失败是致命错误(弹框后 `main` 直接退出,`src/main.rs:321`-`327`),更新互斥量创建失败按 BUSY 静默收尾(`src/update/protocol.rs:271`-`275` 注释详述了为什么「无法确认独占时继续空跑更糟」);合并会模糊这个差异。回应:处置差异正是闭包参数,保留在各自调用点——它本来就该在调用点;被收口的只是「如何向 Win32 询问是否重名」这一机械协议,它不该有两份。最强反方二:净删行数不大,收益主要是注释收口,不值一篇改动。回应:这里的 drift 代价不在行数而在正确性——协议要点是「Err 分支上没有 ERROR_ALREADY_EXISTS 可读」,两份注释意味着未来读者可能只看到其中一份;且 `acquire_named_mutex` 已为测试参数化了 `name`,`init_single_instance` 是唯一没走参数化的那份,合并是顺理成章的收尾而非新抽象。最强反方三:`MUTEX_NAME` 常量含尾 NUL(`src/config.rs:26`)而测试名用 `format!` 显式补 NUL(`src/update/protocol.rs:724`),两种输入形状不同。回应:统一 `encode_utf16` 后两者都以 NUL 结尾,`CreateMutexW` 读到首个 NUL 即停止,形状差异不构成行为差异——现状两处就是这么用的,合并不改变任何一方的输入。

## 验收标准

`grep -rn "CreateMutexW" src/` 的生产调用点从 2 处收口为 1 处(新函数内部);`cargo fmt -- --check`、`cargo test --locked`、`cargo build --release --locked`、`cargo clippy --all-targets --locked -- -D warnings` 四条门禁全部通过;`test_named_mutex_reports_busy_then_releases` 断言原样通过(仅改被调函数名);手工逐字对照两处失败文案不变——`init_single_instance` 失败仍弹 `src/main.rs:325` 的原文案(「无法启动程序:创建单例锁失败……」),`acquire_update_mutex` 失败仍写 `src/update/protocol.rs:293` 的原日志行(「创建更新互斥量失败: {e}」)。

## 风险

闭包捕获 `show_error` 文案时若顺手改写文案,会引入用户可见变化——验收标准已用「原文案逐字对照」钉住,可证伪。新函数带 `unsafe CreateMutexW`,SAFETY 注释需按仓库约定重写;若照抄两份旧注释会重新引入本次要消灭的重复,必须合并为一份同时覆盖「紧接调用读 last-error」「重复路径自行 CloseHandle」两个要点的说明。合并后 `init_single_instance` 的函数体只剩一个表达式,若有人进一步想把它内联进 `main`,会丢失「单例锁失败=致命退出」这一具名语义——本提案刻意保留两个薄包装函数名作为语义锚点。
