# Agent Note：删除网速采样路径上与纯函数重复的脚手架状态

Status: proposed

## 问题

`collector/network.rs` 的每 tick 采样路径维护了多处冗余状态，它们要么与同一文件里的纯函数职责重复，要么是同行数据的镜像，要么是无法到达的兜底。以下四条均为一手核实（`grep` 全文搜索 + 逐行读）。

**一、首采样基线分支与 `rate.rs` 的纯函数重复。** [`collector/network.rs`](../../../src/collector/network.rs) 用一个独立线程标志 [`NET_INITIALIZED`](../../../src/collector/network.rs) 守着一整段"首次采样只记基线、不算速率"的分支（`network.rs:99` 的 `if !NET_INITIALIZED.load(..)` 到 `:111` 的 `return`），里面手工遍历历史表建基线并 `store(true)`。但同一职责已经由 [`select_winner_interface`](../../../src/collector/rate.rs) 承担且行为更好：它用"历史里有该 LUID 才计算速率"表达同一语义（`rate.rs:22` 的 `if let Some(&(prev_in, prev_out, prev_time)) = history.get(luid)`），并**无条件用本次采样覆盖历史**（`rate.rs:37-40`）、同时清理已离线 LUID（`rate.rs:42`）。该函数已有专门测试钉死首次出现的零速语义（[`test_select_winner_interface_first_appearance`](../../../src/collector/rate.rs) 断言首见网卡返回 `(0, 0)` 但数据已入历史）。

也就是说：**初始化的真正属主是 `rate.rs`（有测试），而 `network.rs` 又用 `AtomicBool` 加 13 行平行实现了一遍（无任何测试覆盖该分支）**——`grep` 全文仅 3 处命中（`network.rs` 25/99/109），无测试、无文档消费者。两处表示长期存在分叉风险：改一处基线语义而漏改另一处，不会编译失败，也没有测试报警。

**二、`has_up_interface` 是 `!current_data.is_empty()` 的逐字镜像。** [`has_up_interface`](../../../src/collector/network.rs) 在 `network.rs:76` 声明、`:94` 置位、`:120` 读取；置位语句 `:94` 与同分支体的 `:95 current_data.insert(luid, ..)` 处于同一个 `if row.OperStatus == IfOperStatusUp` 块内，且该块内无 `continue`/`return`——因此"标志为真"与"`current_data` 非空"严格等价，标志不承载任何额外信息。`grep` 全文仅 3 处命中，全在本文件，无测试依赖。

**三、黑名单缓存的 `None` 兜底不可达。** [`with_virtual_blacklist`](../../../src/collector/network.rs) 在 `network.rs:267-271` 保留了一条"重建成功或失败回退后缓存必然为 `Some`；该分支只是类型层面的空名单兜底"的注释与分支。该注释判断正确，可以证明：`VIRTUAL_BLACKLIST` 初值为 `None`（`network.rs:30`）→ [`blacklist_needs_refresh(None, now)`](../../../src/collector/network.rs) 返回 `true`（`network.rs:277` 的 `is_none_or`）→ 必然进入 [`rebuild_virtual_blacklist`](../../../src/collector/network.rs)，而其两条分支都落 `Some`（成功写 `Some((set, now))`，失败要么刷新时间戳、要么写 `Some((HashSet::new(), now))`，见 `network.rs:288-297`）→ `needs_refresh` 只比对时间戳、不再依赖 `Option`。故 `cache.as_ref()` 恒为 `Some`，`:268-271` 那段空名单兜底不可达。

**四、三条断言不区分实现，只提供虚假保险。** 测试行占全仓 1006/4940 行（约 20%），其中数条把「注释里声称保护的性质」与「断言实际验证的东西」写脱节了——把被测代码换成注释所指的错误实现，断言照样通过：

1. [`test_instant_saturating_duration_since_does_not_panic_on_time_regression`](../../../src/collector/rate.rs)（`rate.rs:127-135`）的 `assert_eq!(t.saturating_duration_since(t).as_millis(), 0)` 用同一个 `Instant` 自比。`duration_since` 在同参数下同样返回 0 且不 panic，所以该断言无法区分 `saturating_duration_since` 与注释要防的 `duration_since`。测试自己的注释已承认这一点（"无法直接构造 now < prev 的 Instant……真正差异在逆转行为，此处至少锁定 API 选择不被误改回 duration_since"）——它锁不住。
2. [`test_hash_hex_case_insensitive`](../../../src/update/crypto.rs)（`crypto.rs:125-132`）对同一个哈希串断言 `hash.to_uppercase() == hash.to_lowercase().to_uppercase()`。两边都是标准库方法，该等式与 [`format_hex`](../../../src/update/crypto.rs) 无关；同文件更强的大写已知答案用例 [`test_sha256_known_answer`](../../../src/update/crypto.rs) 已覆盖大小写性质，本条是冗余的标准库自证。
3. [`test_scan_garbage_lines_forward_nothing_and_remember_nothing`](../../../src/update/mod.rs)（`update/mod.rs:847-855`）向 `scan()` 喂入 `b"NO_UPDATE\nEXIT_MAIN|extra\n\n"` 并断言解析结果为空。由于协议只定义 `DONE` / `EXIT_MAIN` 两种动作，该断言对任何实现都成立；真正有意义的流式转发不变量已由同文件 [`test_scan_exit_main_forwards_exactly_once`](../../../src/update/mod.rs)（`update/mod.rs:821-828`）与 [`test_scan_duplicate_exit_main_forward_only_once`](../../../src/update/mod.rs)（`update/mod.rs:830-836`）覆盖，且文件注释明写"不变量由本模块 tests 以 Cursor 喂协议行钉死"。

注意本条不是"删测试提覆盖率"：第 1 条的 `elapsed_ms == 0` 兜底另有真实覆盖（[`test_normalize_zero_elapsed_does_not_panic`](../../../src/collector/rate.rs) 断言 `normalize_bytes_per_sec(5000, 0) == 5_000_000`，能真正区分 `max(1)` 与零除 panic），第 2 条的大小写性质由已知答案用例覆盖，第 3 条的转发不变量由两条流式用例覆盖。

## 提案

1. **删除首采样基线分支与 `NET_INITIALIZED`。** 删掉 `network.rs:25` 的静态量与 `:99-111` 的整段分支，让首 tick 直接走 `select_winner_interface` 这条唯一路径；初始化语义继续由 `rate.rs` 的 `Some(history.get(..))` 表达。同时删除 `network.rs:5` 中仅为该标志导入的 `AtomicBool`（同行的 `Ordering` 仍被 `NET_SPEED_*` / `CONSECUTIVE_ZERO_COUNT` 使用，须保留）。分支内 `network.rs:103` 的 `history.clear()` 一并消失——它只在 `INTERFACE_HISTORY` 仍为空时执行（该表的另一个写者是 `select_winner_interface`，只在 `NET_INITIALIZED == true` 之后被调用），是自证死代码；删掉它也顺带消除了"未来在别处新增历史写者会被首 tick 清空"的隐患。
2. **`has_up_interface` 换成 `current_data.is_empty()`。** 删 `network.rs:76` 的声明与 `:94` 的赋值，把 `:120` 的条件改为 `best_speed_down == 0 && best_speed_up == 0 && current_data.is_empty()`（`current_data` 的可变借用持续到 `:133`，且 `:115` 已在用 `&current_data`，改动与借用结构相容）。
3. **黑名单缓存改用「值 + 可选时间戳」表示。** 把 `VIRTUAL_BLACKLIST` 的元素类型由 `Option<(HashSet<u64>, Instant)>` 改为 `(HashSet<u64>, Option<Instant>)`，`blacklist_needs_refresh` 用 `last.is_none_or(|t| now.saturating_duration_since(t).as_secs() >= BLACKLIST_REFRESH_SECS)` 表达"从未刷新即需刷新"，[`with_virtual_blacklist`](../../../src/collector/network.rs) 退化为无分支、无 `Option` 解包的 `let (list, _) = &*cache; f(list)`。现有四个缓存语义单测（`network.rs:404-464`）的断言意图保持不变，仅把构造 `None` 的写法改为构造 `(HashSet::new(), None)`。
4. **清理三条不具区分力的断言，并把第 3 条改造成有区分力的流式用例。** 删除 `test_instant_saturating_duration_since_does_not_panic_on_time_regression` 与 `test_hash_hex_case_insensitive`；把 `test_scan_garbage_lines_forward_nothing_and_remember_nothing` 的输入改为把 `EXIT_MAIN` 放在无效行**之后**（如 `b"NO_UPDATE\nEXIT_MAIN|extra\nEXIT_MAIN\n"`），断言转发恰好发生一次——这样"无效行不阻断后续转发"才真正被钉死（旧断言在新旧实现下都通过）。同步修正 `rate.rs:58-59` 与 `rate.rs:100` 中把 `elapsed_ms == 0` 说成"覆盖时间逆转"的注释，改为陈述真实覆盖（零除兜底，时间逆转由 `saturating_duration_since` 饱和为 0 后走同一分支）。

## 明确不在本次范围

[`NETWORK_BACKOFF`](../../../src/state.rs) 布尔量与 `network.rs:122` 的 `!NETWORK_BACKOFF` 守卫**保留**。它们表面上是 `CONSECUTIVE_ZERO_COUNT >= BACKOFF_ZERO_THRESHOLD` 的派生量，但 `network.rs:121` 的自增在退避期间没有任何守卫——计数会继续增长，因此"达阈那一次才投递 `WM_USER_NETWORK_DISCONNECTED`"这一边沿检测必须由布尔量承载；把它换成纯派生比较会在每个后续零速 tick 重复投递断网消息，进而在 15s 退避间隔下反复重建定时器。本提案只碰采样路径上的脚手架状态，不动退避状态机。同理保留 [`CPU_INITIALIZED`](../../../src/collector/cpu_mem.rs)：它不给基线打标记的话，首轮 `kernel_diff`/`user_diff` 会相对 0 基线求差（`cpu_mem.rs:44-46`），把 CPU 显示成接近 100%。

## 附带：同批可清的零散项（与上面各条无耦合，可单独取舍）

[`Cargo.toml`](../../../Cargo.toml) 第 25 行的 `"Win32_System_ProcessStatus"` feature 无任何消费者：全仓 `ProcessStatus` 仅此一处命中，`src/` 内 `GetProcessMemoryInfo` / `EnumProcesses` / `PROCESS_MEMORY_COUNTERS` 均零命中，`Windows::Win32::System::ProcessStatus` 路径也从未出现（其余 20 个 feature 逐一核对均有 `src/` 读者，其中 `Win32_Security`、`Win32_System_Registry`、`Win32_Networking_WinSock` 属传递性必需——后两者分别门控 `ShellExecuteExW` 与 `GetAdaptersAddresses`，不可删）。删掉这一行即少编译整个 ProcessStatus 模块进 `windows` crate，零行为风险。注意 `Win32_System_Registry` 必须保留：它不是为了注册表 API 而开，而是 `windows` 0.62 中 `ShellExecuteExW` / `SHELLEXECUTEINFOW` 的 cfg 门控（`Cargo.toml` 现有注释已记录这一点）。

**更新子系统：临时安装包的缓存复用是一段带冗余探针的三级删除级联。** [`do_update_check`](../../../src/update/mod.rs) 在 `update/mod.rs:309-332` 用 `if temp_path.exists()`（`:310`）作为外层探针，内层再套哈希比对与加锁两次判断，而三个失败分支体逐字相同、都只是 `let _ = std::fs::remove_file(&temp_path)`（`:323`、`:327`、`:330`）；同一个删除动作在 `:367` 还有第 4 次重复。外层探针是冗余的：`compute_sha256_hex_file`（[`crypto.rs:81-82`](../../../src/update/crypto.rs)）内部就是 `File::open` + `read`，文件不存在时直接返回 `Err` 走 `:329` 分支，`open_locked_installer`（`update/mod.rs:123`）同样以 `Err` 表达缺失。改成"复用条件成立就返回，否则统一删一次"后，24 行可收为约 8 行，`:367` 的删除因为前面的统一清理而一并消失；该段无测试覆盖（`mod.rs` 的测试只覆盖 `parse_update_action` / `scan_subprocess_protocol` / `is_transient_launch_error`），因此行为等价性只能靠手工核对——这正是它值得先收缩再谈测试的原因。行为语义需逐字保留的两点：哈希不匹配必须删除旧文件而不能复用，以及"加锁失败也要删除后重下"。

## 为什么不保留？

最强的反方理由有三条，逐条回答。

"首采样分支是显式初始化，删掉后靠 `rate.rs` 的空历史副作用，语义变隐晦。" 但语义并没有变隐晦，只是搬到了真正的属主那里：`select_winner_interface` 的"无历史即零速"是**被单测钉死的显式契约**（`test_select_winner_interface_first_appearance`），而 `network.rs` 那份 13 行平行实现在整个仓库里没有任何测试覆盖——把初始化交给有测试的一方，可靠性是上升而非下降。保留两份表示的真实代价是分叉面：基线语义改一处漏一处不会报错。

"`has_up_interface` 是 O(1) 标志，`is_empty()` 要在循环里维护 map 长度。" 两者都是 O(1)：`HashMap::is_empty` 读的是已维护的 `len` 字段，而标志本身需要一个独立分支目标与一次额外写。镜像标志唯一确定的作用是增加一处双写漂移面，这正是 [`state.rs`](../../../src/state.rs) 已经用注释记录过的同类问题。

"`Option` 兜底只有三行，删了划算吗？" 单看第三项收益最低（生产侧净删约 5 行），它在本提案里属于"顺手动同一文件时一并做"的一档；真正的价值是把"缓存必然已构建"这一不变量从注释提升为类型（`(值, Option<时刻>)` 无需解包即可用），顺带消掉那行 `else { return f(&HashSet::new()); }` 的临时空集分配。若评审认为不值得，只删第 3 项而保留 1、2、4 项，提案仍成立。

## 验收标准

`grep -n "NET_INITIALIZED" src/` 与 `grep -n "has_up_interface" src/` 均无命中；`grep -n "Option<(HashSet<u64>, Instant)>" src/` 无命中；`grep -n "test_instant_saturating_duration_since\|test_hash_hex_case_insensitive" src/` 无命中；`network.rs` 中 `history.clear()` 无命中。行为一致性：`rate.rs` 首见网卡用例、`test_immersive_color_*` 之外的全部 `network.rs` 过滤与缓存语义用例（`test_is_valid_interface_*`、`test_virtual_name_*`、`test_blacklist_*` 共 16 例）全绿；改造后的流式用例在有区分力的输入下通过，且在旧实现（丢弃无效行后不再转发）下会失败——这是它相对原用例的增量保护。手动验证：断网 ≥5 秒后采样间隔切到 15s 且只弹一次退避状态切换，恢复网络立即回到 1s 且数值无虚假峰值；接着确认首 tick 速率仍显示 0（不出现基于 0 基线的假峰值）。门禁：`cargo test`、`cargo build --release`、`cargo clippy -- -D warnings`、`cargo fmt` 全绿。

## 风险

已证伪的风险：有人会担心"删掉 `NET_INITIALIZED` 后首 tick 的速率是垃圾值"。不会——首 tick 的 `INTERFACE_HISTORY` 为空，`select_winner_interface` 对每个 LUID 都取不到历史，返回 `(0, 0)`，写入 `NET_SPEED_DOWN`/`NET_SPEED_UP` 的就是零；旧分支则是在 `return` 前压根不写这两个原子量，而二者初值本就是 0，因此**显示结果完全相同**，差别只是"显式写入零"与"保持初值零"（后者仅在启动首个 tick 成立，不影响任何后续 tick）。首 tick 唯一的行为差异是：进 else 分支而非 `return`，因此无网卡场景下 `CONSECUTIVE_ZERO_COUNT` 提前一个 tick 开始增长，退避从第 6 秒提前到第 5 秒触发。`BACKOFF_ZERO_THRESHOLD` 为 5 tick，结论（进入退避）不变，只是早 1 秒。

真实残留风险：`CONSECUTIVE_ZERO_COUNT` 是无网卡/零流量时的计数（`network.rs:121`），本提案不动它的读写序（自增 Relaxed、恢复路径 Release 清零）——若要连带调整内存序，属于另一件事，不在本次范围。黑名单类型改造的风险仅在于 `blacklist_needs_refresh` 的边界语义（`>= 30s` 判过期）必须逐字保留，四个缓存单测正是为此存在。测试删除部分为零行为风险，仅涉及 `#[cfg(test)]` 代码。
