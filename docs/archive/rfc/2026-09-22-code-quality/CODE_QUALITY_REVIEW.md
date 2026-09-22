# Traffic Monitor 代码质量深度审查报告

> 审查范围：全部 17 个 Rust 源文件（约 5,400 行）、`build.rs`、`Cargo.toml`、`installer.iss`、
> `scripts/*.ts`、`.github/workflows/*`。逐文件精读，非抽样。
> 审查维度：潜在 bug、安全风险、工程架构、代码质量、Clean Code、独立观察。
> 结论先行：**这不是"低级程序员"代码库**。真正的风险不在代码腐烂，而在
> （a）`panic=abort` 语境下的鲁棒性裸奔、（b）发布链路无代码签名、（c）release 版零可观测性。

---

## 0. 总评

| 维度 | 评级 | 一句话 |
| :--- | :--- | :--- |
| 潜在 bug | 良 | 无致命缺陷；集中在"异常路径算术/清理不对称"，均有低概率触发条件 |
| 安全 | 良+ | 更新链路 TOCTOU 防护罕见地严谨；短板是无 Authenticode 签名与 CI 供应链 |
| 架构 | 优- | "纯函数核 + 薄 FFI 壳"贯彻得好；两个文件（main.rs / update/mod.rs）有上帝文件倾向 |
| 代码质量 | 优- | 全局状态散布 8+ 处是最大隐性负债；其余纪律（常量、unsafe、无 unwrap）执行到位 |
| Clean Code | 优 | 类型即文档、测试即规格、注释讲"为什么"，密度高于绝大多数同规模项目 |
| 可维护性 | 良 | 高约束文档 + 高注释密度是资产也是门槛；已出现文档与实现的轻微漂移 |

---

## 1. 潜在 Bug（按严重度排序）

### A1（中）CPU 使用率计算存在无保护减法，异常计数器下回绕
`src/collector/cpu_mem.rs:87`

```rust
let usage = ((total - idle_diff) * 100 / total).min(100) as u32;
```

`idle_diff` / `kernel_diff` / `user_diff` 各自 `saturating_sub`（第 71–73 行），但它们是**独立**饱和的：
若计数器出现非单调（休眠恢复异常、API 层复位），可构造出 `idle_diff > total`（= `kernel_diff + user_diff`）。
此时 `total - idle_diff` 在 release（overflow-checks 关闭、`panic=abort`）下 u64 回绕成天文数字，
`* 100` 再回绕，`.min(100)` 只能截上界、救不了回绕后的任意值——显示值可长期错乱；
在 debug 下则直接 panic → `panic=abort` 整个进程中止。
`.min(100)` 的存在说明作者已考虑过"结果超界"，唯独漏了下界。
**修法**：`total.saturating_sub(idle_diff) * 100 / total`，一行。

### A2（中）启动失败路径的资源清理不对称：幽灵托盘图标
`src/main.rs:255-258` ← `src/main.rs:369-394`

`bind_display_and_timers` 的内部顺序是"先建托盘图标，后建监测定时器"。若 `sync_monitoring_timers`
返回 false，`main()` 弹框后直接 `return`：托盘图标既未 `remove_tray_icon`，渲染器也未 `take_renderer()`。
结果是任务栏残留一个悬空图标（悬停/点击才消失），GDI 对象随进程退出才归还。
触发概率低（`SetTimer` 极少失败），但这是"退出序列只有一份实现"（`begin_exit` 注释）原则的反例。
**修法**：该早退分支补 `remove_tray_icon(); renderer::take_renderer();`。

### A3（低-中）退出幂等门只覆盖三分之一入口，不变量已名不副实
`src/main.rs:505-533`、`src/main.rs:737-740`、`src/update/mod.rs:806-816`

`claim_exit_request` 的文档写明"退出序列（托盘清理 + `PostQuitMessage`）只应执行一次"，
但三个入口中只有 `route_exit_request`（`--quit`）过门：
- `WM_CLOSE`（托盘"退出"）→ `begin_exit()` 直接执行；
- `WM_USER_UPDATE_ACTION` → `handle_update_action()` 直接执行。

与 `--quit` 并发到达时 `remove_tray_icon` + `PostQuitMessage` 会执行两遍。两者本身幂等、无实际危害，
但"只执行一次"这条不变量在文档与实现之间已经不成立——这正是后续改动最容易踩的那类坑。
**修法**：`begin_exit` 开头自取 `claim_exit_request(&EXIT_REQUESTED)`；`handle_update_action` 收敛为
"复位 UPDATE_IN_PROGRESS + `begin_exit()`"（顺带消掉第 809–815 行与 `begin_exit` 的尾段重复）。

### A4（低）展示值快照可撕裂，极端时序下漏画一帧
`src/renderer.rs:89-98`、`src/renderer.rs:55-64`

`DisplayValues::load()` 依次读 4 个 Relaxed 原子，非同一时刻快照：可能取到"网速来自 tick N、
CPU 来自 tick N+1"的组合。`invalidate_if_values_changed` 用它与 `LAST_RENDERED_VALUES` 比较、
`render` 又把该组合记为"已渲染"——极端时序下一次真实变化会被误判"无变化"而漏画（下次变化自愈）。
单写者单读者、数值类展示，影响极小；但 `state.rs` 既然有成文的内存序约定，
读侧的撕裂语义也应写进注释，否则后人可能基于"快照一致"做更强的去重逻辑。

### A5（低）更新冷却的算术依赖一条未钉死的常量关系，违反即进程中止
`src/update/mod.rs:198-207`

```rust
*last = Some(Instant::now()
    - std::time::Duration::from_secs(AUTO_CHECK_COOLDOWN_SECS - AUTO_CHECK_ERROR_COOLDOWN_SECS));
```

若日后把 `AUTO_CHECK_ERROR_COOLDOWN_SECS` 调到大于 `AUTO_CHECK_COOLDOWN_SECS`，u64 下溢
（release 回绕成约 584 年的 Duration），随后 `Instant - Duration` 直接 panic → `panic=abort` 中止进程。
本仓已有把"常量关系"钉成测试的先例（`suspend.rs` 的
`auto_update_poll_interval_must_be_far_below_cooldown`），此处却没有。
**修法**：加 `const _: () = assert!(AUTO_CHECK_ERROR_COOLDOWN_SECS <= AUTO_CHECK_COOLDOWN_SECS);`。

### A6（低）子进程协议用 `read_line(String)`：UTF-8 敏感 + 无界行缓冲
`src/update/mod.rs:737-777`

非 UTF-8 行会让 `read_line` 报错 → `read_failed = true` 并 `break`，其后的 `EXIT_MAIN` **永远读不到**
（`test_scan_invalid_utf8_marks_read_failed` 反而把这个行为钉成了规格）；同时无上限行缓冲，
异常子进程可让 worker 线程内存无界增长。子进程是自身、风险低，但作为"进程间协议"，
`read_until(b'\n')` + 行长上限 + 字节级比较更稳，也顺带消掉 UTF-8 依赖。

### A7（低）`SetParent` 的"NULL 既是前值也是失败"歧义未处理，与同函数内自己的标准不一致
`src/window.rs:222-234`

同一段代码里，`SetWindowLongPtrW` 精心做了 `SetLastError(0)` + 事后判别（注释原话："0 既可能表示
'前值就是 0'也可能表示失败"），而紧邻的 `SetParent` 只做 `map_err`——Win32 对 `SetParent` 的返回值
有完全相同的歧义（成功且此前无父窗口与失败都可能返回 NULL），windows-rs 0.62 更是把 NULL 一律
映射成 `Err`（`(!result__.is_invalid()).then_some(...)`）。

**实测记录（本机 Win11，2026 年审查期间）**：用 P/Invoke 创建两个无父顶层窗口后首次 `SetParent`，
返回值是 **65548 = `GetDesktopWindow()`** 而非 NULL（`GetParent` 才是 0）。即当前平台上该路径无恙、
不会误报失败；但结论成立的原因是"本版 OS 把前父返回为桌面句柄"这一实现细节，而非代码自身防御。
连同 `GetWindowRect`（`suspend.rs:258`）失败静默后按全零矩形判"非全屏"（落在安全侧）一并记录，
供后续按同一标准收口。

### A8（极低）死代码 / 观感
- `src/suspend.rs:318`：`is_immersive_color_set` 循环后的 `return true` 不可达（`expected` 含尾 NUL，
  必在循环内返回），是对"匹配即真"的冗余兜底。
- `src/renderer.rs:310-326`：速率在 `1023 B/s` 与 `1.0 KB/s` 之间显示粒度突变（四位有效数字跳成两位），
  纯观感，非错误。
- `src/window.rs:326-364`：`embed_in_taskbar` 成功后不写 `LAST_RECT` 缓存，随后一个 tick 会做一次
  多余的 `SetWindowPos`（无害冗余）；若日后有人给两边写入不同的舍入路径，这里就是错位点。

---

## 2. 安全风险

### B1（中）更新链路无 Authenticode 签名验证
信任链 = GitHub Release + TLS。`version.txt`（含 SHA-256）与安装包同源，哈希校验防的是
**传输/镜像篡改**——这对 `ghproxy.cn` 代理回落是必要且有效的防线（代理拿不到 `version.txt`，
伪造安装包过不了哈希）；但它防不住**发布源本身被攻陷**（GitHub 账号/CI 被接管后，
攻击者同时投放安装包与哈希），而下一步就是 `ShellExecuteExW("runas")` UAC 提权执行。
**建议**：安装包加代码签名，启动前 `WinVerifyTrust` 验签；哈希校验保留（它挡的是另一类威胁）。

### B2（中）CI 供应链：发版产物的"原材料"未校验
`.github/workflows/release.yml:57-94`
- Inno Setup 安装包从 `jrsoftware/issrc` 下载后**直接执行**，无 SHA-256 校验（仅版本 pin）；
- `ChineseSimplified.isl` 从第三方仓库 `kira-96/Inno-Setup-Chinese-Simplified-Translation` 的
  `main` 分支裸拉，连 commit 都未 pin。

二者都直接参与最终安装包的生产。**建议**：两处补哈希校验 / pin 到 commit SHA。

### B3（低）`wait_main_instance_gone` 用 `MUTEX_ALL_ACCESS` 做存在性探测
`src/update/mod.rs:606`

权限位过宽：当主进程与子进程完整性级别不同（如提权/非提权并存）时，`OpenMutexW` 可能以
`ACCESS_DENIED` 返回 `Err`，被误判为"互斥量已消失 = 主进程已退出"→ 提前启动安装器，
恰好撞上 exe 映像仍被占用（安装器内 `AppMutex`/taskkill 兜底会介入，但优雅交接已退化）。
**修法**：改用 `SYNCHRONIZE`，"打不开"与"不存在"可用 `GetLastError() == ERROR_FILE_NOT 区分`。
（`installer.iss` 用 `CheckForMutexes` 无此问题。）

### B4（低）安装器兜底强杀按映像名匹配
`installer.iss:113-130`

admin 权限下 `Get-CimInstance` 全进程枚举 + 按 `Name -eq 'traffic-monitor.exe'` 逐 PID `taskkill`，
排除条件是"命令行不含 `--check-update`"：同名无关进程会被误杀，而伪装成同名 + 带该参数的进程可免死。
代码注释已声明这是知情取舍；可再收窄为"映像路径前缀 = {app}"。

### B5（低）开机自启路径经 `to_string_lossy` 有损写入
`src/tray.rs:254-257`

项目为路径无损转换专门写了 `util::os_to_wide`（并有用例钉住孤立代理项场景），但
`reg_write_string(&str)` 的接口把 `current_exe()` 的无损 `OsString` 堵死在有损转换上：
含非 Unicode 可解码字符的安装路径会写出损坏的自启项。接口小改（注册表写宽字符串）即可对齐。

### B6（信息）隐私与攻击面
- `ghproxy.cn` 代理回落可见用户 IP 与下载行为（网络可达性换隐私，属产品取舍）；
- 无遥测、注册表仅写 HKCU、进程无常驻网络监听——攻击面控制得很好；
- `/DELAYLOAD` + 子进程隔离让 winhttp/bcrypt/schannel/IME/Shell DLL 不进常驻进程
  （既降内存也降 DLL 劫持面）——这是全项目最好的安全设计。

### 安全正面清单（值得保持）
1. **TOCTOU 防护完整**：`create_new(true)` + `FILE_SHARE_READ` 只读共享锁 + "锁定句柄上重算哈希"
   （`try_reuse_cached_installer` / `fetch_verified_installer`）+ 失败路径"先释锁再删文件"。
   校验与执行之间无换文件窗口，锁还挡住并发写/删。
2. **严格 metadata 解析**（`version.rs`）：版本号限定纯数字 `major.minor.patch`，顺带消灭了
   URL/文件名注入面（`asset_path` 由它拼接）；哈希限定 64 位十六进制。
3. **上限以实际读取字节计**，不信 `Content-Length`（`http.rs::for_each_chunk`）。
4. 生产路径无 `unwrap`/`expect`（grep 全量核对：仅 mutex lock 与 `#[cfg(test)]`，符合 AGENTS 豁免条款）。

---

## 3. 工程架构

### 3.1 做得好的
- **"纯函数核 + 薄 FFI 壳"**是一致的架构方言：`rate.rs`、`update/version.rs`、
  `suspend.rs::timer_plan`、`should_rebuild_baseline`、`should_reset_update_progress` 全部
  无 I/O、可单测，且测试真的钉在这些核上。5.4k 行能做到 17 文件各司其职、
  `collector/mod.rs` / `ffi_guard.rs` 的归属规则成文并被遵守，分层是干净的。
- **依赖面极小**：`windows` + `windows-registry` + `winresource`（构建期）。对一个自动更新 +
  UAC 提权的常驻程序，依赖少 = 供应链风险少，这是最划算的架构决策。
- **状态模型有文档**：`state.rs` 模块头的内存序约定、`SuspendReasons` 把位协议封装在属主处、
  `AtomicHwnd` 的 `load`/`take`/`clear` 语义区分（查询误用 `take` 会丢句柄，故类型分层）。
- **CI 与本地门禁同构**（`--locked` 纪律、`clippy --all-targets -D warnings`、三方版本一致校验），
  `release.yml` 的 tag/Cargo.toml/installer.iss 三方一致性验证是业余项目少见的发布工程。

### 3.2 问题与风险
1. **全局状态散布 8+ 处**，且形态不一：`state.rs` 静态原子、`renderer` 的 `RENDERER`/
   `LAST_RENDERED_VALUES`、`tray` 的 `TRAY_DATA`、`network` 的三个 map、`cpu_mem` 的 `CPU_BASELINE`、
   `window::update_taskbar_position` 里**函数体内**的 `LAST_RECT`、`main` 的 `REBUILD_RETRY_INTERVAL_MS`。
   当前"单例 + 单 UI 线程"模型下成立，但 `LAST_RECT` 这类藏在函数体里的隐式状态
   是后人重构（多窗口、集成测试、状态导出）时的第一批暗礁。
2. **两个上帝文件**：`src/main.rs`（839 行：CLI + 启动编排 + 消息路由 + 重建/退出编排 + 测试）、
   `src/update/mod.rs`（1,157 行：编排 + 子进程协议 + 安装器启动 + 缓存 + 注册表）。
   后者内部已有 `FetchFailure`/`ScanOutcome`/`VerifiedInstaller` 等天然切面，
   拆成 `protocol.rs` / `installer.rs` / `cache.rs` 是低风险重构。
3. **文档与实现已开始漂移**：AGENTS 第 3 条说"选取当前流量最大的一张单一物理网卡**锁定**并展示"，
   实现（`rate.rs::select_winner_interface`）是**每 tick 重新择大**、无粘滞。双网卡同时有流量
   （Windows 11 自动切换/链路聚合）时显示会逐秒跳变。要么改文档，要么加滞回（见 §6.6）。
4. **CI 缺 MSRV 验证**：`Cargo.toml` 声明 `rust-version = 1.85`，CI 用最新 stable，
   两者从不对账；也无 fuzz/property test（`parse_update_metadata`、`parse_version` 是理想标的）。

---

## 4. 代码质量：有没有"低级代码"、哪些降低维护性

### 4.1 "低级程序员代码"排查结论：基本没有，残留 5 处小动作
逐项核对了常见低级特征（panic 滥用、裸指针乱飞、吞错误、复制粘贴、魔法数、死代码）：

| 项 | 结论 |
| :--- | :--- |
| `unwrap`/`expect`/`panic!` | 生产路径 0 处（仅 mutex lock 与测试），grep 全量核对 |
| 吞错误 `let _ =` | 有，但每处附带"为何可忽略"注释或 `diag!` 埋点，属纪律内 |
| 魔法数 | 有残留（见下） |
| 复制粘贴 | 有 4 处轻度（见下） |
| 死代码 | 仅 `suspend.rs:318` 一处不可达返回 |

残留的具体问题：

1. **`src/renderer.rs:582-587`**：`lfWeight: 400`、`FONT_QUALITY(3)`（应写具名常量
   `NONANTIALIASED_QUALITY`）、字面量 `"Segoe UI"`——与 AGENTS"具体常量统一进 config.rs"
   的自我约束直接冲突，且 `FONT_QUALITY(3)` 这种"裸数字塞 newtype"恰是最容易被改错的写法。
2. **`src/tray.rs:41-46`**：`PCWSTR(1 as *const u16)` + `#[allow(clippy::manual_dangling_ptr)]`——
   全仓唯一一处"压 lint 换通过"的写法。MAKEINTRESOURCE 惯用法本身无错，但
   `1 as *const u16` 应至少与 `build.rs`/资源脚本里的资源 ID 建立命名常量关联。
3. **`src/update/crypto.rs:89`**：`[0u8; 8192]`、`src/main.rs:136-137`：`0..50` × `100ms`、
   `renderer.rs:280` `with_capacity(32)`、`network.rs:27-28` `with_capacity(16)`——
   均为未命名的内联数值，AGENTS 规则 9 的轻微漂移。
4. **`src/tray.rs:89-98`**：`remove_tray_icon` 删除图标后不清 `TRAY_DATA`，状态与事实脱钩，
   靠"重复 `NIM_DELETE` 无害"兜底。顺带导致 `create_tray_icon` 失败时 `TRAY_DATA` 留旧句柄。
5. **`src/update/mod.rs:806-816` vs `src/main.rs:505-512`**：`handle_update_action` 的尾段
   （`remove_tray_icon` + `PostQuitMessage`）与 `begin_exit` 逐行重复——AGENTS 自己写着
   "避免退出语义在两处漂移"，实际是三处。

### 4.2 降低维护性的写法（修一行要读半天的）
1. **`src/collector/network.rs:259-272`**：`blacklist_needs_refresh(&cell.borrow(), now)` 把
   `RefCell` 借用放进 `if` 条件、块内再 `borrow_mut()`。这依赖"if 条件表达式是独立临时作用域、
   条件求值后即 drop"这一**微妙作用域规则**——今天正确，但在 `panic=abort` 的进程里，
   任何"顺手重构"（比如把条件提成 `let needs = ...`）都会把潜在双重借用从"注释约束"变成
   **进程直接中止**。同仓 `renderer::with_renderer` 特意用 `try_borrow_mut` 防重入，
   两处谨慎程度不一致。建议显式 `let stale = ...; drop(borrow)`。
2. **`src/renderer.rs:37-45`**：`with_renderer` 对重入"静默跳过"——吞掉调用方意图，
   只能靠注释禁止闭包内再入。这类"错误被吸收"的 API 在出问题时最难查。
3. **release 版零可观测性**：`diag!` 在 release 展开为空，GUI 无控制台，无日志文件。
   现场问题（嵌入失败、更新卡住）只能靠用户转述弹窗。见 §6.5。
4. **测试触碰进程级全局**：`main.rs:832`（`CURRENT_MAIN_HWND.store_raw`）与
   `update/mod.rs:1082`（`UPDATE_IN_PROGRESS`）。注释已自知并解释"只留一处"，
   但 `cargo test` 并行执行下仍是地雷（thread_local 类测试无此问题，因为 harness 每测一线程）。
5. **跨文件重复 ×4**：ISCC 查找逻辑在 `release.yml` 重复 3 段、`scripts/package.ts` 第 4 段；
   "定长宽缓冲截断拷贝"在 `tray.rs:63-65` 与 `renderer.rs:588-590` 各一份；
   `AtomicHwnd`/`AtomicPowerNotify` 同构双份（已注释"同构单列"，再来第三个就该泛型化）；
   5 秒退出等待在 `main.rs`/`update/mod.rs`/`installer.iss` 三处各有常量（iss 已注明同量级）。
6. **`scripts/package.ts:25`**：`D:\soft\Inno Setup 7\ISCC.exe`——个人机器路径硬编码进共享仓库
   （作为 fallback 列表首位）。

---

## 5. Clean Code 亮点（值得作为团队标准的）

1. **类型即文档**：`ScanOutcome` 用具名结构体替代 `(Option<UpdateAction>, bool, bool, bool)` 四元组，
   注释写明动机（"三个 bool 在位置上无法区分，调用点只能靠顺序记忆"）；`VerifiedInstaller`
   把"文件锁"做成构造前提——**不持锁就无法构造出这个类型**，TOCTOU 不变量由类型系统守卫。
2. **测试即规格，且钉的是历史 bug**：`rate.rs:112-125` 的"万兆网 × 15s 退避先截后除"回归用例、
   `renderer.rs:642-649` 的 KB→MB 闸门边界 1,048,575、`network.rs:344-382` 的"单命中关键字"矩阵
   （并主动注明 `isatap ⊃ tap` 无法单命中的例外）——每条断言都写清"改哪种实现会红"。
3. **边沿语义抽成纯函数 + 状态机矩阵测试**：`should_rebuild_baseline`（暂停位集的恢复边沿）、
   `should_reset_update_progress`（2×2 矩阵）、`timer_plan`（三态定时器集合）。
4. **RAII 归属规则成文且被遵守**：`ffi_guard.rs` 只收通用句柄守卫，业务守卫（`ScreenDcGuard`/
   `OwnedGdi<T>`/`WinHttpHandles`/`BcryptHandles`/`MibTable`）留在业务文件——避免"为了集中而割裂上下文"。
   `Renderer::new` 的事务式构造（任一步失败局部守卫自动清理，成功才 `into_raw` 移交）是标准答案。
5. **性能纪律贯穿**：渲染热路径零分配（`buf` 复用 + 手写 `write_u32`）、
   `invalidate_if_values_changed` 消掉空转重绘、`SetCoalescableTimer` 省电、
   流式下载"边读边哈希边写"整包不进内存。
6. **注释讲"为什么不"**：`NIF_SHOWTIP` 与 v4 的相依关系（`tray.rs:52-55`）、
   `max(1)` 是零除兜底（`rate.rs:58`）、错误冷却用"时间戳前移"表达（`update/mod.rs:200-206`）、
   `is_valid_interface` 故意不查 `HardwareInterface` 的 Hyper-V/WSL2 反例（`network.rs:155-159`）——
   这些都是"未来维护者会想改错"的位置，注释恰好钉在那里。
7. **失败路径的对称性思维**：嵌入序列失败全程不置 `EMBEDDED`、DPI 失败把窗口回滚到位图尺寸、
   黑名单刷新失败保留旧表并重置计时器、`EXIT_MAIN` 转发失败必须复位 `UPDATE_IN_PROGRESS`——
   每一处都显式回答了"失败之后世界是什么状态"。

---

## 6. 独特看法

1. **"单点真值"才是这个项目真正的架构风格**。`EMBEDDED`（嵌入序列成功与否）、`EXIT_REQUESTED`、
   `UPDATE_IN_PROGRESS`、`LAST_CHECK_TIME`、`SuspendReasons`、`LAST_RENDERED_VALUES`——
   每个跨消息/跨线程的事实都收口到一个可指认的真值源，且注释解释了**为什么它必须唯一**。
   这比任何分层图都更能解释代码为什么长成这样。建议把这条模式显式写进 AGENTS 作为第 10 条：
   "新增跨消息事实时，必须指认唯一真值源并写明竞争处理"。它也是 A3 问题的判据来源。

2. **防御深度是"倒三角"，方向正确但形状危险**。面对网络不可信输入 + UAC 提权的更新链路做到了
   军工级（锁柄重验、严格解析、字节上限、双源回落、进程隔离）；而本地 UI 状态机
   （托盘/定时器/窗口重建/退出）靠注释与人肉纪律。按"被谁攻击"排优先级这是对的，
   但按"哪里会出 bug"排，§1 的 A 类问题几乎全部落在纪律侧。建议把给更新链路的严谨度
   分 20% 给退出/重建状态机。

3. **`panic = "abort"` 是被系统性低估的设计决策**。它把 RefCell 双重借用、整数下溢、
   `Instant` 溢出这些"可恢复错误"统统变成**进程死亡**——对一个没有任何兜底 UI 的任务栏小组件，
   进程死亡 = 指标无声消失。在这个前提下，所有"理论上不会发生"的算术与借用都值得一行
   `saturating_`/`try_` 包裹：A1、A5 就是按这个标准挑出来的，A7 是同一标准下的防御缺口记录。
   反过来说，`with_renderer` 用 `try_borrow_mut` 避免 abort 的那个决定非常清醒——
   希望它是全仓的统一姿势而不只是单点。

4. **AGENTS.md 是资产，但已到"读全文件才能改一行"的门槛，且开始漂移**。9 条大约束 + 隐式约束
   （连"重试挂哪条 timer"都写死）的密度极其罕见，是这个项目改错率低的主因。但：约束越多，
   与代码同步的成本越高（"锁定单一网卡"vs 每 tick 择大已经漂了）。本仓已有 3 个
   "文档约束 → CI 测试"的成功转化（`auto_update_poll_interval_must_be_far_below_cooldown`、
   `quit_coexists_with_check_update`、DPI 全范围对账），建议成惯例：**每条 AGENTS 约束要么配一个
   会红的测试，要么承认它只能靠 review**。约束的可信度取决于它被执行的方式。

5. **可观测性是短板中的短板，性价比最高的改进**。一个 20 行的 `%LOCALAPPDATA%\Traffic Monitor\
   debug.log` 环形日志 + 注册表开关（沿用已有 `REG_PATH_APP`），就能把"嵌入失败""更新卡住"
   这两个最易出现场问题的子系统诊断成本降一个量级。现在 `diag!` 的 release 空展开设计是对的
   （零成本），但它把"现场排障"整个砍掉了——这两件事可以用"注册表开关 + 条件编译"兼得。

6. **单网卡选择缺滞回，"择大"≠"锁定"**。以太网 + Wi-Fi 同时活跃时（Windows 11 的
   自动链路切换、手机共享、双网并存很常见），显示会逐秒在两块卡之间跳。建议赢家粘滞：
   挑战者连续 N 个 tick 总流量更高才换卡——10 行代码进 `rate.rs`，配一个纯函数测试，
   同时把 AGENTS 第 3 条的措辞与实现对齐。

7. **测试注释密度比生产代码还高，这是"反常的好"，但有成本**。作者把测试当规格写
   （每条用例说明"钉死什么不变量、改哪种实现会红"），审查时省了大量推理；
   代价是改一行格式化函数要读五段注释。结论：保持，但提醒注释别长过断言本身——
   规格的最小单元应当是断言。

---

## 7. 修复优先级建议

| 优先级 | 项 | 成本 |
| :--- | :--- | :--- |
| P1 | A1 CPU 计算 `saturating_sub`；A5 加 const assert | 各 1 行 |
| P1 | B2 CI 原材料哈希校验 / pin commit | 小 |
| P2 | A3 退出幂等门收敛 + 消重复；A2 启动失败清理补全 | 小 |
| P2 | B1 代码签名 + `WinVerifyTrust`；B3 `SYNCHRONIZE` | 中 |
| P2 | §6.5 release 日志开关 | 小 |
| P3 | §6.6 网卡赢家滞回；A4 撕裂读注释；A6 协议改字节读 | 小 |
| P3 | §4.1 魔法数归位 config.rs；`remove_tray_icon` 清状态；`FONT_QUALITY` 具名 | 小 |
| P3 | update/mod.rs 拆分；MSRV 进 CI；`parse_version` 加 fuzz | 中 |

—— 完 ——
