# Agent Note：统一 renderer 数值文本表示——std 格式化替换 write_u32，宽度探针由格式化器派生

Status: proposed

## 问题

两处围绕同一批数值文本的重复表示。其一：`src/renderer.rs:745`-756 的手写 `write_u32` 重复实现 std 的十进制格式化（`u32: Display`），生产调用仅 `src/renderer.rs:367`（`format_percent_wide`）与 `src/renderer.rs:396`（`format_speed_wide`）；检索 `grep -rn "write_u32" src/` 命中定义 + 这 2 处调用 + 专用测试，无其它模块使用。其二：`src/renderer.rs:29` 的 `SPEED_WIDTH_PROBES` 常量把 `format_speed_wide` 在四个边界输入的输出串手工抄成字面量，注释（`src/renderer.rs:27`）自认「改档位或单位必须同步这里」；唯一生产消费点是 `Layout::measure`（`src/renderer.rs:662`-666），检索 `grep -rn "SPEED_WIDTH_PROBES" src/` 命中定义、该消费点与 `src/renderer.rs:621` 文档引用，scripts/CI/installer 零命中。

消费者分类：

- 生产消费者：`write_u32` 两处（上述）；`SPEED_WIDTH_PROBES` 一处（上述）。
- 非生产消费者：`test_write_u32`（`src/renderer.rs:856`-870）直接断言手写实现的十进制输出，是实现专属测试；`speed_column_fits_every_tier`（`src/renderer.rs:965`-996）用真实格式化结果全域复验列宽（步长 65_537 粗扫 + 边界样本），不依赖探针常量本身，其文档（`src/renderer.rs:963`-964）明言「不做常量之间的自证」。

## 提案

两个动作共享 `format_speed_wide` 改动面，可独立实施。动作 A：给 `Renderer` 增加一个复用 `String` scratch 字段，`format_percent_wide`/`format_speed_wide` 加 scratch 参数，用 `std::fmt::Write` 的 `write!(scratch, "{value}")` 产出十进制后 `buf.extend(scratch.encode_utf16())`；删除 `write_u32` 与 `test_write_u32`。`render()` 四个调用点（`src/renderer.rs:442`、`:460`、`:474`、`:492`）传 `&mut self` 的独立字段，借用不冲突；测试各加一个局部 scratch。动作 B：删除 `SPEED_WIDTH_PROBES` 及其文档，把 `format_speed_wide` 内的 `KIB`/`MIB`/`GIB` 常量（`src/renderer.rs:380`-382）提升到模块级，`Layout::measure` 改为把边界输入 `[1023, MIB-1, GIB-1, u32::MAX]` 喂给 `format_speed_wide` 再 `measure_wide` 量宽取最大值——格式化器成为文本形态的唯一真值源。`measure` 是 DPI 重算路径（结果缓存于 `Renderer::layout`，`src/renderer.rs:629`-630），且现有 `measure_str`（`src/renderer.rs:720`-721）本就经 `to_wide` 每次分配，局部 Vec 不违反热路径纪律。合计预估净删约 23 行（A 约 19 + B 约 4），B 的主要收益是删除一条「必须同步」义务。

## 明确不在本次范围

- 渲染热路径零分配纪律（`src/renderer.rs:630`）不破：动作 A 必须走复用 scratch，「每帧 `value.to_string()` 小分配」的更简版本明确否决。
- `draw_text`/`measure_wide` 的截尾 NUL 口径（`src/renderer.rs:688`-692、`:709`-710）与 Layout 的 1px 右缘余量（`src/renderer.rs:647`-651）是近期 DrawText 裁字修复的承重部分，有像素级回归测试背书，不动。
- `format_speed_wide` 的档位判定、四舍五入定点写法与 1023 钳制（`src/renderer.rs:375`-395）不改：本次只换十进制产出方式与探针来源，不换文本形态。
- `LAST_RENDERED_VALUES` 去重与 `DisplayValues::load` 每帧双读（`src/renderer.rs:97` 注释明确禁止更强去重）属受保护设计，不动。

## 为什么不保留？

最强反方：① `write_u32` 零分配、零依赖、12 行自明实现，替换需 scratch plumbing 且签名改动波及约 20 处测试调用点，净收益打折；② 字面探针可读性好、直接文档化视觉上界，且 `speed_column_fits_every_tier` 全域扫描已能抓住漂移，同步义务有测试兜底；③ 边界输入列表仍是半个第二表示——新增小数位时最宽形态可能不在边界上。回应：① 这正是「手写重复 std 能力且替换能删掉实现加其专用测试」判据针对的情形：十进制输出的正确性已由 `test_format_speed_wide_boundaries`（`src/renderer.rs:769`-832，逐字节断言含 0/1023/1024/u32::MAX）与 `test_format_label_and_percent_wide`（`src/renderer.rs:872`-898，含越界钳制）完整覆盖，`test_write_u32` 删后保护不降级；scratch 复用维持摊还零分配，签名波及是机械改动。② 测试兜底是事后抓漂移，派生探针让档位/单位变更在构造上不可能漂移（新增 TB 档由 `u32::MAX` 自动覆盖），全域扫描测试保留为布局合身守卫而非同步守卫。③ 承认：现有字面常量有完全相同的弱点（小数位变更同样要人肉同步），派生版不更差，且同一测试兜底；残余语义（复用缓冲、边界输入选取）计入反对票后净收益仍为正。

## 验收标准

- `grep -rn "write_u32\|SPEED_WIDTH_PROBES" src/` 0 命中。
- `cargo test --locked` 全绿：`test_format_speed_wide_boundaries`、`test_format_label_and_percent_wide`、`speed_column_fits_every_tier`、`layout_has_no_overlap_across_dpi`、`render_keeps_glyphs_inside_bitmap`、`percent_values_are_right_aligned` 原样通过（断言不改）。
- `cargo fmt -- --check`、`cargo build --release --locked`、`cargo clippy --all-targets --locked -- -D warnings` 全过且无 warning。

## 风险

残留风险：scratch 若被误用为每次新建即违反零分配纪律——证伪依据：scratch 是 `Renderer` 字段、随结构体复用，代码审查可直接核对；`Layout::measure` 每次 DPI 重算多 4 次格式化 + 量宽，相对现有 5 次以上 `measure_str` 分配可忽略。边界输入选取的最宽形态假设（小数位场景）与现状同弱，由全域扫描测试证伪兜底。