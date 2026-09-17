# Agent Note：删除版本解析器中两处与控制流错位的弱测试
Status: proposed

## 问题

`src/update/version.rs` 内有两个测试的注释声称守护某性质，但按当前 `parse_update_metadata`（`src/update/version.rs:50-70`）的控制流，它们实际走的是另一分支，换成注释所指的错误实现后断言依然通过：其一是 `test_parse_update_metadata_rejects_oversize_blob`（`src/update/version.rs:223-228`），注释称“模拟远端返回超大的合法风格 blob”，输入为 `"0.0.1\n".repeat(10_000)`，但 `parse_update_metadata` 的第一步就是 `lines().count() != 2` 检查（`src/update/version.rs:51-54`），该输入共 10_000 行，直接在行数分支返回 `Err`，parser 根本没有“尺寸”概念（真正的尺寸上限是另一层的 `VERSION_METADATA_MAX_BYTES`，在 `src/update/mod.rs:264,268` 经 `fetch_url` 强制执行，parser 永远见不到超大输入）；其二是 `test_parse_update_metadata_rejects_internal_nul` 的首个断言块（`src/update/version.rs:207-213`），注释称“版本行包含内部 NUL：`parse_version` 走 `split('.')` 后段不可解析”，但输入 `"1.2.3\0\0B94D…FCDE9"` 不含 `\n`，`lines()` 只得 1 行，同样在 `src/update/version.rs:52-54` 就返回 `Err`，根本到不了 `src/update/version.rs:57` 的 `parse_version`，注释与控制流直接矛盾（同一测试的第二个断言 `src/update/version.rs:215-220` 输入含 `\n` 且为两行，能真正到达 `src/update/version.rs:62` 的哈希校验，是强的，不动）。

## 提案

只删测试、不动生产解析：删除 `test_parse_update_metadata_rejects_oversize_blob` 整个测试（`src/update/version.rs:223-228`，含 `#[test]` 共 6 行）；删除 `test_parse_update_metadata_rejects_internal_nul` 的首个断言块（`src/update/version.rs:207-213`，注释加首个 `assert!` 共约 7 行），保留函数名与第二个断言（哈希行 NUL 路径）；净删约 13 行，无生产行为变化。

## 明确不在本次范围

同测试内第二个 NUL 断言（`src/update/version.rs:215-220`，哈希行含 `\0` 且确为两行）是唯一覆盖哈希行 NUL 路径的用例，不得一起删；`test_parse_update_metadata_rejects_wrong_line_count`（`src/update/version.rs:140-154`）、`test_parse_update_metadata_rejects_bad_version_format`（`src/update/version.rs:157-171`）、`test_parse_update_metadata_rejects_bad_hash`（`src/update/version.rs:173-203`）是下文“不减保护”论证所依赖的接管用例，不得动；`fetch_url` 的 `max_response_bytes` 上限（`src/update/mod.rs:264,268`）是另一层的真实尺寸防线，与 parser 行数检查无关，不在此改；若想补“版本号行内含 NUL 且确为两行”（如 `"1.2.3\0\n<64位哈希>"`）或“5e9 级钳位区分”用例，那是测试增强（加法），另案处理，不混入本次删除。

## 为什么不保留？

反方一：`oversize` 测试是大输入冒烟，能防未来实现换成正则回溯或藏 panic。回应：当前实现是 `str::lines + trim + collect` 线性流程，无回溯结构；该测试无耗时、无内存断言，60KB 输入测不出任何性能退化，为假想实现纳税，且注释把“行数拒绝”误述为“尺寸拒绝”，误导后来人以为尺寸路径已覆盖。反方二：`internal_nul` 首断言至少记录了“NUL 输入被拒”的回归意图。回应：意图与实现错位比缺失更贵，后来人会误以为 NUL 版本号路径已覆盖而不敢重构；删后 NUL 哈希路径仍由第二断言守护，版本号非法路径由 `src/update/version.rs:157-171` 六用例守护，单行输入拒绝由 `src/update/version.rs:140-144` 的 0/1 行用例守护，无覆盖丢失。最硬的双重证据：把 `src/update/version.rs:52` 的行数检查删掉（错误实现），`oversize` 测试仍通过（首行版本合法、次行 `"0.0.1"` 长度非 64 照样 `Err`），而 `wrong_line_count` 的 3 行用例（合法版本加合法哈希加 `extra`，`src/update/version.rs:146-151`）会立刻变红——只有后者能区分该检查，满足弱测试“换错仍过、另有更强覆盖”的双重判据。

## 验收标准

`grep -rn "test_parse_update_metadata_rejects_oversize_blob" src` 零命中；`cargo test --locked` 全绿，且点名以下用例必须仍存在并通过：`test_parse_update_metadata_rejects_wrong_line_count`、`test_parse_update_metadata_rejects_bad_version_format`、`test_parse_update_metadata_rejects_bad_hash`、`test_parse_update_metadata_rejects_internal_nul`（仅剩哈希行断言）、`test_parse_update_metadata_valid`；`git diff` 只触及 `src/update/version.rs` 的 `#[cfg(test)]` 模块，生产函数 `parse_update_metadata`、`parse_version`、`is_valid_sha256_hex` 零改动。

## 风险

残留风险是“删测试即减保护”的一般性担忧，此处已证伪：上文逐条列出每个被删断言的接管用例（行数语义由 `wrong_line_count` 接管，版本非法语义由 `bad_version_format` 接管，哈希 NUL 语义由同函数第二断言接管），且变异论证表明被删断言对各自注释声称的性质区分力为零；真实残留风险仅一项：若未来 parser 引入真正的尺寸感知路径（如流式解析），需届时新增针对性测试，而今天 parser 是全量读入后 `lines().count()`，无此路径。
