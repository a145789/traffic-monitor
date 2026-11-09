# Agent Note：清除 collector/main 三处微冗余——每 tick 表的 thread_local、不可达判空、二次日志开关重载

Status: implemented

## 问题

三处独立的小体量真冗余，合计净删约 8 行。其一（无跨 tick 状态的 thread_local）：`src/collector/network.rs:41` 的 `CURRENT_DATA` 是 `RefCell<HashMap>` thread_local，检索 `grep -rn "CURRENT_DATA" src/` 命中仅 2 处（定义 `:41`、唯一使用 `:107`-108），无测试消费；该表每 tick 经 `fill_candidates` 的 `out.clear()`（`src/collector/network.rs:158`）清空重填，从不跨 tick 携带数据，thread_local 唯一作用是复用 16 桶容量（1 Hz 下一次小分配），却换来一层闭包嵌套与一次 RefCell 运行时借用。其二（永不触发的兜底）：`MibTable::rows` 的 `if self.0.is_null() { return &[]; }`（`src/collector/network.rs:55`-57）不可达——`MibTable` 唯一构造点 `src/collector/network.rs:104` 之前已在 `:100` 判空早退（`grep -rn "MibTable(" src/` 命中 3 处：`:51` 类型声明、`:75` FreeMibTable 子串、`:104` 构造，其中构造点仅一处），SAFETY 注释（`:58`-59）自证「唯一构造点已判空，无其他构造路径」，运行时检查与已声明的不变量重复。其三（无消费者的中段二次加载）：`src/main.rs:490` 的 `refresh_debug_log_flag()` 与主进程入口 `:414` 重复；检索 `grep -rn "refresh_debug_log_flag" src/` 全量命中 11 处：`src/util.rs:476` 定义、`:464` 文档提及，`src/main.rs:60` import、`:365`/`:397`/`:414`/`:490` 四处调用、`:412` 注释提及，`src/update/mod.rs:32` import、`:379`/`:418` 两处调用。其中 365→mod.rs:379 与 397→:418 是两对同进程二次加载（局部注释 `src/main.rs:363`-364、`:395`-396 自认刻意幂等），并非独立进程入口；490 既不是入口也不覆盖任何日志窗口——414 与 490 之间无任何写 EnableDebugLog 的路径，且自动关在窗口内不可能触发（阈值 3，`src/config.rs:59`；窗口内日志点仅 de_elevate_self 的互斥分支，每次运行至多 2 条，recovery tick 在 `:467` 才 arm、尚未首发），故该重载无任何行为收益，只与 `src/util.rs:464`「主进程与更新子进程入口各加载一次」的口径相悖。

消费者分类：

- 生产消费者：`CURRENT_DATA` 仅 `collect_network` 自身；`rows()` 判空分支无生产者（不可达）；490 调用无独立消费者（冗余重载）。
- 非生产消费者：三者均无测试消费（network.rs 测试用局部 HashMap 与 INTERFACE_HISTORY；无测试构造 null MibTable；无测试断言 490 的重读行为）。

## 提案

动作 A：`CURRENT_DATA` 降为 `collect_network` 内的 `let mut current_data = HashMap::with_capacity(16);`，删 thread_local 声明与 `CURRENT_DATA.with`/`borrow_mut` 外层（`with_virtual_blacklist` 闭包保留）；diff 会重排闭包体约 25 行缩进，净删行数不变。动作 B：删 `rows()` 的判空早退三行，SAFETY 注释原样保留（它已声明非空不变量）。动作 C：删 `src/main.rs:490`，收掉 `:411`-413 注释里「与尾段同名调用幂等、可安全重复」一句，使主进程只在入口 414 加载一次。三个动作可独立实施。

## 明确不在本次范围

- `INTERFACE_HISTORY`、`VIRTUAL_BLACKLIST`、`FALLBACK_ACTIVE`（`src/collector/network.rs:42`-48）是真跨 tick 状态，thread_local 承重，不动；「表面写法对称」不是保留 CURRENT_DATA 的理由，也不是删它们的理由。
- `MibTable::Drop` 的判空（`src/collector/network.rs:72`）保留：释放路径保守防御，FreeMibTable(null) 语义无文档保证，属 AGENTS.md 资源配对释放接缝，不按不可达删。
- 「每周期只选一张合格接口、不累加、不跨周期粘滞赢家」的采集不变量与 `fill_candidates`/`select_winner_interface` 判据不动。
- main.rs:365→update/mod.rs:379 与 :397→:418 两对同进程二次加载不在本次展开：局部注释（main.rs:363-364/:395-396）自认刻意幂等，且 365 有真实覆盖窗口（acquire_update_mutex 失败留痕须早于 subprocess_main 自身加载）；是否收敛属另行决策。
- 注册表唯一真值源、自动关阈值与「注册表值本身不动」语义（`src/util.rs:463`-466）不动。

## 为什么不保留？

最强反方：① CURRENT_DATA 保留容量复用避免每秒一次堆分配，且与另两个 thread_local 写法对称；② rows() 判空防御未来新增构造路径，删掉后若有人传 null 即 UB；③ main.rs:412 注释明示幂等重复，且 365/397 两对同类二次加载被保留并有注释背书，说明作者接受这类防御性重载。回应：① 1 Hz 下 16 桶小分配无文档佐证其收益，且 `fill_candidates` 内 `eligible` Vec 每 tick 本就分配（`src/collector/network.rs:161`），「零分配」声称不成立；对称只是表面——另两者有跨 tick 状态，CURRENT_DATA 没有，局部变量让「每 tick 表」的属主关系可见。② 私有类型 + 单构造点 + SAFETY 注释已声明不变量，与本仓库其它 unsafe 块「不变量担保而非运行时复查」的写法一致；未来新增构造点时 SAFETY 注释要求作者维持判空，构造点数量可 grep 审查（`MibTable(` 当前 3 命中，仅 `:104` 是构造）。③ 365 有真实覆盖窗口（互斥量失败留痕须早于 subprocess_main 的加载）、379/418 是函数入口，两对二次加载至少有「第一处日志前加载」的理由；490 既非入口也不覆盖任何窗口，是无消费者的纯中段重载。「复活开关」边缘（refresh 重读注册表并清零计数，util.rs:476-482）理论存在但近乎不可达——阈值 3 对窗口内不足 3 条的互斥日志点，不作为删除理由；删除理由是设计口径（入口各加载一次）加无消费者、无行为收益。

## 验收标准

- `grep -rn "CURRENT_DATA" src/` 0 命中；`grep -n "is_null" src/collector/network.rs` 仅剩 `:72`（Drop）、`:100`（构造点）、`:260`、`:309` 四处。
- `grep -n "refresh_debug_log_flag" src/main.rs` 命中 `:60` import 与 `:365`/`:397`/`:414` 三处调用（`:412` 注释与 `:490` 调用一并收掉），主路径 `:414` 之后不再有重载。
- `cargo test --locked` 全绿：network.rs 接口判据矩阵与 WWAN 回归测试、util.rs 日志开关相关测试原样通过。
- `cargo fmt -- --check`、`cargo build --release --locked`、`cargo clippy --all-targets --locked -- -D warnings` 全过且无 warning。

## 风险

残留风险：A 每 tick 多一次小分配——证伪依据：1 Hz 频率与既有 eligible Vec 分配使影响不可测。B 未来新构造点传 null 即 UB——缓解：SAFETY 注释明示构造点判空义务，构造点数量可 grep 审查；Drop 判空保留兜住释放侧。C 删除不改变任何可达行为：414→490 窗口内自动关不可能触发（阈值 3、互斥日志点不足 3），「复活」边缘近乎不可达；若未来出现需要中段重载的日志窗口，那是新设计，应显式加回并写明理由，而非依赖这条无消费者的重载。