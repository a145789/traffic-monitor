# Agent Note：用字符串比较替换 is_immersive_color_set 的手写 UTF-16 逐码元循环

Status: proposed

## 问题
`src/suspend.rs:304` 的 `is_immersive_color_set` 用约 20 行手写代码做一次「宽字符串是否等于 `ImmersiveColorSet`」的判定：`src/suspend.rs:311` 把期望串 collect 成 `Vec<u16>`，`src/suspend.rs:312-321` 逐码元比较并在 NUL 处提前返回，末尾还有一行仅为满足类型系统而存在的兜底 `true`（`src/suspend.rs:325`，注释 `src/suspend.rs:322-324` 专门解释它为何删不掉——E0308）。加上 5 个专用测试（`src/suspend.rs:337`、`:344`、`:352`、`:360`、`:369`）共约 40 行，钉死 null→false、含尾 NUL 精确匹配、前缀/更长即否——合计约 65 行表面积维护一个字符串相等判定。检索记录：内置 grep `is_immersive_color_set`（`src/`）——定义 1（`src/suspend.rs:304`）、生产调用 2（`src/main.rs:616` 与 `src/main.rs:719`，即两个窗口过程的 `WM_SETTINGCHANGE` 臂）、测试调用 5；`windows::core::PCWSTR` 已是本 crate 既有依赖用法（`src/window.rs`、`src/update/*` 多处 `PCWSTR(...)` 构造），替换不引入新依赖。

## 提案
函数体改为「`PCWSTR` 字符串化后与字面量做相等比较」：保留开头的 null 早退（`src/suspend.rs:305-308`，FFI 输入防御），把 `src/suspend.rs:309-325` 的期望串构造与手写循环整段替换为 `PCWSTR(ptr).to_string()` 与 `"ImmersiveColorSet"` 的相等判定。落地口径（已核对 windows-strings-0.5.1 registry 源码）：`windows::core::PCWSTR` 即 `windows_strings::PCWSTR`（windows-core-0.62.2 仅 `pub use windows_strings::*`），可用入口是 `pub unsafe fn to_string(&self) -> Result<String, FromUtf16Error>`（pcwstr.rs:64）与 `pub unsafe fn as_wide(&self) -> &[u16]`（pcwstr.rs:55），**没有** `to_string_lossy`（那是 `HSTRING` 的方法，hstring.rs:23），不得按该名字落地；`to_string` 是 unsafe fn，且 `Cargo.toml:51` 有 `rust.unsafe_op_in_unsafe_fn = "deny"`，函数体内须显式 `unsafe {}` 块加 SAFETY 注（函数本身已是 unsafe fn，调用方契约不变）；按判据 8（手写实现可被既有 crate 能力替换，且替换能删掉实现加其专用测试）同时删除 5 个专用测试所在的整节（`src/suspend.rs:334-376`）。净删约 50-58 行。保守变体（二选一）：保留 `test_immersive_color_valid_string`（`src/suspend.rs:344`）与 `test_immersive_color_longer_name_is_rejected`（`src/suspend.rs:369`）作为「精确匹配、更长即否」的可执行钉子，删其余 3 个，净删约 40 行。

## 明确不在本次范围
两处 SAFETY 契约措辞不统一的问题不随本提案处理：`src/suspend.rs:301-303` 写「调用者必须保证 `lparam` 指向有效的、以 NUL 结尾的 UTF-16 宽字符序列」，而两个调用点的注释写「OS 保证 lparam 指向 NUL 结尾宽字符串（或 null）」（`src/main.rs` 两处 `WM_SETTINGCHANGE` 臂）——这是注释准确性问题、不影响行为，混进来会让本提案的 diff 从「删实现」变成「改契约」；null 早退（`src/suspend.rs:305-308`）保留不删：它是 FFI 边界的输入防御（`lparam` 确可为 null），与「删手写循环」性质不同，不能一起删；`apply_theme_change` / `apply_theme_change_to_main`（`src/main.rs`）与两处调用点不动，签名不变。

## 为什么不保留？
最强反方：删 5 个测试是真实的保护净损失——`src/suspend.rs:371` 的注释明确点名了一个曾被考虑过的错误实现（「若改用无尾 NUL 的 17 元素切片比较，此例会被误判为主题变更」），`test_immersive_color_longer_name_is_rejected` 正是防未来回退到定长切片比较的钉子。回应：(1) 该残余真实存在，故提案给出保守变体（保留 valid + longer 两条）供裁定，主案删全 5 条的前提是「字符串相等由语言语义承担，写不出前缀匹配/定长匹配的错误实现」；(2) 但「未来有人换回手写定长比较」无法被任何实现彻底排除，保守变体以约 10 行代价封住它——若评审认为保护优先，直接采纳保守变体，两者净删差距约 15 行。次强反方：这段代码刚做过一轮刻意简化（`src/suspend.rs:309` 注释自述「去掉整套 const 展开器与误传非 ASCII 即错的隐式契约」），手写运行时循环是那轮的落点，再动等于翻案。回应：那轮的目标是「不做编译期 UTF-16 展开、消除隐式 ASCII 契约」，字符串相等比较同样满足且更彻底——它连运行时比较循环都不需要，与那轮取舍同向而非反向。第三反方：低频路径（`src/suspend.rs:309` 自述 WM_SETTINGCHANGE 一天至多数次）没有性能收益。回应：成立，本提案动机是删表面积、消除那行 E0308 兜底，不主张性能。

## 验收标准
`grep -n "expected_char\|expected: Vec<u16>" src/suspend.rs` 0 命中；主案下 `grep -rn "fn test_immersive" src/` 0 命中，保守变体下恰剩 2 个用例；`cargo test --locked` 全绿，本文件其余用例逐个点名仍绿：`test_baseline_rebuild_only_on_last_resume_edge`、`test_timer_plan_suspended_has_no_timers`、`test_timer_plan_fullscreen_only_keeps_detection_timer`、`test_timer_plan_normal_backoff_uses_slow_network_interval`、`test_timer_plan_normal_online_uses_regular_network_interval`、`auto_update_poll_interval_must_be_far_below_cooldown`；`cargo build --release --locked` 与 `cargo clippy --all-targets --locked -- -D warnings` 无任何警告。

## 风险
风险一（已在反方承认）：主案失去防回退钉子——若实施主案，请在 `src/suspend.rs` 的函数文档里写明「必须是精确相等比较，禁止定长/前缀匹配」，把钉子从测试挪进文档。风险二：`PCWSTR::to_string` 对非法 UTF-16 返回 `Err(FromUtf16Error)`、比较判否，与原逐码元早退路径殊途同归（非法序列不可能等于合法字面量，两种实现都判否）；可证伪依据是保留或新增的 `wrong_string` 类用例。风险三：API 口径已按 windows-strings-0.5.1 registry 源码核对（见提案），但本次审计未编译验证调用形态，实施时以编译器为最终裁决。
