# Agent Note：关闭 winresource 默认 feature，删除整条 toml/serde/syn3 构建依赖链

Status: implemented

## 问题

`Cargo.toml:48` 的 `winresource = "0.1"` 以默认 feature 引入，而 winresource 0.1.31 的 `default = ["toml"]`（registry 源码 Cargo.toml `[features]`）唯一门控 `parse_cargo_toml`（lib.rs:202-203 调用、:854-855 定义），该函数只读 `[package.metadata.winresource]` 表段——`grep -n "package.metadata" Cargo.toml` 0 命中，本仓库根本没有该表段，此解析今天是纯空转。生产消费面：`build.rs:44`-47 只用 `WindowsResource::new()/set_manifest()/set_icon()/compile()`，四者均不受 toml feature 门控（compile 定义 lib.rs:710 无 cfg；版本资源来自 `CARGO_PKG_VERSION_*` 环境变量，lib.rs:205 起，亦无门控）。依赖证据（本人复跑）：`cargo tree -p winresource --locked` 显示 winresource → toml 1.1.4 → serde_core/serde_spanned/toml_datetime/toml_parser/winnow；`cargo tree -i syn@3.0.3 --locked --target all` 证明第二个 syn 大版本（3.0.3，与 windows 系的 2.x 并存）唯一来源是 serde_derive ← serde_core ← toml ← winresource。

消费者分类：

- 生产消费者：toml feature 的生产消费者为零（无 metadata 表段，parse_cargo_toml 空转）；winresource 本体由 build.rs 真实消费。
- 非生产消费者：无。

## 提案

改为 `winresource = { version = "0.1", default-features = false }`，同提交更新 Cargo.lock（AGENTS.md 门禁：依赖变化 lock 必须同提交）。Cargo.lock 净删 108 行、12 个包（toml/toml_datetime/toml_parser/toml_writer/winnow/serde_core/serde_derive/serde_spanned/syn 3.0.3/equivalent/hashbrown/indexmap；逐块实数：12 个包块共 107 行 + winresource 依赖表删 toml 条目 1 行）；产物 .res 内容不变（manifest、图标、版本资源均不受影响）。按门禁重跑 `cargo build --release --locked` 验证资源预算。

## 明确不在本次范围

- winresource 本体不删：build.rs:44-47 的主产物 manifest + 图标是真实消费，只有关 feature 一条路。
- `version_check`（winresource 的非可选依赖）不动。
- windows crate 的 21 个 feature 不动：本次审计逐一映射到 src 引用，全部有生产消费（含三条注释声称的门控——Registry 门控 ShellExecuteExW、WinSock 门控 GetAdaptersAddresses、Diagnostics_Debug 门控 OutputDebugStringW——已对 registry 源码验证为真）。
- build.rs 的 /DELAYLOAD 链接策略与 TRAFFIC_MONITOR_DEV_BUILD 真值源不动。

## 为什么不保留？

最强反方：① 将来若想加 `[package.metadata.winresource]`（如 LegalCopyright、CompanyName 版本资源字段），关闭 feature 后会被静默忽略，需重新开启；② 依赖面变化按 AGENTS.md 必须重跑 release build 验证资源预算，有验证成本。回应：① 今天没有该表段，feature 收益为零、成本是 12 个包与一个 syn 大版本的编译/审计面；未来真要加版本资源字段时，显式开启 feature 是一行改动且意图清晰——比现在为不存在的消费者养整条链合理；「静默忽略」风险可在开启字段时由构建者当场发现（字段不生效即查 feature）。② 验证成本是门禁既定动作，本提案恰好把 release build 的编译面收窄，验证本身即收益的一部分。

## 验收标准

- `cargo tree -p winresource --locked` 不再出现 toml 子树（仅 version_check）；`cargo tree -i syn@3.0.3 --locked --target all` 报包不存在。
- `grep -n "default-features = false" Cargo.toml` 命中 winresource 行；Cargo.lock 与 Cargo.toml 同提交。
- 产物验证：release exe 的 manifest（PerMonitorV2）、图标、文件版本信息不变（资源查看器或 dumpbin 抽查）。
- `cargo fmt -- --check`、`cargo test --locked`、`cargo build --release --locked`、`cargo clippy --all-targets --locked -- -D warnings` 全过且无 warning。

## 风险

残留风险：winresource 未来版本若把 compile 路径改为依赖 toml，升级时会编译失败——证伪依据：当前 0.1.31 的 compile（lib.rs:710）与版本资源（lib.rs:205 起）均无 cfg 门控，且升级 winresource 本身就要重跑门禁，失败会当场暴露而非静默。产物 .res 内容不变是本提案的可现场验证承诺，资源抽查即证伪手段。