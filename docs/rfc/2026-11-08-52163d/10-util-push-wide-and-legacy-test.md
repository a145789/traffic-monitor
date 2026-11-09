# Agent Note：util.rs 两处——内联 push_wide 并删除 dpi_scaled 的 legacy 等价钉死

Status: proposed

## 问题

两处独立冗余，合计净删约 25 行。其一（无外部消费者的 pub 表面积）：`src/util.rs:39`-42 的 `pub fn push_wide` 生产调用仅同文件 `to_wide`（`src/util.rs:35`）一处；检索 `grep -rn "push_wide" src/` 命中 4 处全在 util.rs（35 调用、39 定义、702/704 测试），外部文件 0 命中——唯一调用方就是它自己的包装者，属过早通用化。其二（钉死已完成迁移的第二表示）：`src/util.rs:760`-771 的 `test_dpi_scaled_matches_legacy_across_dpi_range` 与 `test_dpi_scaled_matches_forward_formula` 内的 legacy 断言（`src/util.rs:755`-756）把现行公式与 legacy 公式 `base * (dpi / 96.0)` 逐点等价钉死，但 legacy 公式在生产代码中不存在——`dpi_scaled` 唯一实现是 `src/util.rs:77`-78 的 `(base * dpi) / 96.0`；检索 `grep -rn "legacy" src/` 除这两处测试外仅命中 power.rs 的 legacy 显示器订阅（无关且受保护）。range 测试注释（`src/util.rs:762`-763）自认「等价是实测结论而非恒等式」，钉的是已完成的公式迁移而非现行性质。

消费者分类：

- 生产消费者：push_wide 仅 to_wide；legacy 公式无生产消费者（不存在于生产代码）。
- 非生产消费者：`test_push_wide_appends`（`src/util.rs:701`-706）；两个 dpi 测试自身。to_wide 的行为另有 `test_to_wide_roundtrip`（`src/util.rs:692`-699）、`test_os_to_wide_matches_to_wide_on_ascii`（`:722`-726）、`test_os_to_wide_preserves_lone_surrogate`（`:728`-737）覆盖；dpi_scaled 的舍入方向由 forward 测试的 11 组具体期望值（`src/util.rs:741`-753，含 19.5→20、-4.5→-5 边界）钉死，trunc/floor 类错误实现仍会红。

## 提案

动作 A：把 push_wide 的两行体（`extend(s.encode_utf16())` + `push(0)`）直接写进 to_wide，删 push_wide 与 test_push_wide_appends（约 11 行）。动作 B：删整个 range 测试与 forward 测试里的 2 行 legacy 断言（约 14 行），保留 11 组具体期望值。两个动作可独立实施。

## 明确不在本次范围

- `os_to_wide`/`copy_wide_truncated` 不动：各有真实生产调用方（路径无损转换、托盘 szTip 与字体 lfFaceName 截断拷贝）。
- `dpi_scaled` 的实现与「全项目唯一舍入实现」地位（`src/util.rs:67`-72 文档）不动：窗口与位图共享同一舍入由两处调用同一函数在构造上保证，本提案不碰该不变量。
- `Layout::new` 的宽度反推比例（`src/util.rs:74`-76 文档，刻意不经整数 DPI 中转）不动。
- power.rs 的 legacy 显示器订阅与本「legacy」无关，是受保护的双通道设计，不动。
- to_wide 的 NUL 语义与「config 含尾 NUL 常量勿套本函数」约定（`src/util.rs:31`-32）不动。

## 为什么不保留？

最强反方：① push_wide 净删小，「追加宽串并补 NUL」将来出现第二个调用方需重新提取；② range 测试是唯一扫满 96–384 DPI 的覆盖，删后公式若在稀疏点之外漂移不会被发现。回应：① 当前唯一调用方是它自己的包装者，pub 表面积没有外部消费者；将来真有第二个调用方时重新提取是两行事，现在保留则是为不存在的场景养表面积加一个只测自身实现的专用测试——to_wide 的既有测试已完整覆盖 NUL 与内容行为，删 test_push_wide_appends 不减保护。② 产品不变量是「窗口与位图共享同一舍入」，由调用同一函数构造性保证，与公式具体形状无关；legacy 公式既已从生产消失，「与 legacy 等价」就不再是现行性质而是迁移遗物——漂移的前提是有第二份表示，删除后唯一实现无从漂移；舍入方向错误（trunc/floor）仍被 11 组具体期望值中的边界格抓住。

## 验收标准

- `grep -rn "push_wide" src/` 0 命中；`grep -n "legacy" src/util.rs` 0 命中。
- `cargo test --locked` 全绿：`test_to_wide_roundtrip`、`test_os_to_wide_matches_to_wide_on_ascii`、`test_os_to_wide_preserves_lone_surrogate`、`test_dpi_scaled_matches_forward_formula`（去掉 legacy 断言后）原样通过；`layout_has_no_overlap_across_dpi`（renderer）与 `test_calc_widget_rect` 类窗口测试不受影响。
- `cargo fmt -- --check`、`cargo build --release --locked`、`cargo clippy --all-targets --locked -- -D warnings` 全过且无 warning。

## 风险

残留风险：B 删除全范围扫描后，若未来有人引入第二套舍入实现，逐点等价不再有自动化对账——缓解：当前不存在第二套实现，且 `dpi_scaled` 文档明示唯一舍入实现地位；真引入第二套时属于新设计，应配新的对账测试而非依赖迁移遗物。A 的 to_wide NUL 语义逐字节不变，既有测试证伪兜底。