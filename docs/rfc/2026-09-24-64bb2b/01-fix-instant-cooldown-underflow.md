# Agent Note：把更新冷却改成「下次可检查时刻」，消除 Instant 下溢

Status: proposed

## 问题

`src/update/mod.rs:166-182` 在自动检查失败时用「当前时刻往回减 55 分钟」表达「5 分钟后重试」：`*last = Some(Instant::now() - Duration::from_secs(AUTO_CHECK_COOLDOWN_SECS - AUTO_CHECK_ERROR_COOLDOWN_SECS))`（源码里这条表达式跨两行，见 `src/update/mod.rs:174-175`），两个常量是 3600 与 300（`src/config.rs:85-86`），实减 3300 秒。`src/update/mod.rs:164` 的编译期断言只钉住了「错误冷却 ≤ 正常冷却」，钉不住「当前 Instant 距离时钟原点是否够 3300 秒」。

Rust 的 `Instant - Duration` 在结果不可表示时不是饱和而是 panic（`checked_sub` 返回 `None`，`Sub` 实现直接 `expect`）。Windows 上 `Instant` 的取值以 QPC 为原点，而 QPC 从系统启动开始计数：Rust **1.95.0 / 1.96.0** 的 `library/std/src/sys/time/windows.rs` 里 `Instant::now()` 就是 `mul_div_u64(QPC, 1e9, freq)` 的裸开机纳秒；**1.97.0 起**才加上 `instant_nsec + (u64::MAX / 4)` 的偏移，注释写明这是为了「avoid being too close to 0 which would lead to underflow when computing times in the past」，引用 rust-lang/rust#156142。本仓库 `Cargo.toml:5` 声明 `rust-version = "1.95"`，`Cargo.toml:7-12` 的 `[profile.release]` 是 `panic = "abort"`，所以用 1.95/1.96 工具链构建的 **release** 二进制在**开机 55 分钟内**命中这条路径时，整个常驻进程被 abort，而不是只挂掉更新线程。

触发链已逐段核对：自动检查默认开启（`src/update/mod.rs:73-77` 缺省 true）；元数据只从 `github.com` 取、且失败后重试一次仍失败才返回错误（`src/update/mod.rs:209-221`）；错误分支只在自动检查（`!is_manual`）上执行（`src/update/mod.rs:169`）；`INSTALLER` 之外的路径不需要用户交互。开机后自启 + 直连 github.com 失败（国内是常态而非例外）即满足全部条件。

同一类回推在测试里还有一处残留：`src/collector/network.rs:463,470` 的 `test_blacklist_needs_refresh_when_empty_or_stale` 用 `now - Duration::from_secs(BLACKLIST_REFRESH_SECS)`（`src/config.rs:76` 该常量为 30）构造「陈旧」输入，在 1.95/1.96 工具链上开机不足 30 秒跑 `cargo test` 同样会 panic（测试跑 dev profile、`panic = "unwind"`，表现为该用例失败而非进程 abort）。本 note 一并清掉这一处，避免「同一约束只修了一半」。

消费者清点：这条减法只有 1 处生产消费者（`src/update/mod.rs:172-178`）；`LAST_CHECK_TIME` 全仓 `grep -rn "LAST_CHECK_TIME" src/` 命中 7 处——定义 `src/update/mod.rs:71`，读写 3 处（`:120` 冷却门、`:170` 写、`:196` 推迟），另有 2 处注释引用（`src/config.rs:126`、`src/suspend.rs:388`）需要在改名时同步改述；测试侧 0 命中。发布物用 `stable` 构建（`.github/actions/rust-setup/action.yml:9-11`），因此线上 exe 不受影响；受影响的是按声明 MSRV 自建的二进制，而 CI 的 msrv job 只跑 `cargo check`（`.github/workflows/check.yml:53-54`），永远看不到运行时 panic。

## 提案

把状态从「上次检查时刻」改成「下次可检查时刻」：`LAST_CHECK_TIME: Mutex<Option<Instant>>`（`src/update/mod.rs:71`）改为 `NEXT_CHECK_TIME`，语义变为 deadline。

- 正常完成：`*next = Instant::now() + Duration::from_secs(AUTO_CHECK_COOLDOWN_SECS)`。
- 出错时：`*next = Instant::now() + Duration::from_secs(AUTO_CHECK_ERROR_COOLDOWN_SECS)`，直接表达「5 分钟后重试」，不再需要「提前 55 分钟」的技巧。
- 冷却门（`src/update/mod.rs:120-126`）：`if let Some(t) = *next && Instant::now() < t { ... return; }`。
- `defer_initial_auto_check`（`src/update/mod.rs:195-198`）同样写成 `*next = Instant::now() + AUTO_CHECK_COOLDOWN_SECS`。
- 顺带清掉 `src/collector/network.rs:470` 的 `now - Duration`：该用例要的是「一个明显早于 `now` 的时间戳」，改成 `now.checked_sub(...)`，`None` 时直接返回（此时也无法构造陈旧时间戳，用例无意义），不要用会 panic 的写法。

这样 `src/` 内不再存在任何「往回推 `Instant`」的表达式。把「算出 deadline」抽成纯函数 `next_check_deadline(now: Instant, is_error: bool) -> Instant`，测试可直接喂任意 `Instant`。

## 明确不在本次范围

- 不用 `checked_sub` + 兜底值：兜底值取什么没有正确答案（取 `now` 会变成「立即允许重试」，取某个常量又是第二处魔法数），deadline 模型从根上不需要减法。
- 不改两个冷却常量的数值与相对关系，仅改表达方式。
- 不动自动检查的开关、冷却门的调用顺序、以及 `src/main.rs:280-282` 的 `--relaunched-by-update` 推迟逻辑——它们的语义由 deadline 值自然继承。
- 不在本次顺带改成「记录最近 N 次失败」之类的退避策略。
- 不引入「禁止使用 `Instant - Duration`」的 lint 或包装类型：全仓只剩测试里这一处、且已按 `checked_sub` 收口；为此新增类型属于过度设计（残留风险见下）。

## 为什么不保留？

1. 「发布物用 stable 构建，实际用户不受影响」——成立，但 `Cargo.toml:5` 的 `rust-version = "1.95"` 是对外承诺，CI 还专门有 msrv job 与之对账；声明支持的构建组合里存在「一开机就可能自杀」的路径，属于契约层缺陷，不是可以靠「我们发的是 stable 构建」长期豁免的东西。
2. 「开机 55 分钟内 + 检查失败是小概率」——不成立：程序按设计开机自启（`src/tray.rs` 的自启写入），首个自动检查就落在开机后不久；而直连 github.com 失败在国内是常见结果，两者叠加是设计上会发生的场景，不是边角。
3. 「1.97 以后被标准库遮住了，等 MSRV 提到 1.97 就自动消失」——那会把一个 15 行的改动绑死在一次工具链升级上，且 MSRV 提级在本仓库有六处文本要同步（AGENTS.md 已写明口径）；用 deadline 表达与标准库版本完全解耦。
4. 「panic 只发生在更新线程」——不成立：`Cargo.toml:12` 是 `panic = "abort"`，线程 panic 即整进程终止，任务栏挂件会无声消失，用户看到的是「程序自己没了」。注意这只在 release profile 成立，`cargo test` 走 dev profile 是 unwind，所以测试里的同类回推不会 abort，只会让用例变红。

## 验收标准

- **静态判据必须能穿透跨行表达式**：当前写法在 `src/update/mod.rs:174-175` 跨两行，单行 `grep -rn "Instant::now() *-" src/` 在**修复前也是 0 命中**，不构成判据。改用多行语义检索：`Instant::now\(\)\s*-\s*((std|core)::time::)?Duration` 在**修复前命中 1 处**（`src/update/mod.rs:174`，负例成立）、修复后命中 0 处；等效地可用 `Get-Content -Raw` + 多行正则。
  - 两个容易写错的细节（初稿的判据正是这么退化成假门禁的）：操作符与 `Duration` 之间隔着路径限定符 `std::time::`，模式写成 `[-+]?\s*Duration` 连负例都命中不到，于是修复前后都是 0 命中；把 `+` 也纳入判据会误伤合法的 deadline 加法（`src/update/installer.rs:171` 正是 `Instant::now() + Duration`），故只判减法。
  - 该模式只锚定 `Instant::now() -`，覆盖不到「先存进变量再回推」，也覆盖不到减法与 `Duration` 之间插入其它表达式；它是回归护栏，不是全量形式化验证。
- `grep -rn "NEXT_CHECK_TIME" src/` 覆盖定义 1 处 + 读写 3 处；`grep -rn "LAST_CHECK_TIME" src/` 命中 0 处（含 `src/config.rs:126` 与 `src/suspend.rs:388` 两处注释，一并改述为「下次可检查时刻」语义，避免注释与状态名漂移）。
- `src/collector/network.rs` 内不再有会 panic 的 `Instant` 回推；该用例在 `checked_sub` 返回 `None` 时提前返回并留注释说明原因。
- 新增单元测试（放在 `src/update/mod.rs` 的 `#[cfg(test)]`）钉死 `next_check_deadline`：错误情形得 `now + 300s`、正常情形得 `now + 3600s`，且两者都严格晚于 `now`。边界（原点附近的 `now`）**无法**用真实 `Instant` 构造，不要在测试里假装能构造——测试只钉 delta，下溢的消失由「减法不存在」这一结构事实承担。
- `cargo test --locked` 全绿；`cargo build --release --locked` 无警告；`cargo clippy --all-targets --locked -- -D warnings` 与 `cargo fmt -- --check` 无输出（CI `check.yml` 口径）。
- 实机复现（明确前置条件）：用 **1.95 或 1.96** 工具链 `cargo build --release --locked`，在**开机 55 分钟内**触发一次**自动**检查失败（断网或让 github.com 不可达）——必须是自动路径，托盘手动检查传 `is_manual = true`，而 `src/update/mod.rs:169` 的时间戳写入被 `if !is_manual` 排除，走不到待修路径。若无法构造「开机 55 分钟内 + 失败」，本条降级为未验证项并如实记录，不得用「结构上不可能」替代实证。

## 风险

- 语义差别是「把时间戳从 `now - 3300s` 改成 `now + 300s`」，**新旧都是子进程结束后的完成时刻**（`src/update/mod.rs:173-180` 两分支都在 `run_check_subprocess` 返回之后写入），所以不存在「从检查开始时刻改成完成时刻」这种节奏后移；正常路径的冷却窗口仍是完整的 3600 秒，与今天一致。这条当初在草案里写反了，实施者不要按「后移一个检查耗时」去调常量。
- 本次改动无法在本机以回归测试形式证明「1.95 不再 panic」（本机工具链是 1.98.1，标准库 offset 已生效，且没有装 1.95 工具链），只能靠「减法消失」这一结构性事实 + 纯函数测试承重；残留风险是将来有人在别处重新引入 `Instant - Duration`，`src/collector/network.rs:470` 正是活例。缓解：在 `src/collector/rate.rs` 的模块头留一行 `TODO(no-instant-rewind)` 说明该约束——该模块是仓库里唯一成片做 `Instant` 差分的地方，将来在这里动时间运算的人会先看到它（不新增独立笔记、不加 lint）。
- 若把判据写成单行 grep（草案原样），验收会在修复前与修复后都「通过」，形成一条永远为真的假门禁——实施者必须按上面「先跑负例」的方式落地。
