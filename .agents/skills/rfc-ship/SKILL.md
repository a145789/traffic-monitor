---
name: rfc-ship
description: 在 traffic-monitor 仓库里实施 docs/rfc/ 下的 RFC 笔记——建分支、改代码、跑门禁、提交、推送、用 gh 开 PR，支持把多篇待办 RFC 整合进同一个 PR；并在用户确认功能可用后把笔记移入 docs/archive/rfc/ 并标记 Status implemented。仅当用户要求实施 RFC、开 PR、整合多个 RFC、合并、发布或标记 RFC 完成时使用。只做审计找候选并写笔记时改用 rfc-audit。
---

# RFC 实施：改代码 → 一个 PR（可整合多篇）→ 确认后标记完成

`rfc-audit` 产出笔记，本 skill 负责把笔记变成代码和 PR，并在**用户确认功能 OK 之后**收口状态。

## 姿态：只读 code + 待办笔记，不读其它文档

用户对本仓库的明确要求是**尽量少读、最好不读已有文档**。本 skill 唯一要读的文档是**你要实施的那几篇待办笔记**——它是**规范文件**，不是背景材料，且只读 `## 提案` / `## 明确不在本次范围` / `## 验收标准` 三段。

- **默认不读 `docs/` 下其它任何内容**，含 `docs/archive/`（历史 RFC、审计、调研与**全部已实施笔记**都在那里）。实测 `docs/archive/` 23 篇 173 KB ≈ 5 万 token，约等于把整个 `src/` 再读一遍，且明确可能过时。
- **`AGENTS.md` 通常已由 harness 自动注入上下文**（作为 workspace instructions）。在上下文里就不要再 read 一遍；不在才读一次，取第 1–9 条受保护接缝。
- 需要了解代码现状时**读 code**，不要去找「当初怎么写的」文档。

## 第 0 步：确认待办范围（不要凭记忆）

待办清单的唯一真值：

```
grep -rn "^Status: *proposed" docs/rfc/
```

**只认 `docs/rfc/<会话目录>/NN-*.md` 的命中**；`docs/rfc/README.md` 里的是格式示例，不是待办。按会话目录分组结果，然后：

- **用户指定了某些 RFC** → 只做那些，**只读那几篇**。
- **用户说「把 rfc 都做了」「实施待办」** → 做**全部**待办，整合进**一个** PR。查出来是 0 条就如实报告「当前没有待办」，**不要自己找活干**（那属于审计，用 `rfc-audit`）。
- **待办里存在互相冲突的项**（改同一段逻辑 / 方向相反）→ 拆成多个 PR，并在交付说明里讲清为什么不能合。

RFC 笔记是**规范文件**。实施过程中若发现笔记里的行号、计数、事实与代码不符：**修正笔记本身**，让代码与规范一致。上一版实现的 PR 里如果笔记数字错了，那是要在同一 PR 内改掉的，不是「留待后续」。

## 实施

1. **先 `git status --porcelain` 确认工作区干净**。若有用户未提交的改动，**停下来问**——别把别人的 WIP 一起提进 PR。
2. **建分支**，命名与主题对应，如 `refactor/<topic>`、`feat/<topic>`。
3. **逐条实施笔记里的「提案」**，不要顺手扩大范围。笔记的「明确不在本次范围」小节是**硬边界**。
4. **受保护接缝不可碰**（`AGENTS.md` 第 1–9 条）：Win32 调用顺序、`/DELAYLOAD` 与子进程 `DONE`/`EXIT_MAIN` 协议、单物理网卡锁定、看门狗与 `TaskbarCreated` 重建、`WM_DPICHANGED` 重嵌、挂起/恢复定时器对称、RAII 守卫归属、**会话通知必须先 `WTSUnRegisterSessionNotification` 再 `DestroyWindow`**。
5. **改动要小而可评审**：格式重排、无关重命名、顺手重构都会淹没有效 diff。
6. **发现笔记的方案会造成回归时，停下来报告**，不要照抄执行。判断标准是「用户可观察的行为是否变差」——例如为了「去重」删掉一次跨越长时间窗口的重试，会让用户在付过一次完整下载代价后拿到报错，那是回归而非简化。
7. **公开 API、中文文案、协议字符串**要么不变，要么在 PR 里明确写出来。

## 门禁（每次改完 Rust 源码后全跑）

```
cargo fmt -- --check
cargo test --locked
cargo clippy --all-targets --locked -- -D warnings
cargo build --release --locked
```

- **`--all-targets` 必须带**：CI 用 `cargo clippy --all-targets --locked -- -D warnings`（`check.yml`），只跑 `cargo clippy` 会漏掉 test/bench target，本地绿而 CI 红。
- **`--locked` 必须带**：CI 的 test/clippy/build 三条全带。改动依赖时（`rfc-audit` 的常见候选就是「只为测试存在的包」），不带 `--locked` 会静默改写 `Cargo.lock` —— 本地绿、CI 因 `--locked` 直接红。**动了 `Cargo.toml` 就必须把 `Cargo.lock` 一起提交。**
- `cargo fmt` 可以直接跑（会就地重排），只要提交前格式化过就等价于 CI 的 `--check`；改了格式就重新跑一次门禁。
- 为确认不是增量缓存的假绿，可 `cargo clean -p traffic-monitor` 后强制重编。
- 本地工具链落后于 stable 时，clippy 可能本地通过而 CI 挂在新 lint 上；跨过 Rust 小版本就 `rustup update stable` 后复跑。
- 门禁失败**修代码**，不要绕。删除测试导致数量下降是预期的，但必须在 PR 里点名**哪条性质由哪条更强的用例接管**。

## 提交

- 仓库风格：**中文** + conventional 前缀（`refactor:` / `fix:` / `feat:` / `docs:` / `ci:` / `perf:`）。
- 正文按「**前因：** … / **后果：** …」两段写，最后一行给验证结果。先 `git log -6` 对齐当下风格。
- 一次提交一个自洽单元；修复自己引入的问题**单独一笔**，把「我原先判错了什么」写清楚——这是有效信息，不要 amend 掉。
- **提交信息必须自足，且禁止指向文档**：不许出现「详见 `docs/rfc/...`」「见 RFC 笔记」「参考某文档」这类把答案推给别处的话。评审只读 main 上的这条信息就该看懂整次改动——文档可能过时、可能不在他手上、也可能已归档进禁读区。

### 提交信息 / PR 描述五问（逐条正面回答，不许留空）

1. **改了什么**：动了哪些模块、函数、常量、依赖；公开表面（`pub` 项、消息号、协议字符串、CLI 参数）是否变化。
2. **为什么改**：问题是什么、凭什么说它是问题——给结论性证据（`file:line` 或一句可核对的判断），**不要把整篇 RFC 抄进来**。
3. **对本应用的影响**：依赖与构建产物、二进制表面、窗口类/消息号/协议字符串、定时器与线程行为、发布产物与安装器；并注明**哪些是实测、哪些只是读代码推断**。
4. **对用户的影响**：用户能看到/感觉到什么变化。纯内部清理就**明说「无用户可见变化」**；有差异就写出边界（例：无网卡场景退避触发从 ~6s 提前到 ~5s，结论不变）。删了测试要说明**没丢覆盖**。
5. **是否有破坏性变更**：**必须显式写一句**——「破坏性变更：无」或「破坏性变更：有 —— <影响面与升级路径>」。本项目的破坏性变更指：更新协议与 `version.txt` 格式、安装器路径/文件名、注册表键名、窗口类名与消息号、托盘或配置的持久化格式，以及任何需要用户手工干预才能继续使用的行为变化。**拿不准就按「有」写**并说明影响面。

适用范围：分支上的中间提交只要有主题 + 前因/后果即可；**落到 main 的那条提交信息**（squash 后的那条 / 合并提交）**与 PR 描述必须五问齐全**。

### 骨架（照这个形状写，数字换成真实的；小改动每行一句话即可）

    refactor: <动作导向的一句话主题>

    前因：<问题是什么 + 一句可核对的证据 file:line>
    后果：<怎么改的 + 行数级规模 + 测试增删了哪些、由哪条更强的用例接管覆盖>
    改了什么：<模块与公开表面（pub 项/消息号/协议字符串/CLI）变化>
    对本应用的影响：<依赖/构建产物/协议/窗口类/定时器；标注【实测】还是【读代码推断】>
    对用户的影响：<无用户可见变化 / 具体差异与边界>
    破坏性变更：无
    验证：<实际跑过的命令与结果，未跑过的写「未实测」>

## PR

**整合能力是本 skill 的核心要求**：多篇 RFC 合进一个 PR 时，描述必须把各篇的实施与影响**统一叙述**，而不是把 N 篇笔记的标题堆在一起。

**PR 描述 = 提交信息**（squash 合并后它就是 main 上唯一那条），所以同样按上面的「**五问**」逐条正面回答：改了什么 / 为什么改 / 对本应用的影响 / 对用户的影响 / 是否有破坏性变更。**不要出现指向文档的引用**（「详见 `docs/rfc/xxx`」「见 RFC 笔记」「规范见某文件」）——多篇整合时尤其别用「见第 N 篇笔记」代替把影响写清楚。

另需：

- 列出实际执行的验证命令与结果。
- 一节「**本 PR 不包含**」，列出刻意排除的项，让评审能界定边界。
- **诚实交代未实测项**。没跑过的运行时验证就写「未实测」——不要用「等价性已证明」掩盖只做过静态分析这件事。
- 破坏性变更即使为「无」也**要写出来**，不要沉默留白。

用 `gh pr create` 开 PR，草稿仅用于调查仍在扩张时。

## 用户确认后才标记完成

**硬性前置条件：必须等到用户明确说功能 OK / 验收通过。** 在此之前不要动状态行——CI 绿不等于功能确认。

用户确认后，对**本 PR 覆盖的**每篇笔记：

1. 把状态行改为 `Status: implemented`（`proposed` → `implemented`）。**只改这一行**，标题、正文、验收标准、风险段、相对链接全部不动。
2. 把笔记从 `docs/rfc/<会话>/` 移到 `docs/archive/rfc/<同一会话>/`。移动会**多一层目录**，链接深度随之变化：`../../../src/` 必须改成 `../../../../src/`，并逐条验证链接可达。
3. **不要写日期或提交 SHA 进笔记**——它们只在 git 历史里，rebase 后会失真。
4. 状态与移动**与该 PR 一起**落地：amend 进 PR 分支或加一笔 `docs:` 提交，再合并。这样不存在「代码已合并、规范仍写在待办目录」的中间态。

## 合并

**合并与改写 main 都是不可逆操作：执行前先向用户确认。**

- **默认把 PR 描述作为 main 上的提交信息**：`gh pr merge <n> --squash --delete-branch --body-file <描述文件>`（`-F/--body-file`、`-t/--subject` 均已确认可用，`-F -` 走 stdin）。
- ⚠️ **`--squash` 默认不会用 PR 描述**——它拼接分支上的各条提交信息，那样 main 上留下的是零散过程信息，违反「只读提交信息就懂」。合并后 `git log -1` 复核落到 main 的那条：五问齐不齐、有没有指向文档的引用；不对就改（改写 main 见下条）。
- 「提交信息不对」是可以用 `git commit --amend` 改的，**但主分支改写必须走 `git push --force-with-lease`，且推之前先确认远端只有你待改的那一个提交**（`git rev-list --count <old-main>..origin/main` 应为 1，且 `git diff --stat <old> HEAD` 必须无输出证明树未变）。
- 合并后同步本地：`git fetch` → `git merge --ff-only origin/main`。

## ⚠️ 本环境已知的坑（踩过，别再踩）

| 现象 | 原因与对策 |
| :--- | :--- |
| `rg: not recognized` | **`rg` 不在 PATH**。用 grep 工具或 `Select-String`。给子代理下指令时必须交代。 |
| `bun`/node `execSync` 报 `EPERM uv_spawn cmd.exe` | 沙箱拦**命名管道**：`stdio:'inherit'` 可用，捕获输出不可用。`scripts/release.ts` 依赖捕获（`git rev-parse` 等），必须提权重试原命令，**不要改脚本绕过**。 |
| `git push` 报 `couldn't create signal pipe, Win32 error 5` | MSYS ssh 缺陷（本仓库 origin 走 `git@github.com:`）。用 `$env:GIT_SSH='C:\Windows\System32\OpenSSH\ssh.exe'` 后重试。**不要改 git 配置**。 |
| `gh run watch` / `gh pr view` 偶发 `EOF` | api.github.com 瞬时错误。加重试轮询，不要据此判定失败。 |
| PowerShell `$s.Contains('子串')` 全 False | `git log --format='%B'` 返回的是**字符串数组**，`Contains` 退化为精确元素匹配。用 `Out-String` 或 `-join` 合并后再判断。 |
| PowerShell `.Length` 给出奇怪小数 | 同上，那是**行数**不是字符数。多行输出先落盘再用 `Get-Item` 看字节数。 |
| `Invoke-WebRequest` 抓 GitHub 报 TLS 认证失败 | 该通道不可用；用 `gh release download` / `gh api` 取产物。 |
| 新建 Markdown 的行尾（CRLF/LF） | 仓库 `core.autocrlf=true`、无 `.gitattributes`。**实测 LF 与 CRLF 归一化成同一 blob、`git status` 都干净**，所以 LF 不会造成纠缠；写 CRLF 只是与工作区其余文件一致。**不要为此做全仓 EOL 转换**（那会制造巨大无意义 diff）。 |

## 发布（仅当用户明确要求时）

用户说「发版」「release」才做，不要在实施任务里顺手发。

- 中版本 = minor 递增。改前先 `git tag --list 'v1.*'` 看现有序列。
- 命令：`bun scripts/release.ts <x.y.z>`（脚本自带分支/工作区/版本递增/标签占用校验，跑 clippy+test+两次构建，然后提交、打 tag、推送）。**该脚本需要提权**（见上表管道限制）。
- ⚠️ 脚本自己那条 clippy 是 `cargo clippy -- -D warnings`（**没有 `--all-targets`**），而且它跑完就直接推 tag。**发版前自己补跑一次 `cargo clippy --all-targets --locked -- -D warnings`**，否则可能给一个 CI 会拒的 commit 打上 tag。
- 脚本会跑 `cargo update --workspace` 并提交 `Cargo.lock`。**发布前务必确认三处版本号一致**：`Cargo.toml`、`installer.iss` 的 `AppVersion`、`Cargo.lock`——CI 用 `--locked`，lock 滞后会直接失败。
- 产物校验（这是发布的实际交付物，不能只看工作流绿灯）：release 工作流成功后确认三个附件（`TrafficMonitor-Setup-<ver>.exe`、`traffic-monitor.exe`、`version.txt`），并**独立下载安装包算 SHA256 与 `version.txt` 逐字比对**；再确认 `version.txt` 恰好两行、版本号为纯数字 `x.y.z`。自动更新的校验失败全部源于这一处。
- `version.txt` 第二行是**大写** hex（发布流水线做了 `tr a-z A-Z`；主程序比较时两边都转大写，所以大小写不影响功能，但比对时别拿小写去撞）。
