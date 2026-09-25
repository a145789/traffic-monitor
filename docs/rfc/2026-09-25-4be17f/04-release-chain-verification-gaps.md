# Agent Note：补齐发版链路的验证缺口，让门禁验的就是被发布的那棵树

Status: proposed

## 问题

第一处，发布工作流不跑测试。`.github/workflows/release.yml` 全文唯一一条 cargo 命令是第 `55` 行的 `cargo build --release --locked`，**没有 `cargo test`**：打 tag 出包这条路径不跑测试，只靠「合入 main 时测过」的假设。而 `scripts/release.ts:50-58` 只校验 semver、分支、工作区干净与 tag 不存在，手工打 tag 同样能触发该工作流。

第二处，门禁与产物不是同一份依赖解析结果。`scripts/release.ts:72-88` 的四条门禁跑在**版本改写之前**（脚本自己的注释解释了原因：改写 `Cargo.toml` 版本后锁文件即过期，`--locked` 必败），随后第 `104` 行执行 `cargo update --workspace` 改写 `Cargo.lock`，`108-113` 提交并打 tag，`120` 推送。**改锁之后没有任何一次重验**：门禁验的是改锁前的依赖图，被发布的 commit 用的是改锁后的。

第三处，不校验本地 main 与远端一致。`scripts/release.ts:19-34` 只校验当前分支为 `main` 与工作区干净，第 `120` 行的 `git push origin main v<version>` 会把本地未推送的提交一并推去发版。

检索证据：`Select-String -Path .github/workflows/release.yml -Pattern 'cargo '` 命中 1 条（`build`）；`Select-String -Path scripts/release.ts -Pattern 'cargo '` 命中 `76`、`80`、`88`、`104` 四条，其中 `104` 之后无任何门禁命令；`Select-String -Path scripts/release.ts -Pattern 'origin/'` 命中 0 条。

与 `AGENTS.md` 的冲突：发布门禁要求「依赖解析与 release 验证必须使用仓库已提交的同一解析结果：三条 Cargo 门禁均带 `--locked`，不得让本地命令顺手改写 `Cargo.lock`」，并要求「发版脚本只能在干净的 `main` 上运行」。

## 提案

`.github/workflows/release.yml` 在第 `55` 行的 `cargo build --release --locked` 之前加一步 `cargo test --locked`。

`scripts/release.ts` 在第 `104` 行的 `cargo update --workspace` 之后、提交之前重跑一次 `cargo build --release --locked`（若只想加一条：该命令同时验证锁定依赖图可构建，测试已由前置门禁覆盖；是否同时补 `cargo test --locked` 由实施时按 CI 预算决定并写入提交信息）。

`scripts/release.ts` 在第 `19` 行的分支校验旁增加远端一致性校验：`git fetch` 后要求 `git rev-list --count origin/main..main` 为 0，失败即退出。

## 明确不在本次范围

**不加代码签名**：用户已按证书费用与维护成本明确否决；本篇只补「验证与发布脱节」，不改信任模型。

**不改 `.github/workflows/release.yml:16-17` 的 `permissions: contents: write`，也不把 action 钉到 commit SHA**：这与签名同属 CI 供应链加固，与「验证缺口」正交，留作独立候选，避免一篇笔记同时改验证语义与权限模型。

**不改 `.github/workflows/check.yml`**：四条门禁已齐全（`check.yml:25-35`），且与本地门禁同构，属既有正确面。

**不引入新的版本真值源**：`Cargo.toml`、`installer.iss`、tag 三方一致性由既有校验承担（`release.yml:36-46`）。

**不动 `scripts/release.ts:90-100` 的版本改写顺序**：改写必须在门禁之后（`--locked` 的前提），本篇只在改写之后补一次重验，不调整既有顺序。

## 为什么不保留？

最强的反方理由是：`check.yml` 已经在 push 到 main 时跑过同一棵树，`release.yml` 再跑一遍是重复劳动；而 `cargo update --workspace` 在本仓库当前是空操作（`Cargo.toml` 只有 `windows`、`windows-registry` 与构建期 `winresource`），「改锁后重验」是给不会发生的事加保险，属推测性防护。

逐条回应：第一，两次验证的**对象不同**——`check.yml` 测的是 push 时的 main，`release.yml` 测的是被发布的 commit，两者之间隔着一次版本改写与一次锁改写，而 `AGENTS.md` 明确要求发布验证与提交的解析结果一致。第二，「当前是空操作」不是判据：`cargo update --workspace` 的结果依赖当时的上游版本，一旦某依赖发新版就会静默改变被发布的依赖图，而这条路径的失败模式是「门禁全绿、发布的却是没验过的图」，是最贵的一类失败。第三，本项净增 CI 一行、脚本一行、脚本一段校验，不新增符号、不新增状态、不引入依赖，删掉它不会简化任何调用链。

结论：本篇修的是「门禁与产物不是同一棵树」这一事实，不是给低概率事件加保险。若未来审计要删 `.github/workflows/release.yml` 里的 `cargo test`，必须先证伪「tag 指向的 commit 与 check.yml 验过的 commit 必然相同」——`scripts/release.ts:104` 的锁改写与「允许手工打 tag」这两条一起证伪了它。

## 验收标准

`Select-String -Path .github/workflows/release.yml -Pattern 'cargo test'` 至少 1 条命中。

`Select-String -Path scripts/release.ts -Pattern 'origin/main'` 至少 1 条命中，且该检查位于版本改写（`scripts/release.ts:90`）之前。

`Select-String -Path scripts/release.ts -Pattern 'cargo (test|build)'` 在 `cargo update`（`scripts/release.ts:104`）之后至少 1 条命中。

本地可执行的等价验证：在干净 main 上依次执行 `cargo update --workspace` 与 `cargo build --release --locked`，两者都必须成功且不修改 `Cargo.lock` 之外的任何文件。

弱测试自查：把远端一致性校验改成恒真，人为制造「本地多一个未推送提交」的现场，脚本必须拒绝发版。

四条门禁全绿：`cargo fmt -- --check`、`cargo test --locked`、`cargo build --release --locked`、`cargo clippy --all-targets --locked -- -D warnings`（本篇不改 `src/`，但仍按 `AGENTS.md` 走完）。

## 风险

`cargo test --locked` 在 `release.yml` 的 windows-latest 上会给 tag 到产物之间增加一次完整测试构建；本机增量耗时约 1.15 秒（`108 passed; 4 ignored`），CI 冷启动不可比。若预算不可接受，只能在「缩小测试集合」与「保留全量」之间取舍——本仓库是 bin-only 且测试内联在 `src/`，不存在只跑部分测试目标的省事选项，实施时需明确选择并记录。

`git fetch` 会给发版脚本引入网络依赖：离线环境下脚本会失败。这是有意的（发版本本就需要推送），但会让「离线演练发版」不可用。若要保留离线能力，需把该校验降级为警告而非失败，并在此处如实登记该退化。
