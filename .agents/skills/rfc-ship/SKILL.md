---
name: rfc-ship
description: 在 traffic-monitor 仓库里实施 docs/rfc/ 下的 RFC 笔记——建分支、改代码、跑门禁、提交、推送、用 gh 开 PR，支持把多篇待办 RFC 整合进同一个 PR；并在用户确认功能可用后把笔记迁入 docs/rfc/archive/ 并标记 Status implemented。仅当用户要求实施 RFC、开 PR、整合多个 RFC、合并、发布或标记 RFC 完成时使用。只做审计找候选并写笔记时改用 rfc-audit。
---

# RFC 实施：改代码 → 一个 PR（可整合多篇）→ 确认后标记完成

`rfc-audit` 产出笔记，本 skill 负责把笔记变成代码和 PR，并在**用户确认功能 OK 之后**收口状态。

## 第 0 步：确认待办范围（不要凭记忆）

待办清单的唯一真值是状态行：

```
grep -rn "^Status: *proposed" docs/rfc/
```

按会话目录分组结果。然后：

- **用户指定了某些 RFC** → 只做那些。
- **用户说「把 rfc 都做了」「实施待办」** → 默认**全部待办**，整合进**一个** PR。
- **待办里存在互相冲突的项**（改同一段逻辑 / 方向相反）→ 拆成多个 PR，并在交付说明里讲清为什么不能合。

RFC 笔记是**规范文件**。实施过程中若发现笔记里的行号、计数、事实与代码不符：**修正笔记本身**，让代码与规范一致。上一版实现的 PR 里如果笔记数字错了，那是要在同一 PR 内改掉的，不是「留待后续」。

## 实施

1. **建分支**，命名与主题对应，如 `refactor/<topic>`、`feat/<topic>`。
2. **逐条实施笔记里的「提案」**，不要顺手扩大范围。笔记的「明确不在本次范围」小节是**硬边界**。
3. **受保护接缝不可碰**（`AGENTS.md` 第 1–9 条）：Win32 调用顺序、`/DELAYLOAD` 与子进程 `DONE`/`EXIT_MAIN` 协议、单物理网卡锁定、看门狗与 `TaskbarCreated` 重建、`WM_DPICHANGED` 重嵌、挂起/恢复定时器对称、RAII 守卫归属、**会话通知必须先 `WTSUnRegisterSessionNotification` 再 `DestroyWindow`**。
4. **改动要小而可评审**：格式重排、无关重命名、顺手重构都会淹没有效 diff。
5. **发现笔记的方案会造成回归时，停下来报告**，不要照抄执行。判断标准是「用户可观察的行为是否变差」——例如为了「去重」删掉一次跨越长时间窗口的重试，会让用户在付过一次完整下载代价后拿到报错，那是回归而非简化。
6. **公开 API、中文文案、协议字符串**要么不变，要么在 PR 里明确写出来。

## 门禁（每次改完 Rust 源码后全跑）

```
cargo fmt
cargo test 2>&1
cargo clippy --all-targets -- -D warnings
cargo build --release 2>&1
```

- **`--all-targets` 必须带**：CI 用的是 `cargo clippy --all-targets --locked -- -D warnings`，只跑 `cargo clippy` 会漏掉 test/bench target，本地绿而 CI 红。
- 为确认不是增量缓存的假绿，可 `cargo clean -p traffic-monitor` 后强制重编。
- 门禁失败**修代码**，不要绕。删除测试导致数量下降是预期的，但必须在 PR 里点名**哪条性质由哪条更强的用例接管**。

## 提交

- 仓库风格：**中文** + conventional 前缀（`refactor:` / `fix:` / `feat:` / `docs:` / `ci:` / `perf:`）。
- 正文按「**前因：** … / **后果：** …」两段写，最后一行给验证结果。先 `git log -6` 对齐当下风格。
- 一次提交一个自洽单元；修复自己引入的问题**单独一笔**，把「我原先判错了什么」写清楚——这是有效信息，不要 amend 掉。

## PR

**整合能力是本 skill 的核心要求**：多篇 RFC 合进一个 PR 时，PR 描述必须把各篇的实施与影响**统一叙述**，而不是把 N 篇笔记的标题堆在一起。

描述必须包含这三段（用户明确要求的）：

1. **前因后果**：这些冗余/问题为什么存在、证据是什么、哪条纯函数或测试本来已承担该职责。逐篇对应 RFC。
2. **对用户影响**：纯内部清理就**明说无用户可见变化**，并把笔记记录了的有界差异写出来（例：无网卡场景退避触发从 ~6s 提前到 ~5s，结论不变）。删了测试就要说明**没丢覆盖**。
3. **对应用影响**：公开 API、二进制表面、窗口类/消息号/协议字符串、依赖与构建产物的变化（预期多为「无」），diff 规模，以及**哪些是实测、哪些只是读代码推断**。

另需：

- 点名 RFC 笔记路径作为规范来源。
- 列出实际执行的验证命令与结果。
- 一节「**本 PR 不包含**」，列出刻意排除的项，让评审能界定边界。
- **诚实交代未实测项**。没跑过的运行时验证就写「未实测」——不要用「等价性已证明」掩盖只做过静态分析这件事。

用 `gh pr create` 开 PR，草稿仅用于调查仍在扩张时。

## 用户确认后才标记完成

**硬性前置条件：必须等到用户明确说功能 OK / 验收通过。** 在此之前不要动状态行——CI 绿不等于功能确认。

用户确认后，对**本 PR 覆盖的**每篇笔记：

1. 把状态行改为 `Status: implemented`（`proposed` → `implemented`）。**只改这一行**，标题、正文、验收标准、风险段、相对链接全部不动。
2. 把笔记从 `docs/rfc/<会话>/` 移到 `docs/rfc/archive/<会话>/`。移动会改变相对链接深度，**必须修正链接**（多一层目录 ⇒ `../../../src/` 变 `../../../../src/`），并逐条验证链接可达。
3. **不要写日期或提交 SHA 进笔记**——它们只在 git 历史里，rebase 后会失真。
4. 状态与移动**与该 PR 一起**落地：amend 进 PR 分支或加一笔 `docs:` 提交，再合并。这样不存在「代码已合并、规范仍写在待办目录」的中间态。

## 合并

- `gh pr merge <n> --squash --delete-branch`。
- ⚠️ **`--squash` 不会用 PR 描述作为提交信息**——它拼接分支上的各条提交信息。若你要让 PR 描述成为 main 上的提交信息，必须加 `--body-file`。
- 「提交信息不对」是可以用 `git commit --amend` 改的，**但主分支改写必须走 `git push --force-with-lease`，且推之前先确认远端只有你待改的那一个提交**（`git rev-list --count <old-main>..origin/main` 应为 1，且 `git diff --stat <old> HEAD` 必须无输出证明树未变）。
- 合并后同步本地：`git fetch` → `git merge --ff-only origin/main`。

## ⚠️ 本环境已知的坑（踩过，别再踩）

| 现象 | 原因与对策 |
| :--- | :--- |
| `rg: not recognized` | **`rg` 不在 PATH**。用 grep 工具或 `Select-String`。给子代理下指令时必须交代。 |
| `bun`/node `execSync` 报 `EPERM uv_spawn cmd.exe` | 沙箱拦**命名管道**：`stdio:'inherit'` 可用，捕获输出不可用。`scripts/release.ts` 依赖捕获（`git rev-parse` 等），必须提权重试原命令，**不要改脚本绕过**。 |
| `git push` 报 `couldn't create signal pipe, Win32 error 5` | MSYS ssh 缺陷。用 `$env:GIT_SSH='C:\Windows\System32\OpenSSH\ssh.exe'` 后重试。**不要改 git 配置**。 |
| `gh run watch` / `gh pr view` 偶发 `EOF` | api.github.com 瞬时错误。加重试轮询，不要据此判定失败。 |
| PowerShell `$s.Contains('子串')` 全 False | `git log --format='%B'` 返回的是**字符串数组**，`Contains` 退化为精确元素匹配。用 `Out-String` 或 `-join` 合并后再判断。 |
| PowerShell `.Length` 给出奇怪小数 | 同上，那是**行数**不是字符数。多行输出先落盘再用 `Get-Item` 看字节数。 |
| `Invoke-WebRequest` 抓 GitHub 报 TLS 认证失败 | 该通道不可用；用 `gh release download` / `gh api` 取产物。 |
| CRLF/LF 噪音 | 仓库 `core.autocrlf=true`、无 `.gitattributes`。**新建 Markdown 必须写 CRLF**，否则 `git status` 长期纠缠。 |

## 发布（仅当用户明确要求时）

用户说「发版」「release」才做，不要在实施任务里顺手发。

- 中版本 = minor 递增。改前先 `git tag --list 'v1.*'` 看现有序列。
- 命令：`bun scripts/release.ts <x.y.z>`（脚本自带分支/工作区/版本递增/标签占用校验，跑 clippy+test+两次构建，然后提交、打 tag、推送）。**该脚本需要提权**（见上表管道限制）。
- 脚本会跑 `cargo update --workspace` 并提交 `Cargo.lock`。**发布前务必确认三处版本号一致**：`Cargo.toml`、`installer.iss` 的 `AppVersion`、`Cargo.lock`——CI 用 `--locked`，lock 滞后会直接失败。
- 产物校验（这是发布的实际交付物，不能只看工作流绿灯）：release 工作流成功后确认三个附件（`TrafficMonitor-Setup-<ver>.exe`、`traffic-monitor.exe`、`version.txt`），并**独立下载安装包算 SHA256 与 `version.txt` 逐字比对**；再确认 `version.txt` 恰好两行、版本号为纯数字 `x.y.z`。自动更新的校验失败全部源于这一处。
