# Agent Note：删除版本号后缀分支，只接受严格 x.y.z

Status: implemented

## 问题

[`parse_version`](../../../src/update/version.rs) 在解析 `major.minor.patch` 之外，额外接受 `-` 后缀分支（字母数字与 `-` 即合法），但该分支的产出语义已经断裂：后缀被解析后直接丢弃，[`Version`](../../../src/update/version.rs) 只保留三段数字，派生 `Ord` 比较时 `1.2.3-nightly` 与 `1.2.3` 完全相等，而 [`compare_versions`](../../../src/update/version.rs) 又会把 `0.4.3-nightly` 判定为比 `0.4.2` 更新。生产侧没有任何后缀生产者：[`release.ts`](../../../scripts/release.ts) 用 `/^\d+\.\d+\.\d+$/` 强校验发布版本号，[`Cargo.toml`](../../../Cargo.toml) 当前为 `1.3.1` 纯数字，远端 `version.txt` 由同一发布流水线生成，同样不可能带后缀。唯一的消费者是测试：`test_compare_versions_with_suffix`、`test_parse_version_valid` 中的后缀用例、`test_parse_update_metadata_rejects_bad_version_format` 中的 `1.2.3-` 非法后缀用例，本质是用测试钉死一个无生产者的推测性通用能力。

## 提案

删除 `parse_version` 中的 `split_once('-')` 后缀分支与相关校验，使任何含 `-` 的版本行直接解析失败，走 [`parse_update_metadata`](../../../src/update/version.rs) 已有的“版本号格式不正确”错误路径；同步删除 `test_compare_versions_with_suffix`、后缀有效用例与 `1.2.3-` 用例（保留空段、非数字、四段等真正 load-bearing 的拒绝用例），同步更新 `version.rs:53` 模块文档中“（可选已知后缀）”的残留表述（该注释与 68 行“必须为 major.minor.patch”错误文案自相矛盾），并在更新检查失败提示中保持现有中文错误文案不变，不新增任何配置项或错误码。

## 为什么不保留？

最强的反方理由是“未来若要发 nightly / rc 预览版，后缀解析可以复用”。但当前比较器根本没有实现预发布排序（后缀被丢弃，`nightly` 会被当成正式版比较），留着它不会加速未来，只会让未来实现者误以为后缀已受支持；真需要预发布时，按 semver 优先级重写比较器并补端到端测试，成本远低于长期维护一段语义错误的分支。删掉后非法后缀走统一的元数据错误提示，行为更易解释。

## 验收标准

`rg -n "split_once\('-'\)|suffix" src/update/version.rs` 无命中；`cargo test version` 全绿且仅保留纯数字与拒绝类用例；手动构造 `1.2.3-nightly\n<64位哈希>` 的元数据输入时返回版本号格式错误而非“有更新”；`cargo clippy -- -D warnings` 无警告。

## 风险

远端若曾手写带后缀的 `version.txt`，删除后会被判为“版本文件错误”而不提示更新，属于可见行为变化。缓解：该文件由本仓库流水线独家生成，历史 release 均为纯数字；即使误伤，用户也只是收不到一次预发布提示，下一次纯数字正式版仍可正常比较，不会锁死更新通道。
