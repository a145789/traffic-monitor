# Agent Note：让开发版如实说明「不参与升级安装」（收窄到只改文案 + 独立构建标记）

Status: proposed

## 问题

`scripts/package.ts:18-21` 在带 tag 参数打包时把版本改成 `${baseVersion}-${tag}${ts}`（如 `1.6.0-devk3x9zq`），并在 `scripts/package.ts:45-49` 写入 `Cargo.toml` 与 `installer.iss`，`:64-73` 打包后还原——因此只有**该次构建出的 exe** 保留这个版本号，经 `src/config.rs:10` 的 `env!("CARGO_PKG_VERSION")` 进入运行时。

而 `src/update/version.rs:19-35` 的 `parse_version` 只接受严格 `major.minor.patch`：`"0-devk3x9zq".parse::<u32>()` 失败 ⇒ `None`（`src/update/version.rs:118` 的测试正是钉死 `1.2.3-nightly` 被拒）。`compare_versions`（`src/update/version.rs:12-17`）对任一 `None` 返回 `false`，于是落在 `CheckResult::NoUpdate`，手动检查走 `src/update/mod.rs:325-330`，弹出的是「当前已是最新版本 (v1.6.0-devk3x9zq)。」——一句可证伪的假话。全仓 `grep -rn -e prerelease -e "is_dev" -e "-dev" src/` 命中 0 处，没有任何开发版分支。

影响面已被使用场景限定：dev 包只用于本地测试、不发布（项目所有者确认），所以这不影响任何发布物。剩下的真实代价是两条：其一会误导维护者（手动检查在开发期永远返回假结论）；其二 dev 包**无法用于端到端验证更新流程**——想验证更新就必须正经改 `Cargo.toml` 版本号再打包。

**旧稿的三个缺陷**（本稿据此收窄）：其一，把「改文案」和「跳过检查」混着写——提案只改文案，却在「为什么不保留」里声称「『在 dev 包里直接跳过更新检查』正是提案的语义」，自相矛盾；其二，兜住 `NoUpdate` 的多因合流时没写明判的是**本地** `VERSION`（`src/config.rs:10`）而不是远端 metadata，也没写明必须留在 `is_manual` 之内（`src/update/mod.rs:326-328`）——否则自动检查每小时弹一次框；其三，判定口径写成「能被 `parse_version` 拒绝、且形如 `major.minor.patch-<后缀>`」，而 `scripts/package.ts:4` 接受**任意** tag 参数（`dev`、`rc`、`alpha`、`foo` 都会生成 `1.6.0-<tag><ts>`），这个口径会把它们全部当成开发版。

生产消费者：`src/update/mod.rs:327` 是唯一把版本号用于「更新结论」文案的地方；`src/config.rs:10` 的其它消费者是托盘 tooltip（`src/tray.rs:121`，仅展示），与更新判定无关。非生产消费者：`src/update/version.rs:107-119` 的拒绝矩阵测试。

## 提案

**语义二选一，本 note 选「只改诚实文案」。** 开发版仍然照今天一样抓取远端 metadata、照今天一样受自动检查冷却约束，唯一变化是**手动检查**时不再谎称「已是最新」，而是明确说明该构建不参与升级安装。

1. **用独立构建标记，不用版本号字符串猜。** `scripts/package.ts` 的 dev 路径（`scripts/package.ts:45-53` 一带）在调用 `cargo build --release` 时注入 `TRAFFIC_MONITOR_DEV_BUILD=1`（`execSync` 的 `env` 选项合并 `process.env`）；`build.rs` 检测到该环境变量时输出 `cargo:rustc-env=TRAFFIC_MONITOR_DEV_BUILD=1`；`src/config.rs:10` 旁新增 `pub const DEV_BUILD: bool = option_env!("TRAFFIC_MONITOR_DEV_BUILD").is_some();`。这样「是不是开发版」有唯一真值源（构建时的显式标记），与版本号后缀的命名约定解耦，任意 tag 都不再影响判定；常规构建与 CI 不带该环境变量，标记为 false。
2. **文案分支点。** `src/update/mod.rs:325-330` 的 `CheckResult::NoUpdate` 里，`is_manual` 为真时按 `DEV_BUILD` 选择文案：开发版提示「当前为开发版，不参与升级安装」；否则维持现状「当前已是最新版本 (v{VERSION})」。判定读的是本地 `VERSION`（`src/config.rs:10`），与远端 `latest_version` 无关；`NoUpdate` 的其它成因（远端更旧、远端解析失败）不受影响。
3. **不做「静默跳过检查」**：真要跳过，是另一篇 note 的范围——它必须重新定义 `UPDATE_IN_PROGRESS` 的收尾、`NEXT_CHECK_TIME` 冷却该不该推进、以及自动检查被跳过时手动入口的提示，三者都不能靠改一句文案顺带完成。

## 明确不在本次范围

- **不放宽 `parse_version`**：`src/update/version.rs:50-70` 的严格解析同时服务于**远端 metadata**（`version.txt`），放宽它等于允许发布侧写预发布号，会与「文件名由 `AppVersion` 派生」的三方一致校验（`.github/workflows/release.yml:28-40`）打架。
- **不改 `scripts/package.ts` 的版本命名格式**（带时间戳后缀是为了区分同一台机器上的多次 dev 构建）。
- **不改 `src/update/mod.rs:110-144` 的自动检查开关与冷却逻辑**：开发版仍会按小时抓取 metadata，本 note 不省这一次请求（省它需要先决定上面第 3 条那一整套收尾语义）。
- **不为开发版做「假装能装」的旁路**（那会把一个文案问题升级成更新协议问题）。
- **不引入「版本号后缀即开发版」的正则判定**：它是本稿否决的旧口径。若所有者坚持不碰 `build.rs`，退路是精确匹配 `scripts/package.ts:19-20` 实际产出的形态（`^major\.minor\.patch-dev[0-9a-z]{6}$`），而不是任意后缀——但那仍是字符串猜约定，不如标记硬。

## 为什么不保留？

1. 「dev 包不发布，无所谓」——发布的确实无所谓，但这条路径的受害者是开发者自己：手动检查在开发期**永远**返回「已是最新」，而「检查更新」恰恰是改完更新器后最想验证的入口。
2. 「加一个编译期标记要动 `build.rs` 与打包脚本，太重」——`build.rs` 已存在（`build.rs:6` 的 supportedOS GUID 与 `/DELAYLOAD`），`package.ts` 已经在改 `Cargo.toml` 版本号，注入一个环境变量是同一处的 1 行；换来的是判定不再依赖命名约定。
3. 「`is_dev_version` 是纯函数、可单测，更符合 AGENTS.md 第 10 条」——反了：纯函数的输入是版本号字符串，而「开发版」的事实来自构建方式，纯函数只能**猜**；标记法才是唯一真值源。测试价值也不损失：`DEV_BUILD` 可用 `#[cfg(test)]` 覆盖一个 `no_update_message(is_manual, dev_build, version) -> Option<String>` 纯函数来钉死两个文案分支。
4. 「将来引入 RC/alpha 版本号怎么办」——标记法天然不受影响（RC 版本只要不是 dev 打包就不带标记）；旧稿的正则口径会在那时误伤，这正是不采用它的原因。

## 验收标准

- `grep -rn "DEV_BUILD\|TRAFFIC_MONITOR_DEV_BUILD" src/ build.rs scripts/package.ts` 命中 ≥ 3 处（`build.rs` 注入、`src/config.rs` 常量、`scripts/package.ts` 传入）。
- `grep -rn "开发版" src/update/mod.rs` 命中 1 处文案；`src/update/mod.rs:325-330` 的判定留在 `is_manual` 分支内（人工核对：自动检查路径不产生任何 `show_info`）。
- `grep -rn "is_dev_version" src/` 命中 0 处（旧口径的正则判定不得出现）。
- 新增测试钉死 `no_update_message` 的四种组合：`(manual=true, dev=true)` ⇒ 开发版提示；`(manual=true, dev=false)` ⇒ 「已是最新」；`(manual=false, *)` ⇒ `None`（不提示）。
- 既有拒绝矩阵测试（`src/update/version.rs:107-119`）保持不变并通过，证明没有放松远端 metadata 的严格解析。
- 手动：`bun scripts/package.ts dev` 构建出的 exe，托盘「检查更新」显示开发版提示而非「已是最新版本」；并且**同一份源码不带标记的 release 构建**仍然显示「已是最新版本」（用同一天的两次构建对照，证明标记而不是版本号在起作用）。
- `cargo test --locked`、`cargo build --release --locked`、`cargo clippy --all-targets --locked -- -D warnings`、`cargo fmt -- --check` 全绿。

## 风险

- 标记法只覆盖 `package.ts dev` 这条路径：手工把 `Cargo.toml` 版本改成 `x.y.z-foo` 再 `cargo build` 的开发者不会被标记，仍然看到「已是最新」——这是已知边界，脚本路径才是唯一受支持的 dev 打包入口。
- 环境变量只在该次 `cargo build` 生效；若 `package.ts` 后续拆分或换成别的构建方式（例如新增 CI 任务跑 dev 包），标记会静默丢失。缓解：`scripts/package.ts` 里给该行加注释说明「该变量是开发版判定的唯一来源」，防止被顺手删掉。
- 本地验收需要跑一次 `bun scripts/package.ts dev`（依赖 Inno Setup 与 `ISCC.exe`），CI 上无法覆盖；这也是本 note 唯一无法自动化的验收项。
