# rfc-ship runbook（按需读）

只有两种情况需要打开本文件：**工具报错**，或**用户明确要求发版**。平时不要主动加载。

## 本环境已知的坑

| 现象 | 原因与对策 |
| :--- | :--- |
| `rg: not recognized` | **`rg` 不在 PATH**。用 grep 工具或 `Select-String`。给子代理下指令时必须交代。 |
| `bun`/node `execSync` 报 `EPERM uv_spawn cmd.exe` | 沙箱拦**命名管道**：`stdio:'inherit'` 可用，捕获输出不可用。`scripts/release.ts` 依赖捕获（`git rev-parse` 等），必须提权重试原命令，**不要改脚本绕过**。 |
| `git push` 报 `couldn't create signal pipe, Win32 error 5` | MSYS ssh 缺陷（origin 走 `git@github.com:`）。用 `$env:GIT_SSH='C:\Windows\System32\OpenSSH\ssh.exe'` 后重试。**不要改 git 配置**。 |
| `gh run watch` / `gh pr view` 偶发 `EOF` | api.github.com 瞬时错误。加重试轮询，不要据此判定失败。 |
| PowerShell `$s.Contains('子串')` 全 False | `git log --format='%B'` 返回的是**字符串数组**，`Contains` 退化为精确元素匹配。用 `Out-String` 或 `-join` 合并后再判断。 |
| PowerShell `.Length` 给出奇怪小数 | 同上，那是**行数**不是字符数。多行输出先落盘再用 `Get-Item` 看字节数。 |
| `Invoke-WebRequest` 抓 GitHub 报 TLS 认证失败 | 该通道不可用；用 `gh release download` / `gh api` 取产物。 |
| 新建 Markdown 的行尾（CRLF/LF） | 仓库 `core.autocrlf=true`、无 `.gitattributes`。**实测 LF 与 CRLF 归一化成同一 blob、`git status` 都干净**，LF 不会造成纠缠；写 CRLF 只是与工作区其余文件一致。**不要为此做全仓 EOL 转换。** |

## 改写 main（仅用户手工执行）

改写主分支历史不可逆，且需要判断「远端是否只有你待改的那一个提交」，因此**不由 agent 执行**。需要时请用户按下列步骤操作：

1. `git commit --amend`（或 `git rebase -i`）改好本地提交。
2. 核对：`git rev-list --count <old-main>..origin/main` 应为 `1`；`git diff --stat <old> HEAD` 必须**无输出**（证明树未变，只改了信息）。
3. `git push --force-with-lease`。

## 发布（仅当用户明确要求时）

用户说「发版」「release」才做，不要在实施任务里顺手发。

- 中版本 = minor 递增。改前先 `git tag --list 'v1.*'` 看现有序列。
- 命令：`bun scripts/release.ts <x.y.z>`（脚本自带分支 / 工作区 / 版本递增 / tag 占用校验，跑 clippy + test + 两次构建，然后提交、打 tag、推送）。**该脚本需要提权**（见上表管道限制）。
- ⚠️ 脚本自己那条 clippy 是 `cargo clippy -- -D warnings`（**没有 `--all-targets`**），而且它跑完就直接推 tag。**发版前自己补跑一次 `cargo clippy --all-targets --locked -- -D warnings`**，否则可能给一个 CI 会拒的 commit 打上 tag。
- 脚本会跑 `cargo update --workspace` 并提交 `Cargo.lock`。**发布前务必确认三处版本号一致**：`Cargo.toml`、`installer.iss` 的 `AppVersion`、`Cargo.lock`——CI 用 `--locked`，lock 滞后会直接失败。
- 产物校验（不能只看工作流绿灯）：release 工作流成功后确认三个附件（`TrafficMonitor-Setup-<ver>.exe`、`traffic-monitor.exe`、`version.txt`），并**独立下载安装包算 SHA256 与 `version.txt` 逐字比对**；再确认 `version.txt` 恰好两行、版本号为纯数字 `x.y.z`。自动更新的校验失败全部源于这一处。
- `version.txt` 第二行是**大写** hex（流水线做了 `tr a-z A-Z`；主程序比较时两边都转大写）。比对时别拿小写去撞。
