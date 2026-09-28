# Agent Note：DPI 资源事务的两处失败出口收敛到同一状态

Status: proposed

> 行号策略：本篇只保留「问题」一节里**实施前**的行号快照（那是缺陷证据，随 main 冻结）。
> 其余各节按函数名/消息名定位，不维护改后行号映射表——映射表本身就是漂移源。

## 问题

第一处，资源交换不判别 `SelectObject` 返回值。`Renderer::update_dpi`（实施前 `src/renderer.rs:419`）的四步里，创建位图与创建字体都判了失败并 `return false`，随后的两次交换却没有：直接把返回值交给 `DeleteObject`，随后无条件把 `self.hbitmap`、`self.hfont` 指向新对象。

`SelectObject` 失败（返回 NULL 或 HGDI_ERROR）时后果有三条：`DeleteObject` 成为空操作而旧对象仍被选在 DC 中；`self.hbitmap`、`self.hfont` 已指向**未被选入**的新对象，DC 真实状态与结构体记录分叉，后续 `BitBlt` 会画进旧位图；旧对象句柄被覆盖后再无引用，`Drop` 只删 `self.hbitmap`、`self.hfont`，于是**永久泄漏一个 GDI 对象**。

同一模式在初始构造处也存在（`Renderer::new`），区别是那里有显式声明——「至此所有可失败步骤均已成功。后续选入/测量/配置均不会失败。」；而 `update_dpi` 侧没有这句话，取而代之的 `SAFETY` 注释断言「`SelectObject` 返回的 `old_bitmap` 是此前选入并被替换的 `self.hbitmap`，已脱离 DC 可安全 `DeleteObject`」——该断言只在调用成功时成立，代码没有验证它，`AGENTS.md` 的 unsafe 注释要求正是不允许以「来自成功的 API」代替真正的不变量。

检索证据（改前）：`grep -rn "SelectObject" src` 命中 6 处调用，**无一处判别返回值**；`grep -rn "HGDI_ERROR|IsInvalidHandle" src` 命中 0 处。

第二处，DPI 事务的两条失败出口给出不同结论。`WM_DPICHANGED` 分支里 `dpi_updated == false`（资源创建失败）走 `rollback_window_to_bitmap` 并置 `DPI_DIRTY`；而 `dpi_updated == true` 之后的一步是 `let _ = embed_in_taskbar(hwnd)`——资源已换新、嵌入失败的**错误被直接丢弃，且不置脏位**。结果窗口停在旧尺寸而位图已是新尺寸，`BitBlt` 只覆盖旧位图区域、边缘露出色键底色。这是全仓 98 处 `let _ =` 中吞掉核心动作失败的一处。

与 `AGENTS.md` 的冲突：DPI 不变量要求「任一步失败都必须保持旧资源与窗口尺寸匹配，并可由恢复路径重试，不能留下『新位图 + 旧窗口』或永久错版」。

## 提案

**1. 判别收敛到唯一入口。** 新增私有 `select_or_fail(hdc, obj) -> Result<HGDIOBJ, ()>`，以 `HGDIOBJ::is_invalid` 覆盖 `SelectObject` 的两种失败返回（windows 0.62.2 `Graphics/Gdi/mod.rs:5410`，实现为 `self.0 == -1 || self.0 == 0`，即 HGDI_ERROR 与 NULL）。`Renderer::new` 与 `Renderer::update_dpi` 的**四处交换全部**改经它，四处交换本身不再直接调用 `SelectObject`；其余生产调用只剩两处刻意忽略（`Drop` 还原 stock）与一处已判别（`swap_dpi_objects` 内的回滚），见「验收标准」。失败不带负载——它只说得出「选入没成功」这一件事实，上下文文案由调用方补足。

**2. 多处交换共用一条回滚序。** 交换序列收口为 `swap_dpi_objects(hdc, new_bitmap, new_font)`，它是唯一持有回滚实现的地方：任一步失败都把刚换出的旧位图选回 DC（顺带让新对象脱离 DC），使 DC 状态回到调用前，`self.hbitmap` / `self.hfont`（启动期则是 stock 默认对象）无需改动即可继续成立，新对象也能被调用方的 `OwnedGdi` 守卫删掉。回滚方向不可调换：必须先让旧对象回到 DC，新对象才脱离 DC、`DeleteObject` 才可能生效。回滚自身失败时 `diag!` 留痕，不假装成功。

**3. 两条失败出口收敛到同一可观察事实**——「窗口物理尺寸 == 位图尺寸」，或登记脏位等待恢复事务提交新几何：

| 出口 | 处置 | 理由 |
| --- | --- | --- |
| `update_dpi` 失败（资源创建失败，或选入失败已回滚） | `rollback_window_to_bitmap` + 置 `DPI_DIRTY` | 位图仍是旧尺寸而窗口可能已按新 DPI 布局，错配仍在，需恢复事务提交新几何 |
| `WM_DPICHANGED` 嵌入失败（资源已换新） | 仅 `align_window_size_to` 对齐尺寸，**不置脏位** | 错配已归零；剩余「位置与分层属性未提交」归 `reembed_if_lost` 的 2s 守卫 |

**4. `recover_dpi` 的四步顺序不动**，`OwnedGdi` 的所有权模型不动。

### 嵌入失败为什么不置 `DPI_DIRTY`（实施中推翻本笔记原提案，理由如下）

原提案要求「`src/main.rs:1139` 改为在 `Err` 时置 `DPI_DIRTY`，使两条失败出口收敛到同一状态」。实施后发现该做法引入真实回归，故改为上表的分治，理由：

1. **错配已被压到零。** 剩余事实只有「位置与分层属性未提交」，它不属于 `DPI_DIRTY` 的生产语义（该位应收敛为单一含义：**位图/窗口错配待提交**）。
2. **补做嵌入本就是 `reembed_if_lost` 的职责域**（每 2s 静默重试）。`embed_in_taskbar` 一次调用即提交几何 + 分层属性，不需要恢复调度器再兜一遍。
3. **退避会连累无关自愈动作。** `run_recovery` 是共享调度器，`recover_dpi` 失败即 `all_ok = false`，下个周期由 60s 翻倍直至 600s 上限，`retry_missing_timers` / `heal_stale_suspend` / `ensure_power_notifications` 一并被拖慢——与 `AGENTS.md`「不得造成卡死假活」相悖。
4. **最坏情况可复现且不可自愈。** 竖排任务栏上 `calc_widget_rect` 刻意返回 `None` 拒绝嵌入，`recover_dpi` 过不了 `embed_in_taskbar` 这道门，脏位在生产路径上**永无清位机会**；用户改一次 DPI 就把恢复节奏钉死在 600s，直到进程退出。
5. **仓库内已有同构先例**：电源订阅补注册明确「不计入退避」，理由同为「要让重试尽快发生，不能让退避把下个周期推到 10 分钟后」。

## 明确不在本次范围

- **不改 `Drop` 中刻意忽略的 `SelectObject`**：那里是还原 stock 默认对象，失败仅影响 `DeleteDC` 能否释放，属既有合理设计。
- **不实施「把 unsafe 注释要求变成可执行断言」篇**：`Renderer::new` 的 `SAFETY` 注释、`ffi_guard.rs`、`update/http.rs`、`update/crypto.rs` 归那篇，避免两篇改同一行。（唯一例外见「实施记录」：`Renderer::new` 里那句「后续选入…均不会失败」被本次行为变更**证伪**，必须同步改写，否则代码与注释直接矛盾。）
- **不清理全仓其余 98 处 `let _ =`**：本篇只处理 `WM_DPICHANGED` 嵌入失败的**语义**问题（核心动作失败无人记账），不是风格清理。
- **不改 `recover_dpi` 四步顺序**：那套顺序是「先对齐尺寸、再提交完整几何」的既有不变量，本篇只负责让进入它的入口状态一致。
- **不改 `OwnedGdi` 所有权模型**：既有的 owner 与 `into_raw` 纪律足够表达本次修复，不引入新类型。

## 为什么不保留？

最强的反方理由是：`Renderer::new` 已显式声明「后续选入/测量/配置均不会失败」，作者对该段做了明确假设；在 `hdc_mem` 有效且对象刚由 `CreateCompatibleBitmap`、`CreateFontIndirectW` 成功创建的前提下，`SelectObject` 实务上不会失败，判别属于「不可达分支、永不触发的兜底」，按简化审计应删。第二个反方理由是：`WM_DPICHANGED` 那条已有兜底——`embed_in_taskbar` 失败会把 `EMBEDDED` 清成假，于是 `reembed_if_lost` 会在 2 秒内重试重算几何，「新位图 + 旧窗口」两秒后自愈，不必再对齐尺寸。

逐条回应：第一，判别成本是一行 `if`，而它保护的是会让 `self.hbitmap` 与 DC 状态**永久分叉**并泄漏句柄的路径；同一函数内创建侧已判别，交换侧不判别是同一序列内的不一致——`OwnedGdi` 整套 RAII 的存在前提正是「创建成功但提交失败」这一中间态。第二，自愈只保证**几何**被重算，不改变「两条失败出口对同一事实给出不同结论」这一缺陷本身，也不消除自愈前那两秒内 `BitBlt` 源/目标尺寸不一致的可见症状（`renderer.rs` 的 `update_dpi` doc 有记载）。第三，两处改动都不新增状态、不新增依赖，删掉它们不会让任何调用链变短。

结论：本篇不是「为不可达分支加固」，而是让**同一事务的两条失败出口收敛到同一状态**，并让 `SAFETY` 注释的断言由代码支撑。若未来审计要删判别，必须先证伪「`SelectObject` 可能失败」以及「失败会造成句柄泄漏与 DC/结构体状态分叉」。

## 验收标准

- **判别位置**：`grep -n "SelectObject" src/renderer.rs` 的每一处生产调用，要么在 `select_or_fail` 内紧随判别，要么是经注释说明的刻意忽略（`Drop` 还原 stock 两处），要么是已判别的回滚（`swap_dpi_objects` 内一处）。四处交换本身不再直接调用 `SelectObject`。
- **新增用例 `renderer::tests::test_dpi_swap_rejects_failed_select`**：以失效 DC 走 `update_dpi` 公开路径触发选入失败，断言返回 `false`、`bitmap_size()` 未变，并用 `GetCurrentObject(OBJ_BITMAP)` 断言 DC 选中位图仍等于 `self.hbitmap`——直接锁住「DC 状态与结构体字段不永久分叉」这条承重不变量。用例开头以 `ScreenDcGuard::acquire` 显式自证环境前提：`update_dpi` 的创建阶段用的是屏幕 DC（与 `hdc_mem` 无关），否则在拿不到屏幕 DC 的环境里两条断言会照样成立却什么都没测。
- **新增用例 `renderer::tests::test_dpi_swap_rolls_back_bitmap_when_second_select_fails`**：第一个对象给有效位图、第二个给刚 `DeleteObject` 的失效句柄，注入**第二次**交换失败，断言 DC 仍选中原 `old_bitmap`——锁住回滚序。
- **弱测试自查（已实测）**：把 `select_or_fail` 的判别改成恒不失败，两条用例均变红；只删掉 `swap_dpi_objects` 的回滚块，回滚用例变红、另一条仍绿。
- **行为一致性用例仍绿**：`window::tests::dpi_dirty_position_keeps_window_size`、`window::tests::invalidate_last_rect_clears_committed_cache`、`util::tests::test_dpi_scaled_matches_legacy_across_dpi_range`。
- **默认门禁只增不减**：基线 main 为 110 passed / 4 ignored，本分支 112 passed / 4 ignored；四条门禁全绿。
- **冒烟（本机实跑通过）**：`cargo test --locked -- --ignored --test-threads=1` 四条全绿；其中 `smoke::dpi_dirty_cleared_by_recovery_transaction` 增补断言——`WM_DPICHANGED` 后窗口物理尺寸必须等于渲染器位图尺寸（两条出口的共同后置条件）。

**未覆盖，如实记录**：`WM_DPICHANGED` 嵌入失败**分支本身**（对齐尺寸 + 不置脏位）没有自动化用例——它需要竖排任务栏或真实瞬态失败环境，冒烟里真跑的是成功出口。原提案点名的 `test_dpichanged_embed_failure_sets_dpi_dirty` **已不存在**：首版用桌面窗口（受保护窗口，`SetParent` 必然失败）冒充嵌入失败，既违反主窗口过程测试「不创建真实窗口」的契约，又把已被本裁定否决的行为写成断言。该分支目前由代码评审覆盖，并列入 `smoke.rs` 头部的人工清单。

## 风险

- `SelectObject` 的失败注入需要真实 DC 与失效对象，实现用「失效 DC」与「已 `DeleteObject` 的句柄」两种构造，**不新增测试专用生产函数**。
- 置 `DPI_DIRTY` 会让 `position_flags` 在脏位期间追加 `SWP_NOSIZE`，即恢复事务完成前定位路径不再改窗口尺寸。这是既有语义、不新增，但改变了「资源换新失败后那段时间窗口是否尝试改尺寸」的行为，故列入冒烟与人工清单。
- `Renderer::new` 现在会返回「选入位图失败 / 选入字体失败」两种中文错误文案，经 `show_error("初始化渲染器失败: {e}")` 呈现，符合 `AGENTS.md` 的中文文案约定；属启动期行为变更（原先静默继续）。
- 首轮实施曾置脏位并因此发现第 4 条理由的永久钉死风险；该次回退的完整推理见上文裁定小节。

## 实施记录

- `select_or_fail`（`src/renderer.rs`）：`Result<HGDIOBJ, ()>`，`is_invalid` 覆盖 NULL 与 HGDI_ERROR。
- `swap_dpi_objects`（`src/renderer.rs`）：唯一持有「判别 + 回滚序」的实现，同时服务 `Renderer::new` 的首次选入与 `update_dpi` 的换新；回滚失败 `diag!` 留痕。收口前 `Renderer::new` 曾各自内联一份判别与回滚，已合并，回滚逻辑不再有两份。
- `Renderer::new`：注释「后续选入…均不会失败」随行为变更改写成「资源创建已成功，选入仍可能失败，故共用回滚序」，两处交换改调 `swap_dpi_objects`；失败返回 `Err`，走启动期既有错误路径。
- `WM_DPICHANGED`：`dpi_updated == true` 且 `embed_in_taskbar` 失败时 `diag!` 留痕并 `align_window_size_to`（与 `recover_dpi` 第 2 步同原语，不受 `EMBEDDED` 门控；不能复用 `rollback_window_to_bitmap`，它走 `resize_embedded_window` 会被 `EMBEDDED=false` 门控成空转），**不置脏位**；`dpi_updated == false` 保持原有「回滚窗口 + 置脏位」。
