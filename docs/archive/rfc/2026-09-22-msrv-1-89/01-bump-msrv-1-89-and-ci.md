# Agent Note：把 MSRV 抬到 1.89，并同步 CI action 与依赖体检的自动化

Status: implemented

> 实施补记（2026-09-23）：评审期间曾试过「MSRV 1.95 + windows-registry 0.6.1 → 0.100」一轮并整轮落地验证（编译、门禁、`package.ts dev`、CI 全绿；registry 0.100 对本仓仅有的 `CURRENT_USER` / `set_bytes` 两处用法逐字兼容），最终裁回本笔记原案——registry 换代对本仓消费面收益不足，主 crate `windows` 才是换代的大头而它尚未发布。期间核 sparse index 的取证保留：主 crate 0.100 截至 2026-09-23 仍未发布（semver 升序末行即 0.62.2，0.63 亦无），「明确不在本次范围」的主 crate 换代结论不变；若未来跟进，Dependabot 会以红 PR 形式提示。

## 问题

`Cargo.toml:5` 声明 `rust-version = "1.85"`，但这份声明在本仓没有消费者：产物是安装包与绿色 exe（`README.md` 的下载段），不发 crates.io，README 也不承诺源码构建所需工具链；而 `AGENTS.md:79` 反而明文要求开发者与 CI 用最新 stable（「本地工具链落后时 clippy 可能通过而 CI 挂在新 lint 上；提交前若跨过 Rust 小版本，建议 `rustup update stable` 后复跑 clippy」）。数字与实际工作工具链脱节，有三个可观察后果。

后果一：`AGENTS.md:79` 要求「禁止使用超过 MSRV 的语言特性」，执法者是 `.github/workflows/check.yml:40-54` 的 msrv job（用 1.85 工具链跑 `cargo check --locked`）；该 job 已在 main 上连续红过三次，结案方式是「把代码改回旧写法」——现场留在 `src/main.rs:130` 与 `src/update/mod.rs:121` 两行注释里（「let-chain（`if let Ok(h) = hwnd && ...`）在 Rust 1.88 才稳定，本仓 MSRV 为 1.85」）。也就是说，这份声明的实际作用是周期性地要求改写代码，而不是支持任何真实存在的旧工具链用户。

后果二：edition 2024 默认 resolver 3，其 MSRV 感知解析会按包声明的 `rust-version` 优先选择兼容版本（cargo 文档 `resolver.incompatible-rust-versions` 的 fallback 语义），声明压得越低越会回避同一 semver 范围内的更新版本。本仓依赖面很小，但这条机制意味着「声明」与「依赖新鲜度」是同一个旋钮。边界要说清：`windows = "0.62"` 的 caret 范围是 `>=0.62.0, <0.63.0`，家族换代不在解析器管辖内（见「明确不在本次范围」）。

后果三：CI 侧有两处独立落后，且本仓没有任何自动化来发现它们（`.github/dependabot.yml` 与 `rust-toolchain.toml` 均不存在，已逐路径确认）：`actions/checkout@v4` 三处（`.github/workflows/check.yml:17`、`.github/workflows/check.yml:44`、`.github/workflows/release.yml:16`），上游已到 v7；`actions/cache@v4` 一处（`.github/workflows/release.yml:51`），上游已到 v6。`.github/actions/rust-setup/action.yml:22` 的 `Swatinem/rust-cache@v2` 与 `.github/actions/rust-setup/action.yml:17` 的 `dtolnay/rust-toolchain@master` 是线内浮动引用，无需动作。

依赖侧逐条查过 crates.io sparse index：`windows` lock 在 0.62.2（0.62 线终点）、`windows-registry` lock 在 0.6.1（0.6 线终点）、`winresource` lock 在 0.1.31（该 crate 最新），三者都不是「本线内落后」；唯一的新版本是 windows 家族的 **0.100 新一代**（`windows-registry 0.100.0` 声明 `rust-version = "1.95"`，与 `windows-link 0.100.0` 同批发布），那是破坏性换代，不在本笔记。

## 提案

（1）MSRV 抬到 1.89，六处文本同步——这是本笔记的全部改动量，`src/` 一行不碰、依赖版本一个不升：`Cargo.toml:5` 的 `rust-version = "1.85"` 改为 `"1.89"`；`.github/workflows/check.yml:37-39` 注释里的 1.85 改为 1.89；`.github/workflows/check.yml:50` 的 `cache-key: msrv-1.85` 改为 `msrv-1.89`（缓存键必须跟着工具链走，否则 CI 复用错配缓存）；`.github/workflows/check.yml:51` 的 `toolchain: "1.85"` 改为 `"1.89"`；`.github/actions/rust-setup/action.yml:9` 描述里的「MSRV job 传 1.85」改为 1.89；`AGENTS.md:79` 政策句里的 1.85 改为 1.89，并把该句的举例从「1.88 才稳定的 let-chains」换成 1.89 之后仍不可用的例子（例如 1.95 才稳定的 `cfg_select!`），否则这句话在提级当天自相矛盾。建议同笔在 `AGENTS.md:79` 写清升级口径：MSRV 取刻意滞后于 stable 的档位（本次定 1.89），提级时上述六处同步、两个 job 都绿。

选 1.89 而不是 1.88 或当前 stable（审计时本机 stable 为 1.98）的理由：本仓被 MSRV 反复绊倒的语法只有 let-chains（1.88 稳定），1.89 在这条最低需求之上多留一档余量、别无能力诉求；定在 stable 会让 msrv job 与 check job 除 clippy 外完全重合，等于取消对账；1.89 刻意不预支 windows 0.100 换代的前置——该家族声明 `rust-version = "1.95"`，届时换代那一轮自会再抬。若评审坚持更小步，本笔记全部验收项对 1.88 同样成立，只需替换数字。

（2）CI action 升级：`actions/checkout@v4 → v7`（上述三处）与 `actions/cache@v4 → v6`（上述一处）。两条都只能在 CI 上验证（跨大版本可能要求更新的 runner 运行时或改变默认行为），本地无法预演；回退成本是改回版本号一行。

（3）新增 `.github/dependabot.yml`：两个 `package-ecosystem` 条目——`cargo` 与 `github-actions`，`directory: /`，`schedule.interval: weekly`。这是本笔记里唯一防复发的动作：现存三处落后全是人肉发现的；装上之后，`windows` 从 0.62 跳到 0.100 这类破坏性提议会以红 PR 的形式出现（CI 含 msrv job，不兼容当场打红），由人决定何时吃，而不是靠人记得去看。

## 明确不在本次范围

**windows 家族 0.62 → 0.100 换代**不能与本笔记一起做，三条理由：(i) 观察窗口不足——0.100 线是上游最新一批家族发布才进入的，生态尚未跟上（`winresource 0.1.31` 的 dev-dependency 仍钉 `windows ^0.62`），此时迁移等于替上游当早期用户；(ii) 它是破坏性重构，上游该批发布含 Win32 API 路径层面的大改，本仓全部 `windows::Win32::*` 路径、`Cargo.toml:19-43` 的 feature 列表、以及 `AGENTS.md:37` 第 4 条的 `/DELAYLOAD` 延迟导入契约都要复核，属编译驱动的一轮独立工作；(iii) `windows-registry 0.100.0` 声明的 `rust-version = "1.95"` 高于本笔记的目标 1.89，换代的前置留给换代那一轮自行满足，一次只搬一样。实施换代前必须自行核对 `windows` 主线实际发布版本的 `rust-version`：审计时 crates.io 摘要接口与 sparse index 的快照不一致（摘要仍报 0.62.2 为最新，而 `windows-registry` / `windows-link` 的 0.100.0 已可在 index 查到），不要照抄本文的「1.95」当结论。

**两处 let-chain 注释与还原**：`src/main.rs:130`、`src/update/mod.rs:121` 在 MSRV ≥1.88 后成为假陈述，但它们的处置归属同目录 `02-adopt-rust-189-idioms.md`（同一处代码禁止两篇笔记都写，避免并行实施时互相打架），本笔记不改 `src/`。

**不把 `check.yml:50-51` 改成从 `Cargo.toml` 解析 `rust-version`**：workflow 里解析声明会把「独立对账」变成「同源读取」，声明与执法脱节时不再有第二个信号；三写同步的成本是每次提级改三行文本，可接受。本笔记维持该决策，只改数字。

**不新增 `rust-toolchain.toml`**：本机 rustup 默认工具链即 stable，`Cargo.toml` 的 `rust-version` 与 msrv job 已经表达了「最低支持」与「实际执行」两个事实，再加一份「用哪个工具链」的声明只会扩大对账面；`AGENTS.md:79` 关于本地工具链落后导致 clippy 与 CI 不一致的提示已覆盖该场景。

**不升 `winresource`、不动 bun 侧与 `edition`**：`winresource 0.1.31` 是该 crate 最新；`package.json` 的 `@types/bun` 是 `latest` 浮动引用且 `bun.lock` 已钉死可复现；`Cargo.toml:4` 的 `edition = "2024"` 已是最新 edition；均无动作。

**不动 `installer.iss` 与本机 Inno Setup 版本**：CI 打包侧已把 IS 钉在 7.1.0 并用 SHA-256 校验（`.github/workflows/release.yml:51-82`），本机 IS 版本不参与产物一致性判定。

## 为什么不保留？

最强反方一：1.85 是刻意的纪律声明，抬到 1.89 只是把绊线挪过 let-chains 这一道，下次有人写出 1.89 之后才能用的语法还要红一次，问题没有解决、只是延后。回应：这正是需要显式选择的政策，而本仓现状是「声明没有消费者、却持续产生改写成本」——证据是 `README.md` 的下载段（用户拿安装包）与「不发 crates.io」的事实，`AGENTS.md:79` 又要求用最新 stable 开发；把数字定在刻意滞后于 stable 的位置、并把提级写成有节奏的动作，是让绊线继续有意义，而不是取消它。替代方案（保留 1.85）的真实成本写在「风险」。

最强反方二：既然要跟上节奏，为什么不直接抬到当前 stable（1.98）？回应：那会让 msrv job 与 check job 的差别只剩 clippy，对账信号趋近于零；本仓 msrv job 的价值是「声明与执法独立对账」，一个刻意滞后的数字才使它有对象可对。

最强反方三：引入 Dependabot 会带来噪音 PR 与合并负担。回应：噪音是可见且可关的，落后是隐形的——本文列出的三处落后就是隐形成本已经发生的证据；且 CI 会把不兼容升级当场打红，噪音上限是「关掉一个红 PR」。

## 验收标准

`grep -n 'rust-version' Cargo.toml` 的值为 `"1.89"`；`grep -rn '1\.85' .github/ AGENTS.md Cargo.toml` 0 命中（`docs/archive/` 的历史命中不计入）；`grep -n 'msrv-1.89' .github/workflows/check.yml` 1 命中且 `grep -n 'toolchain: "1.89"' .github/workflows/check.yml` 1 命中。

`grep -n 'actions/checkout@' .github/workflows/check.yml .github/workflows/release.yml` 全部为 `@v7`；`grep -n 'actions/cache@' .github/workflows/release.yml` 为 `@v6`。

`.github/dependabot.yml` 存在，且 `cargo` 与 `github-actions` 两个 ecosystem 条目都在。

门禁全绿（`AGENTS.md:72-77` 四条，全部带 `--locked`）：`cargo test --locked`、`cargo build --release --locked`、`cargo clippy --all-targets --locked -- -D warnings`、`cargo fmt -- --check`；MSRV 侧：`rustup toolchain install 1.89` 后 `cargo +1.89 check --all-targets --locked` 通过（CI 上由 `.github/workflows/check.yml:40` 的 job 自动执行同一命令，本地预检只是为了缩短反馈环）。

打包与运行：`bun scripts/package.ts dev` 必须产出安装包且不报错（产物名含 dev 时间戳版本号）；启动 `target\release\traffic-monitor.exe` 后小组件正常嵌入任务栏、双行数据持续刷新、托盘菜单四项可用、`--quit` 能退出已运行实例——此项须用户实机确认，CI 绿不算。

`Cargo.lock`：本笔记预期不改变其内容（`rust-version` 不参与 lock 内容）；若 `git status` 显示它变了，须与 `Cargo.toml` 同笔提交（`AGENTS.md:79` 的规矩）。

## 风险

风险一（真实残留，且是本提案的主要取舍）：若评审认为 1.85 的纪律价值高于本文列出的成本，本笔记应整体放弃、改走回头路；代价是每当开发者或 agent 写出 1.88+ 语法就要改写一次代码（已有先例），且 windows 0.100 换代时还要再搬一次声明。证伪本文前提的方式：若未来出现源码构建的消费者（README 增加源码构建章节、或本仓依赖被外部 crate 复用），「没有 1.85 消费者」这一前提失效，本文应重评。

风险二：action 跨大版本（checkout v4→v7、cache v4→v6）无法本地预演，只能在 PR 上由 CI 验证；若新版本要求更新的 runner 或改变默认行为（例如缓存命中语义），表现为 CI 红，回退成本为改回一行版本号。

风险三：1.89 工具链本机当前不存在（只有 1.85 与 stable/nightly），本地复现 msrv job 需先 `rustup toolchain install 1.89`；不装也不影响 CI 执法，只是反馈环变长。

风险四：Dependabot 首轮会扫出若干 PR；若出现 windows 0.62 → 0.100 这类破坏性提议，属预期行为（关闭即可），但它会反复重开——直到「明确不在本次范围」里那轮换代落地，或 manifest 被显式钉住。
