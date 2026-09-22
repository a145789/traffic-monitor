---
name: rfc-local-loop
description: 本地串行实施 docs/rfc/ 下的待办 RFC 笔记——顶层 agent 只做编排与机械断言，每篇派一个全新编写子 agent（commandcode/xiaomi/mimo-v2.6-pro）与一个全新审阅子 agent（commandcode/z-ai/glm-5.3-flash），编写方自跑门禁并暂存（不提交），审阅通过后由审阅方提交、快进 main、自动归档笔记，一篇接一篇直到待办清空。全程本地，不 push、不开 PR。仅当用户要求「本地串行、不推送、自动归档」地实施 RFC 时使用；要开 PR、要推送、或要等用户逐篇确认功能可用时改用 rfc-ship。
---

# 本地串行 RFC 流水线（编排者只编排）

每篇 RFC 一轮；一轮 = 编写方建分支实施并暂存 → 审阅方按规范验收 → 通过后由审阅方提交、把 main 快进到分支、自动归档笔记。串行执行，main 每次只前进一笔提交。

本 skill 自包含，不引用别的 skill 的正文。它与 `rfc-audit`（出题）、`rfc-ship`（改代码 → PR → 用户确认后收口）的分工是**互斥**的：本 skill 的交付物是本地 main，且在用户授权下**自动**归档。

## 0. 角色边界（编排者必读）

编排者 = 当前**顶层** agent，也就是你。你不是实现者、不是审阅者。

**你可以做**：只读 git 元数据命令（`status`/`diff --name-only`/`diff --cached --name-only`/`rev-parse`/`branch --list`/`merge-base`/`log`/`show --stat`）、`git write-tree`（只写一个悬空 tree 对象，不动 ref/索引/工作区）、`Test-Path`/`Get-Item`（mtime 与存在性断言）、派发 `subagent`、读写 `target/rfc-run/**`。

**你绝对不做**：

- 不编辑 `src/`、`docs/` 或任何被跟踪文件；不修代码、不"顺手帮它改一下"。
- 不读 `git diff` 正文、不读源码、不读笔记正文——只读**文件名清单、指纹、mtime、门禁日志的路径与退出状态、审阅方的短 JSON**。你的上下文是流水线的控制面，一旦读进 diff 就不再干净。
- 不执行任何 git 写操作（`add`/`commit`/`checkout`/`switch`/`rebase`/`branch`/`mv`/`reset`/`stash`/`clean`）。全部由子 agent 执行。
- 不派 `subagent_fork`（会继承编排上下文，"每篇全新 agent"随之失效），不派 `spawn_teammate`，不用 `workflow`。每个子 agent 都是一次性 `subagent`，`run_in_background: false`（下一步依赖它的结果）。
- 不改 `Cargo.toml`、不改 `docs/rfc/README.md`；发现规范需改，报告给用户。

**子 agent 只能一次一个**：同一时刻只允许一个 agent 在写工作区。串行是这套流程全部安全性的前提，不要为了快改成并行。

**上下文预算**：每个子 agent 自己把长产物写进 `target/rfc-run/<runId>/`，最终回复只给 ≤10 行摘要 + 产物路径。你把 `w<N>.md` / 门禁日志当**路径**用，不要读进上下文；审阅方的 verdict JSON 因为已瘦身（不内嵌门禁输出）可以读。

## 1. 前置检查（建任何子 agent 之前，你亲自执行）

任一不满足就**不要开工**，原样报告命令输出并停下来问用户：

```powershell
git rev-parse --abbrev-ref HEAD          # 必须是 main
git status --porcelain                   # 必须为空（含未跟踪文件）；非空即停，不 stash、不清理
Test-Path target                          # 必须为 True（/target 已在 .gitignore，运行产物写这里永不脏工作区）
grep -rn "^Status: *proposed" docs/rfc/   # 只认 docs/rfc/<会话目录>/NN-*.md 的命中；0 条就如实报告「当前没有待办」，不要自己找活
```

另外逐条核对：

- **冻结清单**：把命中的笔记按 (会话目录名, `NN`) 升序定序，写进 `target/rfc-run/<runId>/state.md`。用户指定了子集或起始篇就用用户的。
- **分支不冲突**：每篇的目标分支名 `rfc/<NN>-<topic>` 用 `git branch --list <name>` 确认不存在（本仓库已有多条 `rfc/*` 分支，冲突是真实风险）。
- **串篇检查**：该篇所属会话目录里若还有别的 `proposed`，一并进清单，不要跳着做。
- **不检查 `origin/main`**：本流程只在本地推进，main 合法地领先于 origin/main，不要拿它当断言。
- **授权确认**：自动归档需要用户明确授权（`docs/rfc/README.md` 的状态语义一节已写入该例外）。用户没授权就别自动归档，改为合并后停下、把笔记留在 `docs/rfc/` 并报告待确认。

`runId` 用 `$env:DSH_SESSION_ID` 去掉 `session-` 前缀后的前 6 位 + 起始时间戳（如 `a1b2c3-20260920-1430`）。

## 2. 每轮时间线

| 步 | 执行者 | 动作 | 产物 |
|:--|:--|:--|:--|
| 1 | 你 | 机械断言（见第 4 节 A） | — |
| 2 | 编写子 agent | 建分支 → 实施 → `cargo fmt` → 归档笔记 → 暂存 → 四条门禁 | `w<K>.md`、`w<K>-gate-*.log`、`w<K>-index.txt` |
| 3 | 你 | 交接断言（见第 4 节 B）；不通过就把日志路径退回第 2 步重跑 | 指纹 |
| 4 | 审阅子 agent | 只读验收：diff × 笔记 × AGENTS.md × 门禁日志 | `r<K>.json` |
| 5 | 你 | 判定（见第 6 节）。`lgtm` → 第 6 步；`changes_requested` 且有 blocker → 第 2' 步；无 blocker → 按 lgtm；`blocked` → 弃轮协议 | — |
| 2' | 编写子 agent（全新实例） | 修订：接受或反驳，重跑门禁，重新暂存 | `w2.md`、`w2-gate-*.log` |
| 6 | 审阅子 agent（同模型，独立一次调用） | 收尾：提交 → 守卫 → `checkout main` → `rebase <branch>` → 删分支 | 提交 SHA |
| 7 | 你 | 更新 `state.md`，进入下一篇 | — |

最多 **3 轮**审阅（即最多 `w1/r1/w2/r2/w3/r3`）。修订轮一律派**全新** `subagent`，不 fork：上一轮的全部产物靠磁盘路径传递。

## 3. 门禁协议

**命令取 CI 的四条**（`.github/workflows/check.yml` 是真值），编写方负责全部四条：

```powershell
cargo fmt -- --check
cargo test --locked
cargo clippy --all-targets --locked -- -D warnings
cargo build --release --locked
```

- `cargo fmt`（**就地格式化，不是门禁**）是修复动作，必须在暂存之前跑；验证项永远是 `cargo fmt -- --check`。反过来做的后果是验证自己改文件，把索引和工作区打散。
- 每条命令用 `Tee-Object` 把输出落盘：`target/rfc-run/<runId>/<NN>/w<K>-gate-<cmd>.log`。**门禁正文不进任何 LLM 的汇报文本**——审阅方读文件的原始字节，不读另一个模型的转述。这是"命令真的绿了"唯一可信的来源。
- 改了 `Cargo.toml` 必须把 `Cargo.lock` 一并暂存。
- 门禁失败**改代码**，不许 `#[allow]`、不许注释断言、不许删测试变绿。删测试仅在笔记明确要求时允许，且要点名哪条性质由哪条更强的用例接管。

**编排者的 mtime 断言**（免费，抓"跑完门禁又改代码"这类陈旧）：对每个门禁日志，其 `LastWriteTime` 必须晚于**所有本次改动的门禁相关输入文件**（`src/**`、`Cargo.toml`、`Cargo.lock`、`build.rs`）的 `LastWriteTime`。笔记归档是纯文档位移，不计入。

**编排者保留的唯一抽查**：`cargo fmt -- --check`（暖缓存亚秒级，不触发编译）。就地 `cargo fmt` 是最现实的漂移源，而这条正是 CI 的形式。

## 4. 交接断言（你执行，纯元数据）

### A. 每轮开始前

```powershell
git rev-parse --abbrev-ref HEAD                    # main
git status --porcelain                             # 空
git branch --list rfc/<NN>-<topic>                 # 空
```

### B. 编写方报 ready 之后

```powershell
git rev-parse --abbrev-ref HEAD                    # 目标分支
git diff --name-only                               # 必须为空：索引 == 工作区
git diff --cached --name-only                      # 只含本次范围内的路径（例如不得出现仓库根部的杂项文件）
git write-tree                                     # 索引指纹，记入 state.md
git diff --cached --stat                           # 只记行数，不要读正文
```

`git diff --name-only` 非空 ⇒ 编写方漏暂存 ⇒ 退回重跑，不要自己 `git add`。

### C. 审阅给出 lgtm 之后、收尾之前

```powershell
git write-tree                                     # 必须与 B 步指纹一致
```

不一致 ⇒ 审阅对象与待提交内容不是同一份 ⇒ **作废本轮审阅**，退回编写方说明情况后重跑门禁与审阅。修订轮之间指纹**必须变化**，没变说明它没真的改，直接退回。

## 5. 子 agent 提示词

派发时统一带模型路由：编写方 `provider: commandcode` / `model: xiaomi/mimo-v2.6-pro`；审阅方 `provider: commandcode` / `model: z-ai/glm-5.3-flash`。AGENTS.md 已注入子 agent 上下文，提示词里不必要求它再读一遍，只留兜底。

四段提示词都以这段**共同约束**开头（按角色删减不适用项）：

```text
仓库：D:\work_space\life\traffic-monitor（Windows + pwsh）。工作目录即仓库根。
硬禁止：不得派发子 agent、不得使用 workflow 或 team_task_*、不得给其它 agent 发消息、不得自建分支、不得 push、不得 git reset --hard / stash / clean。
长产物一律自己写进指定路径，最终回复只给 ≤10 行结构化摘要 + 产物路径，不要把 diff 或命令输出原文贴进回复。
AGENTS.md 已在你的上下文里；若不在（少数 harness 不注入），先读它、只取第 1–9 条受保护接缝。
```

占位符映射（`{{NN}}` 是笔记序号，`{{K}}` 是当前修订轮，`{{GATE_PREFIX}}` 为 `w1`/`w2`/`w3`）：

| 占位符 | 值 |
|:--|:--|
| `{{RUN_ID}}` | `target/rfc-run/` 下的本次运行目录名 |
| `{{NN}}` | 笔记文件名的两位序号 |
| `{{RFC_PATH}}` | `docs/rfc/{{RFC_DIR_NAME}}/{{NN}}-<topic>.md` |
| `{{RFC_DIR_NAME}}` | 会话目录名（如 `2026-09-19-code-quality`） |
| `{{NOTE_DST}}` | `docs/archive/rfc/{{RFC_DIR_NAME}}/{{NN}}-<topic>.md` |
| `{{BRANCH}}` | `rfc/{{NN}}-<topic>` |
| `{{BASE}}` | `target/rfc-run/{{RUN_ID}}/{{NN}}` |
| `{{GATE_PREFIX}}` | 当前轮前缀 `w1` / `w2` / `w3` |
| `{{GATE_LOG_DIR}}` | `{{BASE}}`（日志即 `{{GATE_PREFIX}}-gate-*.log`） |
| `{{W_MD_PATH}}` | `{{BASE}}/{{GATE_PREFIX}}.md`（本轮作者报告） |
| `{{W_INDEX_PATH}}` | `{{BASE}}/{{GATE_PREFIX}}-index.txt` |
| `{{R_JSON_PATH}}` | `{{BASE}}/r{{K}}.json`（本轮审阅结论） |
| `{{W_PREV_MD_PATH}}` / `{{R_PREV_JSON_PATH}}` | 上一轮的 `w<K-1>.md` / `r<K-1>.json` |

### 5.1 编写（首轮）

```text
你是本轮的实现者（writer），只负责实现、暂存、取证。你不执行 git commit。

【任务】只实施这一篇 RFC 笔记：{{RFC_PATH}}（所属会话目录：{{RFC_DIR}}）
【规范】那篇笔记就是唯一规范，只读它的「问题 / 提案 / 明确不在本次范围 / 验收标准 / 风险」。
禁止读 docs/ 下任何其它内容（含 docs/archive/，那里可能过时）。实施中发现行号、计数、事实与代码不符，就修正笔记本身，让代码与规范一致。

【步骤】
1. 断言 `git rev-parse --abbrev-ref HEAD` 为 main 且 `git status --porcelain` 为空。不满足就返回 STATUS: blocked 并原样报告，不要清理任何东西。
2. git checkout -b {{BRANCH}}（分支名已给定，禁止自创）
3. 实施：逐条落实「提案」；「明确不在本次范围」是硬边界。禁止顺手重构、格式重排、无关重命名、扩大范围。
4. cargo fmt（就地修复，必须在暂存之前）
5. 归档笔记（本轮已获用户授权）：把 `Status:` 行改成 implemented（**只改这一行**），并 git mv 到 docs/archive/rfc/{{RFC_DIR_NAME}}/。不写入日期或提交 SHA。
6. 暂存：只 git add 你这次真正改动的路径（代码路径 + 笔记新旧路径）。绝对禁止 git add -A / -u / .。
7. 跑四条门禁，每条用 Tee-Object 落盘到 {{BASE}}/{{GATE_PREFIX}}-gate-<name>.log（即 {{GATE_LOG_DIR}}）：
     cargo fmt -- --check
     cargo test --locked
     cargo clippy --all-targets --locked -- -D warnings
     cargo build --release --locked
   失败就改代码重跑，不许 #[allow]、不许注释断言、不许删测试变绿。改了 Cargo.toml 要把 Cargo.lock 一并暂存。
8. 断言 `git diff --name-only` 输出为空（工作区改动已全部进入索引）。若第 7 步重跑过程中又动过文件，必须重新 add，直到该断言成立。
9. 把 `git diff --cached --stat` 输出写入 {{W_INDEX_PATH}}，把完整报告写入 {{W_MD_PATH}}。

【返回格式】严格如下，≤10 行，不要贴 diff、不要贴命令输出原文：
STATUS: ready_for_review | blocked
BRANCH: {{BRANCH}}
NOTE_SRC: {{RFC_PATH}}（已归档至 <新路径>）
改动: <文件:行 级别，每条一句，最多 8 条>
门禁: <四条各写 ok/fail + 日志路径>
INDEX: <{{W_INDEX_PATH}}；变更文件数>
未实测: <老实交代>
偏离笔记/疑点: <有就写，没有写"无">
```

### 5.2 审阅（每轮）

```text
你是本轮的审阅者（reviewer）。你只读、只验证：不修改任何文件（唯一例外是写自己的 {{R_JSON_PATH}} 与回复）。

【审查对象】分支 {{BRANCH}} 上的**暂存内容**；规范是笔记（现已归档到 {{NOTE_DST}}）。
【作者自述（仅用于定位，不得作为证据）】{{W_MD_PATH}}

【你必须独立取证，禁止采信作者的任何结论性表述】
1. 通读 `git status --porcelain`、`git diff --cached --stat`、`git diff --cached` 全文（含 rename 检测 `-M`）。
2. 逐条核对笔记「提案」是否真的实现；「验收标准」是否逐条成立（可 grep 的把命令与命中贴进 evidence）。
3. 读门禁日志**原文**：{{GATE_LOG_DIR}} 下四个 .log。它们是 cargo 的真实输出，不是作者的转述。
   明确判断：四条是否真的全绿、日志是否像是对着当前这份暂存内容跑的（例如日志里编译的 crate 与改动是否吻合）。
4. 按 AGENTS.md 第 1–9 条逐条检查受保护接缝是否被碰；检查生产路径是否新引入 unwrap/expect；检查有没有超出笔记范围的改动（顺手重构、格式重排、无关重命名）。
5. 判断是否有用户可观察的回归；公开表面（pub 项、消息号、协议字符串、CLI、注册表键名、窗口类名）有没有变。
6. 归档校验：Status 行是否只改成 implemented（是否**只改了那一行**）、文件是否移入 docs/archive/rfc/<同会话目录>/、有没有写入日期或 SHA。
7. 上一轮问题逐条结案：resolved / unresolved / withdrawn（撤回必须写明为什么你之前的判断不成立）。

【只报真问题】禁止风格洁癖；禁止要求笔记没规定的行为；禁止把「我更喜欢另一种写法」当阻塞项。nit 必须单独标 severity=nit，不得用来卡轮次。

【明确禁止】不要跑 cargo（门禁日志已由作者落盘，重跑只是重复成本）；不要改任何文件；不要 git add/commit/checkout。若你怀疑日志陈旧或与暂存内容不符，返回 blocked 并说明依据，交由编排者处理。

【输出】把下面这份 JSON 同时写到 {{R_JSON_PATH}}，并在回复里原样给出（它必须很短，不要内嵌门禁输出）：
{
  "verdict": "lgtm | changes_requested | blocked",
  "rfc": "{{RFC_PATH}}",
  "branch": "{{BRANCH}}",
  "index_fingerprint": "<你执行 git write-tree 得到的值>",
  "gates": [{"cmd":"cargo test --locked","ok":true,"log":"<路径>"}, ... 四条],
  "archive_ok": true,
  "issues": [{"id":"O1","severity":"blocker|nit","where":"src/util.rs:42","evidence":"<可复现命令/命中，或 AGENTS.md 条款原文>","why_blocking":"<一句>"}],
  "prior": [{"id":"O1","status":"resolved|unresolved|withdrawn","note":"<一句>"}],
  "summary": "<≤3 句>"
}
判定 lgtm 的硬条件：四条门禁全 ok、笔记「验收标准」逐条有证据、归档校验通过、无未结 blocker。任何一条门禁没跑或 ok=false，一律不得 lgtm。
```

### 5.3 编写（修订轮，第 2..3 轮）

与 5.1 同，但换掉任务段并追加：

```text
【本轮是修订，不是重做】分支 {{BRANCH}} 与已暂存内容都在，继续在它上面改，不要重新建分支、不要重新归档（归档已就绪）。
【上一轮产物】你的报告 {{W_PREV_MD_PATH}}；审阅结论 {{R_PREV_JSON_PATH}}；门禁日志目录 {{GATE_LOG_DIR}}

【你对每条 blocker 的两种合法动作】
- ACCEPT：改代码修掉它，并说明改动点。
- REBUT：基于代码事实 / AGENTS.md 条款原文 / 门禁输出反驳，写明证据。
  反驳被接受的判据是审阅者下一轮标为 withdrawn。禁止靠重复主张取胜；禁止靠改代码掩盖问题（删断言、让测试恒真、加 #[allow]）——一旦被发现视为本轮失败。

【本轮额外要求】
- 必须真的改动内容：修订后的索引指纹必须与上一轮不同。
- 必须重跑四条门禁并落盘到 {{GATE_PREFIX}}-gate-*.log（旧日志保留，不要覆盖）。
- 重新 add 后仍要满足 `git diff --name-only` 为空。
- 逐条给出：ISSUE <id>: ACCEPT <改动点> | REBUT <证据>
- 返回格式同首轮，版本号用 w<K>。
```

### 5.4 收尾（审阅者执行，独立一次调用）

```text
你已对本轮给出 lgtm，且四条门禁已由作者落盘且你已核对全绿。现在进入收尾模式：只做 git 操作，不改任何文件。

【守卫】依次执行，任一失败立即停止并原样报告，不要自行救历史：
  git rev-parse --abbrev-ref HEAD              # 必须是 {{BRANCH}}
  git status --porcelain                       # 除已暂存内容外必须为空
  git diff --cached --quiet                    # 期望退出码为 1（即确实有待提交内容）
  git merge-base --is-ancestor main {{BRANCH}} # 期望退出码 0；非 0 说明 main 已前移，停止并报告

【提交】git commit -m "<message>"，message 用本仓库风格（中文 + conventional 前缀，对齐 git log -6），
必须自足并回答五问：改了什么 / 为什么改（一句可核对证据）/ 对本应用的影响（依赖、产物、二进制表面、协议与消息号、定时器与线程；注明哪些实测、哪些只是读代码推断）/ 对用户的影响 / 是否有破坏性变更（必须显式写「无」或「有 + 影响面」）。
**禁止出现任何指向文档的引用**（不得写「详见 docs/rfc/...」「见 RFC 笔记」）。把最终 message 原文完整贴回。

【快进 main】因为串行执行且 main 未前移，rebase 等价于把 main 快进到分支 tip：
  git merge-base --is-ancestor main {{BRANCH}}   # 再确认一次，非 0 立即停
  git checkout main
  git rebase {{BRANCH}}
  git log --oneline -1                            # 必须等于分支 tip
  git status --porcelain                          # 必须为空
  git branch -d {{BRANCH}}

【绝对禁止】git push；git reset --hard；git checkout -- .；git clean；git stash；在 main 上直接 commit；git add -A；删除或改动仓库根部的无关未跟踪文件；修改笔记的 Status 行以外没被授权的文档。

【返回】≤10 行：commit SHA + message 原文 + `git log -1 --stat` 的关键行 + `git status --porcelain`（应为空）+ 当前分支名 + `git branch -d` 结果。
```

### 5.5 弃轮收尾（`blocked` 或超过 3 轮时）

单篇失败**不阻塞整批**，但必须先把工作区还给 main，否则下一篇的前置检查过不去。派审阅者一次"弃轮收尾"：

```text
本轮未通过审阅（原因：{{REASON}}）。执行：
  git rev-parse --abbrev-ref HEAD                 # 必须是 {{BRANCH}}
  git commit -m "wip(rfc): <topic> 未通过审阅，保留分支待续"
  git checkout main
  git status --porcelain                          # 必须为空
**不要** rebase、**不要**删分支、**不要**动 main。工作保留在 {{BRANCH}} 上待人工续做。
返回分支名 + 当前分支 + status 输出。
```

只有**环境级失败**才停整条流水线：git 守卫失败、回不到 main、指纹断言反复不一致、子 agent 反复返回空。

## 6. LGTM 机械判据（你逐条断言，缺一不收口）

1. `verdict == "lgtm"`。
2. `gates` 四条都存在、`ok` 全为 true、每个 `log` 路径存在且 mtime 断言通过（第 3 节）。
3. `index_fingerprint` 与你在第 4 节 B 步记录的值一致（你在 C 步也要重取一次确认未变）。
4. `archive_ok == true`，且你抽查：笔记在 `docs/archive/rfc/<同会话目录>/` 下、`Status: implemented`、无日期/SHA。
5. `prior` 里所有历史 blocker 都是 `resolved` 或 `withdrawn`。
6. 若 `verdict == "changes_requested"` 但 `issues` 里没有 `severity == "blocker"`：**按 lgtm 收口**，把这些 nit 记进最终清单（防止拿 nit 卡轮次）。

## 7. 归档协议

- 归档由**编写方**完成并暂存（文件改动必须在 commit 之前进索引），与代码落在**同一笔提交**里，不存在「代码已合并、规范还在待办目录」的中间态。
- 只改 `Status:` 一行；文件 `git mv` 到 `docs/archive/rfc/<同一会话目录>/`；**不写日期或提交 SHA**（rebase 后会失真）。
- **只移动笔记自身**：同会话目录里的其它文件（例如作为源材料的 `CODE_QUALITY_REVIEW.md`）保持不动。因此全部待办清空后，`docs/rfc/<会话目录>/` 可能仍残留这些非笔记文件——此时 `grep "^Status: *proposed"` 为 0 就说明流水线已清空待办，把残留文件点进交付清单交给用户决定去留。
- 本流程的自动归档依赖用户授权。未获授权时不要归档，改为合并后停在 `proposed` 并报告待确认。
- 归档即时生效，所以**中断后恢复可以直接重新 `grep "^Status: *proposed" docs/rfc/`**——天然只剩未完成项，不需要额外的已完成账本。冻结清单只用于本轮顺序确定性。

## 8. 中断、超限与恢复

- **状态落盘**：每次阶段切换往 `target/rfc-run/<runId>/state.md` 追加一行：时间戳、篇目、轮次、分支、指纹、verdict、未结 issue id。你的上下文被截断也不丢进度。
- **子 agent 超时或返回空**：不要盲目重派。先按 `state.md` + 磁盘产物判断该阶段是否已完成（产物在磁盘上，阶段完成与否不依赖它的回复），再决定补派哪一段。
- **硬上限 3 轮**：超限不合并、不 reset，走 5.5 弃轮收尾，把 dispute 包（双方最后主张 + 门禁日志路径 + 你的判定依据）写进 `target/rfc-run/<runId>/<NN>/dispute.md`，继续下一篇。
- **恢复**：新会话里重新 grep 待办 → 重新冻结清单 → 从第一篇未归档的开始。已合并的篇目已进 main，天然不会重做。

## 9. 收尾交付清单

全部待办处理完后，一次性报告（不要复述 diff 或日志正文）：

| 篇目 | 结果 | 轮次 | 提交 SHA | 备注 |
|:--|:--|:--|:--|:--|

另附：**未完成/被弃轮的篇目**及其分支名、**累计 nit 清单**、**环境级失败**（若有）、`docs/rfc/<会话目录>/` 下残留的非笔记文件、`state.md` 与 dispute 包路径。诚实标注哪些验收标准是实测通过、哪些只是静态核对。
