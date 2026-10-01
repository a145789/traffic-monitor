# Agent Note：删掉空转的 HeapCompact 循环并修正其 unsafe 注释

Status: rejected（复核裁决：无用户可感知行为差异，属代码卫生，不实施）

## 问题

`src/util.rs:647-664` 的 `compact_and_trim` 在修剪工作集之前，先遍历进程内所有堆并逐一对它们调用 `HeapCompact`：`:654` `GetProcessHeaps(&mut [])` 取数量、`:656-657` 分配句柄数组并填充、`:658-660` 对每个句柄 `HeapCompact(*heap, HEAP_FLAGS(0))`、`:663` 最后 `trim_working_set()`。唯一生产调用点是 `src/update/mod.rs:214`（更新检查收尾），`grep -rn "compact_and_trim" src` 命中 2 处（`src/util.rs:647` 定义 + 该调用），没有测试，因此非生产消费者为空。

该循环的文档与 unsafe 注释宣称的效果与操作系统语义**相反**。MSDN 对 `HeapCompact` 的 Remarks 原文是：「the **HeapCompact** function returns the size of the largest free block in the heap but **does not compact the heap any further**」——只有在调试用全局标志 `Disable heap coalesce on free` 置位时它才真正压缩；而正常情况下系统在每次 `HeapFree` 时就已经合并空闲块。也就是说 `HeapCompact(flags=0)` 在这里既不合并、也不归还页，`src/util.rs:637-638`「先遍历进程内所有堆（含 UCRT malloc 堆）调用 `HeapCompact` 将空闲页 decommit 归还 OS」这句是**不成立的**：真正削掉本进程物理页的是紧随其后的 `trim_working_set()`（`src/util.rs:627-633`）。

同一循环还踩了 MSDN 对 `GetProcessHeaps` 的明确警告：「some of the private heaps retrieved by the function may have been created by other code running in the process and **may be destroyed after GetProcessHeaps returns** … continued use of such handles **can cause undefined behavior** … Heap functions should be called only on the default heap of the calling process and on private heaps that the process creates and manages.」而 `src/util.rs:651` 的 unsafe 注释只论证了「`HeapCompact(flags=0)` 使用默认序列化，对多线程安全」——序列化安全与**句柄有效性**是两件事，注释没有覆盖真正的风险来源。按 AGENTS.md「`unsafe` 注释必须说明真正支撑安全性的不变量」，这条注释是不合格的。

## 提案

1. 删掉 `src/util.rs:654-661` 的 `GetProcessHeaps` + `HeapCompact` 循环，以及 `src/util.rs:5` 的 `GetProcessHeaps, HEAP_FLAGS, HeapCompact` 导入；`compact_and_trim` 收敛为「`trim_working_set()` + 说明为什么不需要压缩堆」。
2. 把 `src/util.rs:637-638` 的说明改写成真实判据，并引用 MSDN 原文（不压缩、仅调试全局标志下才压缩），同时把「他方私有堆句柄可能已失效、不得对其调用堆函数」这条约束写成注释，防止未来有人重新加回这个循环。
3. 函数名与调用点保持不变（`compact_and_trim` 在本仓语义是「收尾修剪」，`src/update/mod.rs:214` 调用点语义不变），避免把一次删空转变成本模块之外的改动。

## 明确不在本次范围

- 不动 `trim_working_set`（`src/util.rs:627-633`）及其在挂起路径（`src/suspend.rs:76`）与初始化路径（`src/main.rs:789`）的调用。
- 不新增周期 trim：初始化修剪仍是一次性定时器（`src/config.rs:105-106` 的 `TIMER_ID_INIT_TRIM` / `TIMER_INTERVAL_INIT_TRIM`），周期 trim 会因反复 decommit/resume 造成工作集反弹，这条取向不在本次改动内。
- 不引入 `HeapSetInformation(HeapDeCommitFreeBlockThreshold)`：它是另一个动作（提高堆自行 decommit 的门限），需要真机内存观测支持，若将来测得收益再单独立项。
- 不动 `set_low_memory_priority`（`src/util.rs:385-399`）与 `configure_background_process`（`src/util.rs:408-426`）：内存优先级与堆压缩是两套机制。

## 为什么不保留？

1. 「多压一层没坏处，保守留着。」—— 它压的不是自己的堆：`GetProcessHeaps` 返回的句柄包含 UCRT 等**他方代码**创建的私有堆，MSDN 明确说继续使用这些句柄可能导致未定义行为。而且它本身是空转，不是「没坏处的保守」。
2. 「将来有人在 GFlags 里开了那个标志它就生效了。」—— 生产运行不设调试全局标志，不能把调试态行为当生产语义；即便设了，那也应该是显式的一次调试动作，不是常驻代码路径。
3. 「删了内存回落会变差。」—— 真正削物理页的是 `trim_working_set()`；`HeapCompact` 的返回值只是「最大已提交空闲块的大小」，调用它并不改变工作集。若真机数据反驳这一点，本节应被撤销（见风险）。
4. 「注释只是措辞问题，不值得立一篇笔记。」—— 本仓把 `unsafe` 注释当作安全论证的载体（AGENTS.md 明文），而这里的注释与 OS 语义相反、且未覆盖句柄失效，属于必须修的失实论证，不是措辞。

## 验收标准

- `grep -n "GetProcessHeaps\|HeapCompact" src` → **0 命中**（含导入行）。
- `grep -n "decommit" src/util.rs` → 0 命中；`src/util.rs` 中 `compact_and_trim` 的说明能逐句对上 MSDN 的 Remarks。
- 四条门禁全绿：`cargo fmt -- --check`、`cargo test --locked`、`cargo build --release --locked`、`cargo clippy --all-targets --locked -- -D warnings`（本函数无既有用例，故不涉及测试改名）。
- 行为面：`src/update/mod.rs:214` 的调用点与 `trim_working_set()` 的调用次数不变（仍为 1 次）。

## 风险

- 若真机测得「删除循环前/后工作集回落有差异」，说明「`HeapCompact` 在 flags=0 下会归还页」这一解读有误，本节应撤销。证伪依据：MSDN `HeapCompact` 的 Remarks 原文（已引）+ 同机同场景下任务管理器内存与 `%LOCALAPPDATA%\Traffic Monitor\debug.log` 的调用时序对比。
- 残留：`SetProcessWorkingSetSize(usize::MAX, usize::MAX)` 只把物理页退到 Standby，**不减少 committed 内存**；本项不改变这一点，原代码也没有改变它（`HeapCompact` 不改变提交量，只报告最大空闲块）。若产品目标是降 committed，那是另一个议题（堆门限或换分配器），本节不承诺。
