# Agent Note：DPI 资源事务的两处失败出口收敛到同一状态

Status: proposed

## 问题

第一处，资源交换不判别 `SelectObject` 返回值。`src/renderer.rs:419 update_dpi` 的四步里，创建位图（`src/renderer.rs:433-438`）与创建字体（`src/renderer.rs:442-445`）都判了失败并 `return false`，随后的两次交换却没有：`src/renderer.rs:450` 与 `src/renderer.rs:457` 直接取返回值交给 `src/renderer.rs:452`、`src/renderer.rs:459` 销毁，随后 `src/renderer.rs:454`、`src/renderer.rs:461` 无条件把 `self.hbitmap`、`self.hfont` 指向新对象。

`SelectObject` 失败（返回 NULL 或 HGDI_ERROR）时后果有三条：`DeleteObject` 成为空操作而旧对象仍被选在 DC 中；`self.hbitmap`、`self.hfont` 已指向**未被选入**的新对象，DC 真实状态与结构体记录分叉，后续 `src/renderer.rs:398` 的 `BitBlt` 会画进旧位图；旧对象句柄被覆盖后再无引用，`src/renderer.rs:479-484` 的 `Drop` 只删 `self.hbitmap`、`self.hfont`，于是**永久泄漏一个 GDI 对象**。

同一模式在初始构造处也存在：`src/renderer.rs:243` 与 `src/renderer.rs:247` 同样不判别返回值。区别是那里有显式声明——`src/renderer.rs:239` 写着「至此所有可失败步骤均已成功。后续选入/测量/配置均不会失败。」；而 `update_dpi` 侧没有这句话，取而代之的是 `src/renderer.rs:447-449` 的 `SAFETY` 注释，它断言「SelectObject 返回的 old_bitmap 是此前选入并被替换的 self.hbitmap，已脱离 DC 可安全 DeleteObject」——该断言只在调用成功时成立，代码没有验证它，`AGENTS.md` 的 unsafe 注释要求正是不允许以「来自成功的 API」代替真正的不变量。

检索证据：`grep -rn "SelectObject" src` 命中 6 处调用（`src/renderer.rs:243`、`247`、`450`、`457`、`488`、`489`），**无一处判别返回值**；`grep -rn "HGDI_ERROR|IsInvalidHandle" src` 命中 0 处。

第二处，DPI 事务的两条失败出口给出不同结论。`src/main.rs:1133-1146` 的 `WM_DPICHANGED` 分支：`dpi_updated == false`（资源创建失败）时执行 `rollback_window_to_bitmap` 并置 `DPI_DIRTY`（`src/main.rs:1144-1145`，命令与位语义见 `src/state.rs:58-70`）；而 `dpi_updated == true` 之后的一步是 `src/main.rs:1139` 的 `let _ = embed_in_taskbar(hwnd)`——资源已换新、嵌入失败的**错误被直接丢弃，且不置脏位**。结果窗口停在旧尺寸而位图已是新尺寸，`BitBlt` 只覆盖旧位图区域、边缘露出色键底色（该症状在 `src/renderer.rs:417-418` 有记载）。`src/main.rs:1139` 是全仓 98 处 `let _ =` 中吞掉核心动作失败的一处。

与 `AGENTS.md` 的冲突：DPI 不变量要求「任一步失败都必须保持旧资源与窗口尺寸匹配，并可由恢复路径重试，不能留下『新位图 + 旧窗口』或永久错版」。

## 提案

把四处交换收敛到同一个私有判别路径（例如 `fn select_or_fail(hdc: HDC, obj: HGDIOBJ) -> Result<HGDIOBJ, ()>`，或等价的就地判别），`update_dpi` 失败时返回 `false`——该返回值**已有既定语义**：`src/main.rs:1133-1146` 会回滚窗口并置 `DPI_DIRTY`，`src/main.rs:651-657`（`bind_display_and_timers`）同样会回滚并置位，调用方无需改动。`Renderer::new` 侧失败则返回 `Err`，走启动期既有错误路径（`src/main.rs:377`）。

`src/main.rs:1139` 改为在 `Err` 时置 `DPI_DIRTY`（`src/main.rs:1145` 已有同样的写法），使两条失败出口收敛到同一状态：**窗口尺寸与位图尺寸一致，或登记脏位等待 `recover_dpi` 重试**。`recover_dpi` 的四步流程（`src/main.rs:896`）不动。

## 明确不在本次范围

**不改 `src/renderer.rs:488-489` 的 `Drop` 中刻意忽略的 `SelectObject`**：那里是还原 stock 默认对象，失败仅影响 `DeleteDC` 能否释放，`src/renderer.rs:481-484` 已说明该取舍，属既有合理设计。

**不改 `src/renderer.rs:246` 与 `src/renderer.rs:447-449` 之外的注释**：其中 `447-449` 随本篇行为变更一并重写；`src/renderer.rs:246` 与 `src/renderer.rs:331`、`src/renderer.rs:395`、`src/ffi_guard.rs:11`、`src/ffi_guard.rs:23`、`src/update/http.rs:25`、`src/update/crypto.rs:15` 归「把 unsafe 注释要求变成可执行断言」篇，避免两篇改同一行。

**不清理全仓 98 处 `let _ =`**：本篇只处理 `src/main.rs:1139` 的**语义**问题（核心动作失败无人记账），不是风格清理。

**不改 `src/main.rs:896 recover_dpi` 的四步顺序**：那套顺序是「先对齐尺寸、再提交完整几何」的既有不变量，本篇只负责让进入它的入口状态一致。

**不改 `OwnedGdi` 的所有权模型**：既有的 owner 与 `into_raw` 纪律足够表达本次修复，不引入新类型。

## 为什么不保留？

最强的反方理由是：`src/renderer.rs:239` 已经显式声明「后续选入/测量/配置均不会失败」，作者对该段做了明确假设；在 `hdc_mem` 有效且对象刚由 `CreateCompatibleBitmap`、`CreateFontIndirectW` 成功创建的前提下，`SelectObject` 实务上不会失败，判别属于「不可达分支、永不触发的兜底」，按简化审计应删。第二个反方理由是：`src/main.rs:1139` 那条已有兜底——`embed_in_taskbar` 失败会把 `EMBEDDED` 清成假（`src/window.rs:238`），于是 `src/window.rs:364` 的 `reembed_if_lost` 会在 2 秒内重试重算几何（间隔 `src/config.rs:82`），「新位图 + 旧窗口」两秒后自愈，不必再置 `DPI_DIRTY`。

逐条回应：第一，判别成本是两行 `if`，而它保护的是会让 `self.hbitmap` 与 DC 状态**永久分叉**并泄漏句柄的路径；同一函数内创建侧已判别（`src/renderer.rs:433`、`442`），交换侧不判别是同一序列内的不一致——`OwnedGdi` 整套 RAII 的存在前提正是「创建成功但提交失败」这一中间态。第二，自愈只保证**几何**被重算，不保证 `DPI_DIRTY` 被登记，也不改变「两条失败出口对同一事实给出不同结论」这一缺陷本身；何况自愈前那两秒内 `BitBlt` 源/目标尺寸不一致是可见症状（`src/renderer.rs:417-418`）。第三，两处改动都不新增状态、不新增依赖，删掉它们不会让任何调用链变短。

结论：本篇不是「为不可达分支加固」，而是让**同一事务的两条失败出口收敛到同一状态**，并让 `SAFETY` 注释的断言由代码支撑。若未来审计要删判别，必须先证伪「`SelectObject` 可能失败」以及「失败会造成句柄泄漏与 DC/结构体状态分叉」。

## 验收标准

`grep -n "SelectObject" src/renderer.rs` 的每一处生产调用（`243`、`247`、`450`、`457`）其后 3 行内必须存在返回值判别（人工核对，`grep` 只能定位）。

新增用例名 `test_dpi_swap_rejects_failed_select`，位于 `src/renderer.rs` 的 `#[cfg(test)]`：以失效对象触发交换失败，断言 `update_dpi` 返回 `false` 且 `bitmap_size()`（`src/renderer.rs:474`）未变。

新增用例名 `test_dpichanged_embed_failure_sets_dpi_dirty`：断言嵌入失败后 `DPI_DIRTY`（`src/state.rs:70`）为真。

弱测试自查：把交换判别改回无条件提交（等价于恢复当前行为），上述两条用例必须变红。

行为一致性用现有用例点名，必须仍绿：`window::tests::dpi_dirty_position_keeps_window_size`、`window::tests::invalidate_last_rect_clears_committed_cache`、`util::tests::test_dpi_scaled_matches_legacy_across_dpi_range`。

默认门禁数量只增不减（起点 `108 passed; 0 failed; 4 ignored`）；四条门禁全绿。

## 风险

`SelectObject` 的失败注入需要真实 DC 与失效对象，最省的做法是传入一个已被 `DeleteObject` 的句柄；这会引入测试专用构造路径，而按简化审计判据「测试专用的表面积」本身就是可删候选。若无法在不新增测试专用函数的前提下构造该失败，本笔记退化为「只加判别 + 由 code review 承担不可达性论证」，并在实施时把该退化如实记录，不得用「不 panic 即通过」的用例充数。

置 `DPI_DIRTY` 会让 `src/window.rs:395` 的 `position_flags` 在脏位期间追加 `SWP_NOSIZE`，即恢复事务完成前定位路径不再改窗口尺寸。这是既有语义、不新增，但会改变「嵌入失败后那两秒内窗口是否尝试改尺寸」的行为，需用冒烟用例（`cargo test --locked -- --ignored --test-threads=1`，四条）确认视觉上无新症状。

给 `Renderer::new` 加判别会把「选入失败」从静默继续改为返回 `Err`，属启动期行为变更；其可见后果是启动错误路径（`src/main.rs:377`）多一种文案来源，需确认该路径的提示文案与既有中文约定一致（`AGENTS.md`：新增说明性用户文案默认使用中文）。
