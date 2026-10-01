# Agent Note：用签名锚点固定安装包信任

Status: rejected（复核裁决：防供应链攻击的假设性威胁，无用户可感知收益且成本高，不实施）

## 问题

当前更新链的信任完全落在**同一个渠道内**：版本元数据（版本号 + 安装包 SHA-256）从 `github.com` 的 `releases/latest/download/version.txt` 抓取（`src/update/mod.rs:59-67` 定义路径、`:253` 抓取），解析成 `{version, hash_hex}`（`src/update/version.rs:46-66`，哈希形状强制 64 位 ASCII hex，见 `:33-35`）；安装包从 `github.com` 直连下载、失败回落第三方镜像 `ghproxy.cn`（`src/update/mod.rs:323-357`，镜像路径在 `:332-352`）；启动安装器前对这个包重算 SHA-256 并与元数据比对（`src/update/installer.rs:204-284` 下载后重验、`:131-149` 锁定句柄重验、`:171-192` 副本交班重验）。

这层摘要确实挡得住镜像：`ghproxy.cn` 只中转字节，拿不到元数据，无法伪造出可安装的包（`README.md:93-99` 已把这条策略写成显式设计）。但它挡不住**发布渠道本身被控**：账号、Release、CI 任一被改写，攻击者可以同时替换安装包与 `version.txt` 里的摘要，两侧一致，校验照样通过；而安装器以管理员身份运行（`src/update/installer.rs:571-587` 用默认动词，提权交给 SetupLdr；`installer.iss:1-23` 未设 `PrivilegesRequired`，取默认 admin）。

全仓检索 `WinVerifyTrust|SignTool|signtool|Authenticode`（`*.rs`、`*.ts`、`*.yml`、`*.iss`、`*.ps1`、`*.toml`）→ **0 命中**；`installer.iss:1-23` 的 `[Setup]` 段没有任何签名相关指令；`.github/workflows/release.yml` 只在 `:129-137` 生成 `version.txt`（`sha256sum` 本地安装包）、在 `:249-287` 回读远端 `version.txt` 与本地哈希比对、在 `:208-247` 上传资产。也就是说：发布侧没有签名步骤，客户端没有验签步骤。

按 AGENTS.md 的不变量「安装包信任必须绑定到最终启动的同一对象身份」，现在满足的是**完整性**（启动的就是校验过的那一份字节），缺的是**来源身份**（这份字节由谁发布）。

## 提案

1. 在发布链给安装包做 Authenticode 签名：`.github/workflows/release.yml` 在 `Compile installer`（`:125-127`）之后、`Generate version.txt`（`:129-137`）之前插入签名步骤（`signtool sign /fd SHA256 /tr <RFC3161 时间戳服务> /td SHA256`），密钥与证书来自受保护的 secret，禁止明文入库。`version.txt` 里的摘要必须是**签名后**文件的摘要（现有步骤顺序天然满足，只要签名插在它之前）。
2. 客户端在启动安装器前对**已锁定句柄**验签：`WinVerifyTrust` 的 `WINTRUST_FILE_INFO.hFile` 接受文件句柄，因此可以复用 `src/update/installer.rs:136` 那个只读共享句柄，把「校验对象 == 启动对象」这条既有的身份绑定延伸到来源身份；校验项是「签名链有效」且「签名者指纹等于编译期常量」。期望指纹放 `src/config.rs`（与 `TRAY_ICON_RESOURCE_ID`（`src/config.rs:218`）同类的具名常量惯例），**不得**从网络取——否则锚点又落回同一渠道。验证策略在此定死：签名链有效 + RFC3161 时间戳（使证书过期后旧签名仍可验证）+ 指纹等于该常量；不引入「忽略过期」的分支。
3. 保留现有 SHA-256 全程校验（`src/update/installer.rs:131-149`、`:204-284`、`:171-192`）不动：它是防镜像篡改的那一层，与签名是两层不同锚点，不是替代关系。
4. 归因要复用现有出口：验签失败按「安装包不可信」处置，与 `src/update/installer.rs:447-458` 的重验失败同款（先 `relaunch_main_app_at`，再 `show_error`），不得静默。

## 明确不在本次范围

- 不改 `version.txt` 的两行格式与严格解析（`src/update/version.rs:46-66`）：改格式必须与 `release.yml:129-137` 的生成端同批，而本项不需要它。
- 不替换 SHA-256 校验、不取消代理回落（`src/update/mod.rs:59-60`、`:332-352`）：镜像策略已在 `README.md:93-99` 披露为显式设计。
- 不改启动动词（`src/update/installer.rs:571-587` 的默认动词）：它服务 `installer.iss:46-59` 的 `runasoriginaluser` 身份语义，是另一条既有裁定。
- 不引入自建更新服务器、CDN 或双签名方案。
- 不改 `compare_versions` 的降级保护（`src/update/version.rs:8-13`）：它挡的是版本回退，与来源身份无关。

## 为什么不保留？

1. 「SHA-256 已经够了。」—— 摘要与产物同源、同一快照，被控的渠道可以同时改两者；摘要提供的是传输完整性，不是发布者身份。
2. 「签名要证书、要钱、要运维，成本高。」—— 承认这是本项唯一实质成本，属产品决策；本节给的是最小落点：一个证书 + 一次 `signtool` + 一次 `WinVerifyTrust` + 一个编译期常量。
3. 「证书会过期/被吊销，验签会把合法更新挡在门外。」—— 这条已在提案第 2 条处理掉：RFC3161 时间戳让旧签名在证书过期后仍可验证，因此不存在「轮换当天挡住合法更新」；残余风险只剩运维纪律（换证书时同步 `src/config.rs` 的期望指纹，见风险第 2 条）。
4. 「验签要加载 wintrust/加密 DLL，违反『主进程不得为更新加载网络/加密依赖』（AGENTS.md 不变量 1）。」—— 验签发生在 `--check-update` / `--update-install` 这条短生命周期边界内（两者都在单例锁之前被拦截，`src/main.rs:361-389`、`:394-409`），不是常驻主进程；但 `/DELAYLOAD` 清单（`build.rs:36-40`）与 `check.yml:37-77` 的 dumpbin 正反断言必须同批更新，否则门禁会红（这属于本项的已知联动面，不是反对理由）。

## 验收标准

- `grep -rn "WinVerifyTrust" src` 命中 ≥1 处，且位于 `--check-update`/`--update-install` 可达路径上（不在常驻主进程启动路径上）。
- `grep -n "signtool\|SignTool" .github/workflows/release.yml` 命中签名步骤；`grep -n "Get-AuthenticodeSignature\|signtool verify" .github/` 命中发布侧自检（可选但建议）。
- **负例（可执行的反例测试）**：把安装包改一个字节并**同步**改写 `version.txt` 的摘要 → 现状会通过（这正是本项要修的口子），签名后必须被拒并走 `relaunch_main_app_at` + 可见提示。
- `check.yml:37-77` 的 delay-import 断言按新的 DLL 面更新后仍为正反双断言（标准导入表不含 winhttp/bcrypt/wintrust，delay 目录含全部）。
- 四条门禁全绿；一次真机完整升级成功（`src/smoke.rs:22-27` 的人工清单）。

## 风险

- 私钥成为新的单点：CI secret 泄漏等于「伪造的合法性」。必须用受保护存储、限制签名 job 的权限，并优先把它与 `09-harden-installer-and-release-chain.md` 的 action 钉 SHA 一起做——否则一个可变引用的 action 就能取走私钥。
- 证书轮换是本项唯一需要运维纪律的地方：验证策略已在提案第 2 条定死（RFC3161 时间戳 + 指纹常量 + `compare_versions`（`src/update/version.rs:8-13`）挡降级），因此不会出现「证书轮换当天挡住合法更新」——时间戳让旧签名在证书过期后仍可验证。残余风险是运维本身：轮换证书时忘记同步 `src/config.rs` 的期望指纹会让更新整体失效，需把「换证书 = 同步常量 + 跑一次负例」写进发版清单。
- 验签失败会新增一条「用户点了是但装不上」的路径；文案与恢复动作必须与 `05-report-install-settlement-failures.md` 的口径一致（不得声称必然恢复成功）。
- 未覆盖：本机无 ISCC、无证书、无真实发布通道，本项全部验证只能在 CI + 真机上做。
