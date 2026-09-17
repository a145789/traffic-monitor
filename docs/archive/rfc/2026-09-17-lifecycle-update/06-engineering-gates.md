# Agent Note：工程门禁补齐发布一致性与锁文件恢复

Status: proposed

## 问题

两处工程缺口都会在“最不该错的时候”错。第一处是 `scripts/package.ts:43` 的 dev 打包临时改 `Cargo.toml` 版本后执行构建，`Cargo` 会同步改写根包在 `Cargo.lock` 中的版本，但 `scripts/package.ts:62` 的 `finally` 只恢复 `Cargo.toml` 与 `installer.iss`，导致每次 dev 打包后工作区留下锁文件版本脏改，下一次提交极易把时间戳 dev 版本带进正式发布。第二处是发布链无一致性校验：`scripts/release.ts:74` 的 clippy/test/build 三条命令缺 `--all-targets --locked`（与 `AGENTS.md` 门禁及 CI 的 `cargo test --locked`、`cargo build --release --locked`、`cargo clippy --all-targets --locked` 不对齐，本地绿 CI 红），且结尾 `git push && git push --tags` 会推出全部本地标签而非本次目标标签；`.github/workflows/release.yml:21` 只从 tag 取版本，从不校验 tag、`Cargo.toml`、`installer.iss` 三方一致，出现“元数据宣称新版本、装进去仍是旧程序”时全链无拦截。检索证据：`Select-String -Pattern "Cargo.lock|Cargo.toml|installer.iss" scripts/package.ts scripts/release.ts` 确认 package.ts 无 `Cargo.lock` 引用、release.ts 的 `git add` 含锁文件但 clippy/test/build 行无 `--locked`；`Select-String -Pattern "push --tags|VERSION" scripts/release.ts .github/workflows/release.yml` 确认全量推标签与单向取版本各一处。

## 提案

把三处收敛为“同一提交、同一版本、同一门禁”，生产消费者为发布安装包与 `version.txt` 的版本真实性（更新闭环的信任根）；非生产消费者无（脚本与 CI 无单测，验收靠命令与工作区状态）。具体改动：`package.ts` 在改版本前同保存 `Cargo.lock` 原文，`finally` 中与 `Cargo.toml`、`installer.iss` 一起恢复，并覆盖构建成功、编译失败、打包失败三条路径（已有 `try/finally`，只加保存与恢复两行，不要加 `--locked`，因临时版本与原锁天然不一致）；`release.ts` 在改版本前强制三方一致校验（tag 目标、`Cargo.toml`、`installer.iss` 任一不一致即退出），三条 cargo 命令对齐为 `cargo test --locked`、`cargo build --release --locked`、`cargo clippy --all-targets --locked -- -D warnings` 并追加 `cargo fmt -- --check`，结尾改为只推本次目标提交与目标标签（如 `git push origin main vX.Y.Z` 语义，禁止裸 `push --tags`）；四道门禁必须排在版本写回之前执行（改写 `Cargo.toml` 版本后锁文件即过期，任何 `--locked` 命令必败，门禁只能验证改前基线），改版本之后只跑 `cargo update --workspace` 加一次复核构建；`release.yml` 在构建前加一步三方一致校验（tag 去 `v` 前缀等于包版本且等于 `AppVersion`），失败即红。

## 明确不在本次范围

版本号方案（semver、dev 时间戳后缀格式）一字不动；`cargo update --workspace` 是否保留不在本次范围（本篇只要求锁文件随本次目标版本一起提交，不评价依赖升级策略）；安装器签名、来源认证不在本次范围（另见 `02` 的范围声明）；不改任何 `src/` 行为，`unsafe` 与注释不动。

## 为什么不保留？

最强的反方是“dev 锁脏改肉眼可见，发布前人工对一下版本即可，门禁越严越碍事”。逐条回应：锁脏改恰恰肉眼难见（`Cargo.lock` diff 只有一行版本号，易被当成正常升级顺手提交），而一旦带时间戳版本进正式 tag，更新链的 `version.txt` 哈希与用户已装程序版本即永久错位，事后无法低成本修复；人工对版本在深夜发布时必然失效，而三方校验是十行脚本，imbus“一劳永逸”。第二个反方是“给 dev 构建加 `--locked` 更简单”。不采纳的理由：临时版本与原锁按定义就不一致，加 `--locked` 会让所有 dev 打包直接失败，这正是审计原文指出的坑。

## 验收标准

`Select-String -Pattern "Cargo.lock" scripts/package.ts` 必须命中保存与恢复两处；`Select-String -Pattern "--locked|--all-targets|push --tags|fmt --" scripts/release.ts` 必须同时命中对齐后的门禁且无裸 `push --tags`；`Select-String -Pattern "AppVersion|CARGO_PKG_VERSION|GITHUB_REF_NAME" .github/workflows/release.yml` 必须命中新增的一致性校验步。命令验收：dev 打包成功、编译失败、打包失败三条路径后 `git status --porcelain` 均干净；`release.ts` 在三方不一致时非零退出且不产生提交与标签。现有门禁保持全过：`cargo test --locked`，`cargo build --release --locked`，`cargo clippy --all-targets --locked -- -D warnings`，`cargo fmt -- --check`。

## 风险

残留风险是 `release.yml` 新增校验步在首个发布或标签格式漂移（如 `v` 前缀缺失）时误红。缓解是校验逻辑与现有 `VERSION=${GITHUB_REF_NAME#v}` 同源，只做字符串相等比较，不做语义化解析；若因大小写哈希（`version.txt` 的大写 SHA）或换行符导致校验误报，应收紧比较对象到三方版本号本身，不得扩大到哈希比对。证伪依据：任一 dev 打包后工作区非干净，或任一 tag 与包版本不一致仍能走到编译安装器，即判定失败。
