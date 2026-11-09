# Agent Note：windows-registry 对齐 windows 0.62 依赖家族，消除 link/result/strings 双版本

Status: implemented

## 问题

`Cargo.toml:44` 的 `windows-registry = "0.100.0"` 引入 windows-link 0.100.0 / windows-result 0.100.0 / windows-strings 0.100.0，与 windows 0.62.2 已有的 0.2.1 / 0.4.1 / 0.5.1 三胞胎并存——`Cargo.lock:123`-186 实见七个条目（link/result/strings 各两个版本，registry 自身一条）。生产消费面极窄：`src/util.rs:19` 的 `CURRENT_USER` 与 `src/util.rs:566`-617 的 6 个 reg_* 包装，用到的 API 仅 open/create/get_u32/set_u32/get_string/set_string/set_bytes(Type::String)/remove_value，错误处理只做 `.ok()/.is_ok()`，不依赖错误类型的版本（下游 tray.rs、update/mod.rs、renderer.rs 经包装消费，不直接触碰 windows-registry 类型）。版本对齐证据（本人从 crates.io index 缓存解析 0.6.1 记录）：windows-registry 0.6.1 非 yanked、rust_version 1.82（< 项目 MSRV 1.95）、依赖恰为 windows-link ^0.2.1 / windows-result ^0.4.1 / windows-strings ^0.5.1——与现有 windows 0.62 树完全重合，降级后三个 0.100 包从 lock 消失。

消费者分类：

- 生产消费者：util.rs 6 个 reg_* 包装（真实调用方遍布 tray/update/renderer/util 自身），API 面在 0.6.1 全部存在且签名相同（docs.rs 0.6.1 逐一核对，含 `set_bytes(&self, name, ty: Type, value: &[u8])`）。
- 非生产消费者：无测试直接消费 windows-registry 类型。

## 提案

`windows-registry = "0.6.1"`，同提交改 Cargo.lock；三条 --locked 门禁 + release build 重跑。Cargo.lock 净删 21 行（17 增 / 38 删）、3 个重复包整块消失（windows-link/result/strings 的 0.100 三块各 6/9/9 行，逐块实数；余下删行是依赖列表里 `windows-link 0.2.1` 这类带版本后缀的消歧写法改回不带版本），windows-registry 自身块行数不变，编译/审计面去掉一整套平行 windows 基础 crate。在笔记或 Cargo.toml 就近留一句版本对齐约定：未来 windows 升级到 0.100 家族时，windows-registry 同批升回。

## 明确不在本次范围

- windows-registry 整包不删：6 个包装有真实生产调用，手写 RegGetValueW 替代会净增 unsafe 面。
- 项目 MSRV 声明（Cargo.toml rust-version = 1.95）不动：降级后声明更保守无害，CI 对账门禁不违反；不为降级而改 MSRV。
- util.rs 包装的签名与语义不动（含 reg_write_string_os 的 OsStr 无损路径）。
- windows crate 本体与其 21 个 feature 不动。

## 为什么不保留？

最强反方：① 这是向旧发布线降级，放弃 0.100 线的后续修复；② 0.100.0 的 MSRV 恰为 1.95（与项目 rust-version 相同，疑似当初为它抬过 MSRV），降级动机需审视；③ 未来 windows 升级新家族时 registry 要再升回，反复改动。回应：① 用到的 API 是 0.1 起就稳定的基础方法，0.6.1 与 0.100 的行为差异由编译 + 现有测试 + 注册表冒烟覆盖；消除双版本的收益（少编译三个 crate、审计面不再有两套 link/result/strings）是确定的，而「后续修复」对本仓库用到的稳定 API 面没有已知需求。② MSRV 声明本身不动，对账门禁照常通过；若确曾为 registry 抬 MSRV，降级后该理由消失，但降 MSRV 是另一个独立决策，不在本次。③ 版本对齐是跟随 windows 家族的自然动作，同批升级即可；现在保留双版本才是持续的编译/审计成本。

## 验收标准

- `cargo tree --locked` 中 windows-link/windows-result/windows-strings 各仅一个条目（0.2.1/0.4.1/0.5.1）；`grep -n "0.100.0" Cargo.lock` 对这三个 crate 0 命中。
- `cargo fmt -- --check`、`cargo test --locked`、`cargo build --release --locked`、`cargo clippy --all-targets --locked -- -D warnings` 全过且无 warning。
- 人工冒烟：开机自启勾选/取消（reg_write_string_os/reg_remove_value）、自动更新开关（reg_write_dword/reg_read_dword）、主题跟随（reg_read_dword 读 SystemUsesLightTheme）、调试日志开关（reg_read_dword）均正常。

## 风险

残留风险：0.6.1 与 0.100 在未单测覆盖的注册表行为上存在差异（如错误码映射）——证伪依据：包装只做 `.ok()/.is_ok()`，错误类型差异被丢弃；编译 + 门禁 + 上述冒烟覆盖全部 6 个包装的实际路径；真发现差异可回退 0.100（lock 变更可逆，无协议/资产身份影响）。