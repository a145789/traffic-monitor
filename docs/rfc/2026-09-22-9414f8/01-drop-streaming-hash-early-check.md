# Agent Note：删除安装包流式哈希早验，锁定句柄重哈希为唯一校验裁决

Status: proposed

## 问题
安装包哈希是否等于远端预期，这一个事实存在两份表示，其中一份被模块自己宣告为非权威。`src/update/http.rs:279` 的 `fetch_to_file` 边下载边哈希并返回流式哈希（签名 `src/update/http.rs:284` 为 `Result<String, FetchFileError>`，哈希构造/更新/收尾在 `src/update/http.rs:289`、`:292`、`:303`），`src/update/installer.rs:121` 取其返回值并由 `src/update/installer.rs:136-143` 做早验比对、失败即删文件；而模块不变量明文写着这份表示不作数——`src/update/installer.rs:3-4`「最终构造 `VerifiedInstaller` 的唯一依据是锁定句柄的重算哈希（`compute_sha256_hex_locked`），不采信流式哈希、不按路径另开文件」，`src/update/installer.rs:83-84` 在函数文档里逐字重复同一句。结果是每次成功下载都要为一份注定不被采信的哈希多算一次整包 SHA-256——它在 `fetch_to_file` 的消费闭包内随下载增量计算（`src/update/http.rs:291-297`），不产生额外文件 I/O，删掉省下的是下载期的哈希 CPU（典型几 MB 包为毫秒级、`INSTALLER_MAX_BYTES` 上限量级为亚秒级）。检索记录：内置 grep `streaming_hash|fetch_to_file|hash\.update|hash\.finish`（`src/`）——`streaming_hash` 3 处命中（`src/update/installer.rs:121` 绑定、`:136` 比对、`:141` 失败文案插值），`fetch_to_file` 定义 1（`src/update/http.rs:279`）、生产调用 1（`src/update/installer.rs:121`），另 `src/update/http.rs:3` 与 `src/update/crypto.rs:29` 为注释提及。

## 提案
把 `fetch_to_file` 的返回类型改为 `Result<(), FetchFileError>`：删除 `src/update/http.rs:288-289` 的 `Sha256` 构造、消费闭包里的 `hash.update`（`src/update/http.rs:292-293`）、`hash.finish`（`src/update/http.rs:303-305`）以及「成功返回已下载内容的十六进制哈希（流式哈希）」的文档段；删除 `src/update/installer.rs:121` 的 `streaming_hash` 绑定与 `src/update/installer.rs:136-143` 的早验块（该块的 drop 写锁/删文件动作并入既有 `Err(FetchFileError::…)` 处理路径，清理顺序保持「先释放锁再删文件」不变）；同步改写 `src/update/crypto.rs:29` 的注释（「流式下载与锁定句柄重验共用同一增量能力」改为「锁定句柄重验是唯一校验路径」）。净删约 20 行（http.rs 约 12 行、installer.rs 约 8 行），并省去下载期一次整包哈希计算（纯 CPU、毫秒级，无 I/O 变化）。同步去掉措辞里的空事实：`src/update/http.rs:3-4` 模块头与 `src/update/installer.rs:3-4`、`:80-84` 的「边读边哈希」「不采信流式哈希」都必须一并删除——流式哈希已不存在，留着就是把不存在的表示继续写成规范。

## 明确不在本次范围
`compute_sha256_hex_locked`（`src/update/crypto.rs:107`）与缓存复用路径的重哈希（`src/update/installer.rs:69`）绝不能一起删——它们是 accept 的唯一裁决，删掉等于无校验安装；`src/update/installer.rs:153-168` 的锁定句柄重验块一字不动，本提案只删它前面的早验，不改它的判据、错误文案与清理动作；`compute_sha256_hex`（cfg(test)）与 `compute_sha256_hex_reader`（`src/update/crypto.rs:89`）保留——前者是三处测试的正确性参照物，后者是「流式一致性」测试的被测面且不依赖 `fetch_to_file` 返回值；`Sha256` 结构及其 BCrypt 句柄守卫不动（AGENTS.md 第 8 条：业务守卫留在业务文件）。

## 为什么不保留？
最强反方是安全纵深：下载期校验与锁定重验互为对照，为省一次哈希 pass 删掉安全关键路径的第二道防线，收益不成比例。逐条回应：(1) accept 语义零变化——构造 `VerifiedInstaller` 的判据仍是 `src/update/installer.rs:153-168` 的锁定句柄重哈希，输入（锁定句柄）与阈值（`expected_hash_hex`）都没动；(2) accept 集合唯一扩大的场景是「下载内容哈希不符、但在降级只读锁的无锁窗口（`src/update/installer.rs:144-151`）内被替换为与预期一致的正确文件」，此时被执行内容仍满足「锁定句柄哈希 = 预期」不变量（锁持有至 `ShellExecuteExW` 返回），安全性不降；(3) 诊断信息损失为零——两处错误文案逐字相同（`src/update/installer.rs:140` 与 `:166` 都是 `安装包校验失败 (预期: {}, 实际: {})`），删早验不丢任何用户可见信息；(4) 次反方「早验能在写锁未释放时就拒绝」——重哈希同样拒绝，差别只是晚一步删文件，清理与错误归类（`FetchFailure::Local`、不回落代理）逐字不变。第二反方：「不采信流式哈希」只约束 accept 依据，并未宣告 reject 侧的早验为冗余。回应：这正是本提案要消除的语义分叉——同一份哈希事实不该有「可用于拒绝、不可用于接受」的双重身份，删掉后 `src/update/installer.rs:3-4` 的不变量表述与实现完全对齐。

## 验收标准
`grep -rn "streaming_hash" src/` 0 命中；`grep -n "Sha256::new" src/update/http.rs` 0 命中；`grep -n "Result<(), FetchFileError>" src/update/http.rs` 至少 1 命中（`fetch_to_file` 新签名）；`cargo test --locked` 全绿，行为一致性点名 `test_cached_reuse_accepts_matching_locked_content` 与 `test_cached_reuse_rejects_tampered_locked_content`（`src/update/installer.rs` 的两个 TOCTOU 用例，期望哈希均由 `compute_sha256_hex_locked` 构造，不依赖流式哈希）；`cargo build --release --locked` 与 `cargo clippy --all-targets --locked -- -D warnings` 无任何警告。

## 风险
残留风险一：坏包的拒绝点从写锁阶段后移到只读锁重验，被污染的安装包在磁盘上多存活几十毫秒——可证伪依据是 accept 判据逐字未动（`src/update/installer.rs:153-168`），故不构成安全降级，只是拒绝时点后移。残留风险二：未来若有人想基于 `fetch_to_file` 返回值做新判定（例如「下载期哈希不符则换源」），需要重新引入哈希——缓解是同步改写 `src/update/http.rs:275-278` 的文档明示无返回值。残留风险三：早验块消失后失败路径的锁释放顺序要逐条核对（现 `Err` 分支均为「先 drop 写锁再删文件」，`src/update/installer.rs:126-134`），改写时不得把删文件挪到锁释放之前，否则 Windows 下删除被占用文件失败而残留。
