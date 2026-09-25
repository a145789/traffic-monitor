# Agent Note：把「主进程不加载网络 DLL」与 unsafe 注释要求变成可执行断言

Status: proposed

## 问题

第一处，核心隔离不变量目前没有任何验证，且门禁自己在报这件事。`AGENTS.md` 第 4 条要求长期主进程不得因更新功能加载网络/加密依赖，`build.rs:36-38` 用 `/DELAYLOAD` 把 `winhttp.dll`、`bcrypt.dll`、`bcryptprimitives.dll` 排除出标准导入表。但默认门禁 `cargo test --locked` 会输出：`warning: linker stdout: LINK : warning LNK4199: 已忽略 /DELAYLOAD:winhttp.dll；未找到来自 winhttp.dll 的导入`，并且只对 winhttp 报、不对 bcrypt 报。含义是：测试二进制里根本不存在 `winhttp.dll` 的导入，因为没有测试调用 `src/update/http.rs` 的 WinHTTP 路径（该文件仅 1 条用例 `test_winhttp_error_code_mapping`，`src/update/http.rs:319`，是纯错误码映射）。因此「主进程不加载网络 DLL」这条产品级不变量，在门禁里没有任何东西能证明它成立，而 `AGENTS.md` 又要求人工审查门禁输出中的 warning。

第二处，`unsafe` 注释里有一批正是 `AGENTS.md` 点名的反例句式。规范原文要求注释说明「真正支撑安全性的不变量（指针/句柄有效性、缓冲区生命周期、NUL 终止、别名、内存布局或失败歧义），不能只复述『句柄来自成功的 OS API』」。检索 `grep -rn "SAFETY:" src` 共 130 处命中，其中以下 6 处以「来源/有效性」替代了真正的不变量：`src/ffi_guard.rs:11`（`CreateMutexW 成功创建的互斥量句柄`）、`src/ffi_guard.rs:23`（`CreatePopupMenu 成功创建的菜单句柄`）、`src/update/http.rs:25`（`句柄来自成功的 WinHTTP API 调用，均为有效指针`）、`src/update/crypto.rs:15`（`句柄来自成功的 BCrypt API 调用，均有效`）、`src/renderer.rs:331`（`均为有效 GDI 句柄`）、`src/renderer.rs:395`（`均为有效 DC`）。同一仓库里存在写对的对照样本：`src/update/protocol.rs:390`、`src/update/protocol.rs:403`、`src/update/protocol.rs:434`、`src/suspend.rs:242` 都写出了所有权与「只释放一次」。两种温度并存说明这不是理解问题，而是 review 未落地。

第三处，`src/ffi_guard.rs` 是全仓唯一没有任何测试的 RAII 基建（28 行、0 条用例），而它的三个生产消费者都在关键路径上：单例互斥量（`src/main.rs:254-271`）、更新互斥量（`src/update/protocol.rs:276-305`）、托盘菜单（`src/tray.rs:207`、`src/tray.rs:222`）。它的 `Drop` 是「exactly-once 释放」这条 `AGENTS.md` 不变量的唯一载体，却没有一条用例钉住它。

## 提案

为 `winhttp`/`bcrypt` 的隔离不变量加一条可执行检查：对 release 产物读取导入表，断言其中不出现 `winhttp.dll` 与 `bcrypt.dll`，作为 CI 步骤或 `scripts/` 下的检查脚本。注意 `/DELAYLOAD` 的语义正是「这两个 DLL 不进标准导入表」，所以断言与 `build.rs:36-38` 是同一事实的两端，不冲突。

把上列 6 处 `unsafe` 注释改写成真正的不变量：说明句柄的所有权归属、谁释放、释放几次，而不是复述它由哪个 API 返回。

给 `src/ffi_guard.rs` 的两个 `Drop` 各补一条用例，钉住「恰好关闭一次」。

## 明确不在本次范围

**不得因为 LNK4199 说「winhttp 没被导入」就删掉 `build.rs:36-39` 的 `/DELAYLOAD` 指令**：该警告出现在**测试二进制**（测试不调用 WinHTTP 路径），而 release 二进制仍带这些导入，`/DELAYLOAD` 正是为它服务的。这条警告是要被本篇消掉的现象，不是可删冗余的证据。

**不改 `src/renderer.rs:246` 与 `src/renderer.rs:447-449` 的注释**：那两处断言的失效由「DPI 资源事务」篇随行为变更一起重写，本篇只处理行为不变的注释，避免两篇笔记改同一行。

**不把全仓 130 处 `SAFETY` 注释一起重写**：本篇只处理上面点名的 6 处（按 `AGENTS.md` 点名的反例句式逐字判定），其余保持不动。

**不做代码签名、不改 CI 权限与 action 引用形式**：属独立候选。

**不改 `src/renderer.rs:488-489` 的 `Drop` 中刻意忽略的 `SelectObject` 返回值**：那里是还原 stock 对象，失败仅影响 `DeleteDC` 能否释放，`src/renderer.rs:481-484` 已说明该取舍。

## 为什么不保留？

最强的反方理由是：注释改写与新增断言都不改变运行行为，属文档面与测试面的改动；而导入表检查是「没有生产消费者」的 CI 步骤，按简化审计属可删表面积。

逐条回应：第一，release 为 `panic = "abort"`（`Cargo.toml:12`），不变量失守没有 unwind 兜底，`unsafe` 注释是安全论证的唯一载体，它不是文档装饰而是约定的一部分——`AGENTS.md` 把它与内存安全并列。第二，导入表检查的生产消费者是产品承诺本身（`README.md:10` 的「轻量高效」）与 `AGENTS.md` 第 4 条的隔离不变量，而它取代的是当前**完全不存在**的验证；这不是「为测试而存在」，而是把一个已被声明但不被检查的约束接上检查。第三，6 处注释与 2 条用例不新增任何运行期符号——用例只观测既有 `Drop`，断言只观测既有产物，删掉它们不会加快任何生产路径。

结论：本篇的目标不是「多写文档」，而是把两条**已经存在于规范、且已有违规证据**的约束变成会红的东西。若未来审计要删导入表断言，必须先证伪「主进程不加载网络依赖」仍是现行不变量（见 `AGENTS.md` 第 4 条与 `README.md:10`）。

## 验收标准

`grep -rn "成功创建的" src` 为 0 命中；`src/update/http.rs` 与 `src/update/crypto.rs` 内
`grep -n "句柄来自成功的"` 为 0 命中（全仓 blanket 为 0 不成立：`src/update/protocol.rs:434`
是带「唯一持有者」的所有权合格样本，不在本篇范围）；上列 6 处注释改写后仍为 `// SAFETY:` 前缀（保持既有形式）。

导入表检查可执行且对当前 release 产物给出「不含 `winhttp.dll`、不含 `bcrypt.dll`」的结论；实现方式自选（`dumpbin /imports` 或等价的产物检查脚本），但不得为此在 `Cargo.toml` 引入新的运行时依赖。

`src/ffi_guard.rs` 的 `MenuGuard` 至少 1 条用例（`test_menu_guard_drop_runs_exactly_once`），
`MutexGuard` 的释放断言位于 `src/update/protocol.rs` 的 `test_named_mutex_reports_busy_then_releases`
（该用例是默认门禁下 `MutexGuard` 的唯一构造点，且末尾“释放后可重取”同时证明 OS 层释放）；
两者都覆盖「创建即持有、`Drop` 恰好执行一次」。`MutexGuard` 不在 `ffi_guard` 内另设用例的原因：
`protocol` 的占用测试并发构造同一守卫，计数断言放在别处会竞态；这是有意为之，不是覆盖缺口。

弱测试自查（本条最关键）：把 `Drop` 整体中和（删除计数自增与其配对的 `CloseHandle`/`DestroyMenu`），
上述用例必须变红。判据是测试专用的 `#[cfg(test)]` 释放计数（`MUTEX_GUARD_DROPS`/`MENU_GUARD_DROPS`，
release 无此符号），不是句柄重操作探针——后者已证伪：二次 `CloseHandle` 探针在默认并行下约 2/16 轮
假红（句柄槽复用），且复用时会关掉他人的活句柄；`GetHandleInformation` 非破坏性但同受复用影响。
测试专用面的代价已登记：两处 `Drop` 内各一行 `cfg(test)` 自增，`ffi_guard` 头两处静态量。

默认门禁数量只增不减（起点 `108 passed; 0 failed; 4 ignored`），四条门禁全绿。

## 风险

导入表检查依赖 `dumpbin`（MSVC 工具链）或等价实现，CI 上需先确认可用；若要避免额外工具，也可在 `scripts/package.ts` 侧以产物文本级检查代替，但那会把该断言绑在打包路径而非门禁上，覆盖范围变小——两种方案都需在实施时确认其残余覆盖，并如实记录。

为 `Drop` 写「可观测是否已关闭」的用例，可能需要引入测试专用的构造或探针路径。按简化审计判据，测试专用的表面积本身就是候选，故实施时应优先用标准手段（对已关闭句柄再操作并断言失败）而不新增测试专用函数；若确实无法避免，需在实施时把该测试专用面登记为已知代价。

LNK4199 是否在补上导入表断言后从门禁输出中消失，取决于断言实现位置（脚本 vs CI 步骤 vs 测试内部）：本笔记只要求「该现象被断言覆盖或显式记入已知警告清单」，不承诺警告本身一定消失。
