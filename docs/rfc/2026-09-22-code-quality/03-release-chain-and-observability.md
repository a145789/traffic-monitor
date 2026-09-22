# Agent Note：发布链路硬化与 release 现场可诊断性

Status: proposed

## 问题

复核确认：更新链路的不可信输入侧（网络 + UAC 提权执行）防护是本仓最强的部分——锁定句柄重验、严格 metadata 解析、按实际读取字节计上限、代理回落与哈希校验配合都到位；短板在发布侧与现场诊断侧，共六处，其中第 1 项已裁定不做、第 4 项经复核判定**本轮不改**（理由见「明确不在本次范围」）。

1. **无 Authenticode 签名。** 信任链 = GitHub Release + TLS。`version.txt`（含 SHA-256，由 `.github/workflows/release.yml:126-132` 生成）与安装包同源：哈希校验挡得住传输/镜像篡改（对 `src/update/mod.rs:60` 的 `ghproxy.cn` 代理回落必要且有效——代理拿不到 `version.txt`，伪造包过不了哈希），但挡不住发布源被攻陷后的同源投放，而下一步是 `ShellExecuteExW("runas")` 提权执行。
2. **CI 原料未校验。** `.github/workflows/release.yml:60-63` 从 `jrsoftware/issrc` 下载安装包后直接执行（只有版本号 pin，无 SHA-256）；`.github/workflows/release.yml:93-94` 从第三方仓库 `kira-96/Inno-Setup-Chinese-Simplified-Translation` 的 `main` 分支裸拉 `ChineseSimplified.isl`，连 commit 都未 pin。两者都直接参与最终产物生产。
3. **互斥量存在性探测权限位过宽。** `wait_main_instance_gone`（`src/update/mod.rs:600`）在 `src/update/mod.rs:606` 用 `MUTEX_ALL_ACCESS` 调 `OpenMutexW`，`:607` 把任何 `Err` 当作「互斥量已消失 = 主进程已退出」直接 `return true`。主/子进程完整性级别不同（提权与非提权并存）时 `ACCESS_DENIED` 会被误判为已退出，导致提前启动安装器并撞上 exe 映像仍被占用——`installer.iss` 的 `AppMutex` 与强杀兜底会介入，但优雅交接已退化。
4. **安装器强杀按映像名匹配（已评估，本轮不改）。** `installer.iss:119-122` 在 admin 权限下用 `Get-CimInstance` 全进程枚举 + `Name -eq 'traffic-monitor.exe'` 逐 PID `taskkill`，排除条件仅「命令行不含 `--check-update`」：同名无关进程会被误杀，伪装同名且带该参数的进程可免死。`installer.iss:107-110` 的注释已声明这是知情取舍。**收窄到「映像路径前缀 = `{app}`」看似更精确，但成本高于收益**，故本笔记不动它，理由见「明确不在本次范围」。
5. **自启路径有损转换。** `src/tray.rs:254-257` 用 `current_exe()?.to_string_lossy()` 拼注册表值，安装目录含非 Unicode 可解码字符时会写出损坏的自启项。`src/util.rs` 已有为路径无损转换准备的 `os_to_wide`（并用例钉住孤立代理项场景），是 `reg_write_string` 的 `&str` 接口把无损 `OsString` 堵死在有损转换上。
6. **release 期零可观测性。** `diag!` 在 release 展开为空（`Cargo.toml:25-26` 的 feature 注释即为此），GUI 无控制台、无日志文件；`embed_in_taskbar` 失败、更新卡住这类现场问题只能靠用户转述弹窗。这是「短板中的短板」，也是性价比最高的改进。

## 提案

本笔记的四项为**一次实施范围**（同一分支内可分组提交，PR 怎么切由实施阶段判断）：

1. **`SYNCHRONIZE` + 错误码分流**（小）：`src/update/mod.rs:606` 改 `SYNCHRONIZE`，只把「互斥量不存在」（`Err` 且 `GetLastError() == ERROR_FILE_NOT_FOUND`）判为已退出；其余错误（含 `ACCESS_DENIED`）按「仍存在」保守处理，继续等或走到超时。`installer.iss` 用 `CheckForMutexes`，无此问题、不改。
2. **自启路径无损写**（小）：`src/util.rs` 增加写入宽字符串的注册表接口（或让 `reg_write_string` 接受 `OsStr`），`src/tray.rs:251-258` 改走 `os_to_wide`；读取侧（`src/tray.rs:247-249`）不变。
3. **CI 原料 pin**（小）：`issrc` 安装包补 SHA-256 校验；`.isl` 二选一——pin 到含 40 位 commit SHA 的 raw URL（最小改动），或把该语言文件 vendored 进仓库（彻底消除对上游 `main` 的依赖，代价是上游更新需手动同步，且需先确认该文件的再分发许可）。
4. **运行时日志开关**（中）：沿用既有 `src/config.rs:21` 的 `REG_PATH_APP` 加 `EnableDebugLog`（DWORD），日志写 `%LOCALAPPDATA%\Traffic Monitor\debug.log` 并环形截断；新增 `util::log_event!`（开关未开时只做一次 `Relaxed` 原子读即返回，release 开销与今天几乎等价），把现有静默点接上：`src/window.rs:308`（周期重嵌入失败）、`src/main.rs:372`（托盘创建失败）、`src/suspend.rs:47` 与 `:62` 的 `let _ = sync_monitoring_timers(...)`、`src/renderer.rs:37-45`（渲染器重入被跳过）、`src/update/mod.rs` 的协议与启动失败路径。`diag!` 保持不变、继续只服务开发期。

## 明确不在本次范围

- 保留 `ghproxy.cn` 明文回落（`src/update/mod.rs:60`）：可达性换隐私的产品取舍，已定性为取舍而非缺陷。
- 锁定句柄重验（`FILE_SHARE_READ_ONLY`，`src/update/mod.rs:75`）与「校验与执行之间无换文件窗口」的设计不动，这是本仓最强的安全设计；日志改造也不得在 `VerifiedInstaller` 持有文件锁的区间内引入对同一文件的额外访问。
- `installer.iss` 的「先礼后兵」顺序（`installer.iss:46-50` 与 `CurStepChanged`）保持：优雅退出等待 → 超时 → 有限次强杀。
- **不把强杀条件收窄为「映像路径 = `{app}`」**（原报告 B4 不采纳）：这是**兜底路径**，收窄它的风险高于它要防的误杀。三条理由——`Win32_Process.ExecutablePath` 对跨会话/受保护进程可能为空，需再加保守分支；8.3 短路径（`C:\PROGRA~1\...`）与 `{app}` 长路径前缀会静默失配；把 `{app}` 嵌进 `cmd /C powershell -Command "..."` 的转义链更长，写错即「兜底静默失效 → 安装器弹文件占用」。而误杀的前提是用户机器上恰有一个同名 exe，概率极低。若确要做，先在同名 exe + 含 `--check-update` 参数的伪装场景实测后再议。
- 更新检查的冷却/退避与常量关系断言不在这里，见 `01-abort-safety-and-invariants.md`。
- 不做遥测：日志只落本地文件，不出网。
- 不做 Authenticode 签名与安装前 `WinVerifyTrust`（原提案第 5 项，已裁定不做：个人项目、无签名证书是常态，且无用户明确要求；信任链维持 GitHub Release + TLS + 同源哈希校验）。

## 为什么不保留？

反方其一：「日志开关与 AGENTS 的 release 零成本取向冲突」——冲突只在「每次判一次」与「事件发生才写」之间：用一次 `Relaxed` 原子读作门，未开启时不产生系统调用与分配；收益是把两类现场问题的诊断成本降一个量级，而更新链路与本地状态机之间的防御深度本就是倒三角，日志是同时补两侧的最低成本手段。反方其二：「CI pin 只是把信任从上游挪到仓库里的哈希常量，还得手动维护」——哈希常量一次写入、每次发布自动生效，改动只发生在升级 pinned 版本时；vendored `.isl` 更是 pin 到内容的极端形式，许可允许时应优先。

## 验收标准

- `grep -n 'MUTEX_ALL_ACCESS' src/update/mod.rs` 无命中；`grep -n 'SYNCHRONIZE' src/update/mod.rs` 有命中；代码里可读出「打不开」与「不存在」的分流（`ERROR_FILE_NOT_FOUND`）。
- `grep -n 'to_string_lossy' src/tray.rs` 无命中。
- `issrc` 下载步骤（当前 `.github/workflows/release.yml:59-63`）内出现原料哈希校验且不匹配即失败退出——注意 `.github/workflows/release.yml:131` 的 `sha256sum` 是产物侧哈希，不算满足本条；`.isl` 来源行含 40 位 commit SHA 或改为本地路径。
- `installer.iss:119-122` 的强杀条件保持原样（映像名 + `--check-update` 排除）——本笔记明确不动兜底匹配。
- `grep -rn 'log_event' src/` 命中 ≥ 5；开关关闭时 release 二进制运行不产生 `debug.log`（人工确认），打开时能复现重嵌入失败留痕。
- `bun scripts/package.ts` 与 `bun scripts/release.ts` 流程不变，CI 的三方版本一致校验保持绿。

## 风险

- 日志引入新失败面（磁盘满、路径权限、`LOCALAPPDATA` 缺失）：实现必须「写失败即静默丢弃并自动关开关」，不得 panic、不得阻塞 UI 线程（用原子量做失败计数与开关复位）。
- `SYNCHRONIZE` 分流若写反（把 `ACCESS_DENIED` 判成已退出）会比现状更糟；必须有显式分支「`Err` 且非 `ERROR_FILE_NOT_FOUND` ⇒ 继续等待」，并在 PR 描述里说明该场景无法在 CI 复现（人工验证：管理员启动主程序 + 普通权限跑 `--check-update`）。
- vendored `.isl` 需确认上游许可允许再分发；不允许则退回 pin commit 方案。

