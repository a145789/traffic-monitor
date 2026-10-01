# Agent Note：加固安装器与发布链

Status: superseded（第 3 项「升级复活开机自启」拆出为 09-stop-upgrade-resurrecting-autostart.md 保留；其余四项经复核否决：第 1 项前提与 Inno 官方文档相反、第 2/4/5 项非用户可感知）

## 问题

**其一，兜底强杀的重试判据漏判「脚本没跑起来」。** `installer.iss:158-164` 的循环只看 `ResultCode = 0` 就 `Exit` 出循环，而 `Exec`（`:161`）本身是返回布尔的函数：进程没起来时它返回 `False`，`ResultCode` 保持初值 0，安装器于是把「一次都没执行」误判成「已清干净」，`ForceKillMaxAttempts`（`:79`）的兜底轮次被整体跳过。后果是复制阶段仍可能撞上旧实例占用（`installer.iss:62-72` 的注释说明这正是要修的场景），安装静默回滚。

**其二，`kill-remnant.ps1` 的 UTF-8 BOM 是一条没有任何校验的隐式约束。** 脚本头 `installer/kill-remnant.ps1:1-5` 自述：安装器用 Windows PowerShell 5.1 执行它，**无 BOM 时按 ANSI 代码页解码，中文会变成乱码且可能吃掉引号**。当前文件确实以 `EF BB BF` 开头（已实测首字节），但全仓没有任何测试或 CI 步骤断言它——`grep` 在 `.github/` 下对 `BOM`、`kill-remnant` 均 0 命中；唯一读该文件的用例 `src/update/installer.rs:836-908` 只验筛选判据，不验编码。一次普通编辑器另存即可破坏它，而且破坏后的表现是「脚本行为变了」而非「构建失败」。

**其三，同一个 HKCU Run 值有两个属主。** 安装器的 `[Registry]` 在 `installer.iss:43-44` 写 `HKCU\...\Run\TrafficMonitor`（由 `:39-41` 的 `startup` 任务控制），应用的托盘开关读写**同一个值**（`src/tray.rs:277-290`，路径常量 `src/config.rs:50`，菜单 ID `src/config.rs:224`，分发 `src/tray.rs:266`）。Inno 会跨安装记住任务的勾选状态，因此「用户在托盘里关掉开机自启（值被删除）→ 下次升级按记忆的任务重新写回」是可能的。附带问题：该值由提权安装器写 `HKCU`，在「标准用户 + 管理员凭据」场景会落到提权账户的 hive（`installer.iss:21-23` 已用 `UsedUserAreasWarning=no` 抑制 IS 的警告，并注明知情取舍）。

**其四，发布工具的中间态与失败续做。** `scripts/package.ts:45-49` 的版本改写发生在 `try`（`:51`）**之前**，恢复只在 `finally`（`:75-84`）：进程在改写完成到进入 `try` 之间被强杀（Ctrl+C、终端关闭）会留下 `x.y.z-<tag><ts>` 的临时版本，`Cargo.lock` 的根包版本也不恢复，随后 `release.ts:75-85` 的版本一致性门会拒绝发版而脚本不留自愈入口。`scripts/release.ts:131-136` 的 `git add`/`commit`/`tag` 三步都是裸调用，只有 push（`:142-148`）带 catch 与续做指引——若 commit 成功而 tag 失败，仓库停在「版本已改写 + 已有 release commit + 无 tag」，脚本不输出任何下一步，与 AGENTS.md「按脚本输出继续」的要求脱节。

**其五，发布作业的第三方 action 未钉 SHA，且权限过宽。** `.github/workflows/release.yml:16-17` 让整个作业持有 `contents: write`，而同作业的 `actions/checkout@v7`（`:24`）、`actions/cache@v6`（`:62`）是浮动标签，`.github/actions/rust-setup/action.yml:17` 更是 `dtolnay/rust-toolchain@master`（**可变分支**）。上游任意一次提交都会进入持有写 token 的发布作业，可以在 `ISCC.exe` 调用（`release.yml:125-127`）之前改产物、改 `version.txt` 生成（`:129-137`）、改 publish 门禁。对比：本仓对 Inno Setup 原料做了 SHA-256 双钉死（`:74-81`）、对中文语言文件钉了 40 位 commit（`:121-123`），dependabot 同时管理 cargo 与 github-actions（`.github/dependabot.yml`）——也就是说「钉住」这件事本仓会做，只是没用在 action 上。另需明确：`.github/workflows/check.yml:9-10` 已经是 `contents: read`，**不要**按误报去改它。

## 提案

1. `ForceKillRemnant` 判 `Exec` 的返回值：`Exec` 返回 `False` 时视为本轮失败并重试；只有 `Exec` 成功且 `ResultCode = 0` 才 `Exit`。注释同步说明「完成判定同时来自脚本退出码与进程启动成功」。
2. 给 BOM 加一条断言：在 `src/update/installer.rs:836-908` 那条既有用例旁加一条「`installer/kill-remnant.ps1` 前 3 字节必须是 `EF BB BF`」的测试（该用例本来就已从磁盘读该脚本，成本一行），或在 `.github/workflows/check.yml` 加一步等价校验。
3. 自启项：**保留向导里的勾选框，但把写 Run 值限制在「全新安装」那一次**。做法是给 `[Registry]` 的 Run 条目加 `Check:` 守卫（或把这次写入搬进 `[Code]` 的 `ssPostInstall`），判据是「卸载信息键不存在 ⇒ 全新安装」：全新安装且勾选了 `startup` 任务才写这一行，**升级一律不碰它**。此后 Run 值的唯一写方是应用（`src/tray.rs:277-290`），「用户关了自启又被升级写回」这个毛病从结构上消失，且不新增任何注册表值、不损失首次安装的勾选框。被否决的两个替代方案见「为什么不保留」第 3 条。
4. `scripts/package.ts` 把两处 `writeFileSync` 移入 `try`（或先注册进程级退出钩子），保证任何退出路径都落到同一处恢复；`scripts/release.ts` 给 `add`/`commit`/`tag` 加同一处 catch，输出「已完成的步骤 + 精确续做命令 + 不要重跑脚本」。
5. 第三方 action 全部钉 40 位 commit SHA（dependabot 可维护 pin）；把 `contents: write` 从作业级下沉到真正写 Release 的步骤，其余步骤用 `contents: read`。

## 明确不在本次范围

- 不改三方版本一致性门（`scripts/release.ts:75-85`、`release.yml:36-48`）与「同 tag 资产不可静默覆盖」的意图（`release.yml:245-247`）。
- 不改 repair 入口的准入与范围（`release.yml:226-243`）。
- 不改三条资产名、`version.txt` 格式与更新器的资产定位（`src/update/mod.rs:298-300`）。
- 不改 Inno Setup 原料与语言文件的既有钉法（`release.yml:74-81`、`:121-123`）：它们已经正确。
- 不改安装器入口的优雅退出与强杀筛选判据（`installer.iss:102-157`、`installer/kill-remnant.ps1:53-57`）：三项收窄（目标路径 + 当前会话 + 可命令行）是既有裁定，且已有用例钉住。
- 不修「标准用户 + 管理员凭据」安装时勾选框写到管理员 hive 这件事（`installer.iss:21-23` 已用 `UsedUserAreasWarning=no` 注明为知情取舍）：可靠路径是应用自己的托盘开关（运行在正确的用户下），本次不动这个既有决策。

## 为什么不保留？

1. 「`ResultCode` 是脚本的退出码，足够了。」—— `Exec` 失败时它保持初值 0，恰好看成成功；判据必须同时含「进程起来了」这一事实。
2. 「BOM 现在是好的，将来也不会有人改。」—— 这条约束的表现形式是「静默改变行为」而非「构建失败」，正是最需要断言的一类；成本是一行断言。
3. 「两个属主各管一段，不冲突。」—— 它们写的是同一个值的两个方向（写/删），且安装器会按记忆状态重新写；这正是「同一事实只能有一个状态属主」要排除的形态。两个看似更彻底的替代方案都被否决：**改写成「意图值」、由应用收敛真值**的问题在权限范围上打架——标准用户用管理员凭据安装时安装器写进的是管理员账户的 hive，而应用运行在标准用户的 hive 里读不到，要修就得写机器级 HKLM，可托盘开关是普通用户权限、写不了 HKLM，方案越修越复杂；**删掉安装器侧勾选框、自启只由托盘管**虽然属主最干净，但这是任务栏常驻组件，装完重启发现组件不在会被当成「装坏了」，为少一个属主牺牲首次安装体验不划算。因此取「安装时初始化一次、此后应用独占」这条边界。
4. 「脚本出错手动收拾就行。」—— AGENTS.md 明确要求发版失败后「按脚本输出继续」，而当前输出缺失；把中间态与续做命令写进脚本是最低成本的对齐。
5. 「action 钉 SHA 会增加升级成本。」—— dependabot 已在管 github-actions（`.github/dependabot.yml`），维护成本由它承担；而一个 `@master` 引用在持写 token 的发布作业里等于把发布链的完整性交出去。

## 验收标准

- `grep -n "ResultCode = 0" installer.iss` 命中处同时带 `Exec` 返回值的判断。
- 新增 BOM 断言用例通过（或 CI 步骤命中）；`grep -n "EF BB BF\|0xEF" src/update/installer.rs .github/workflows/check.yml` 命中 ≥1 处。
- 自启项：`grep -n "Check:\|IsFirstInstall" installer.iss` 命中守卫（或 Run 写入已移入 `[Code]`），`installer.iss` 的 Run 条目不再对升级无条件生效，`grep -n "TrafficMonitor" installer.iss src/tray.rs` 显示安装期之外只有应用在写；真机两例：①全新安装勾选「开机自动启动」→ Run 值写入且重启后组件自动出现；②托盘关掉自启 → 升级一次 → Run 值仍为空（不再复活）。
- 脚本：`grep -n "writeFileSync" scripts/package.ts` 命中处全部位于 `try` 内；`grep -n "catch" scripts/release.ts` 覆盖 `add`/`commit`/`tag`/`push` 四步。
- CI：`grep -n "uses:" .github/workflows/*.yml .github/actions/rust-setup/action.yml` 的第三方 action 全部为 40 位 SHA（本地 composite action `./.github/actions/rust-setup` 例外）；`grep -n "permissions:" .github/workflows/release.yml` 下 `contents: write` 只在需要的步骤/作业上。
- 四条门禁全绿；一次真机安装/升级 + 一次 release 演练（dry-run 或 repair 前的 draft 流程）。

## 风险

- 钉 SHA 后升级需要人工或 dependabot 跟进，长期失修会把工具链冻在旧版本；需接受这是供应链安全的固定成本。
- 自启项的改动会在安装器里引入一个「本机是否已装过」的判据（卸载信息键），若该键被清理工具误删，一次升级会被当成全新安装而重新写回 Run 值；因此实施时要取 Inno 的官方判据（`RegKeyExists` 查卸载键）而不是自造标记，并保留「托盘关掉后升级仍为空」这条真机验收。
- `installer.iss` 的改动本机无法编译验证（无 ISCC），只能由 release 工作流首次编译验证——这是本节最大的验证缺口。
- BOM 断言只保护「已写明的隐式约束」，不覆盖脚本里其他编码假设（例如未来新增非 ASCII 字面量时的行为）。
- 第 5 项只解决「谁能改发布作业」，不解决 secret 的最小权限与私钥保管；后者见 `04-anchor-installer-trust-with-signature.md`，两篇应一起实施。
