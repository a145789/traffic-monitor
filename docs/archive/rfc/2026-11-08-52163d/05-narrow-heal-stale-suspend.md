# Agent Note：收窄 heal_stale_suspend 返回值并拆除 ReasonProbe 判定矩阵的仪式层

Status: implemented

## 问题

同一函数上两处冗余。其一（无读者的位集返回）：`src/suspend.rs:256` 的 `heal_stale_suspend(hwnd) -> u32` 累积三个原因的 healed 位（`src/suspend.rs:263`、`:271`、`:284`），但检索 `grep -rn "heal_stale_suspend" src/` 命中 6 处：模块文档（`src/suspend.rs:8`）、函数文档（`:250`/`:255`）、定义（`:256`）、import（`src/recovery.rs:27`）、唯一调用点（`src/recovery.rs:209`）——调用点原文 `if heal_stale_suspend(hwnd) & SUSPEND_REASON_MONITOR != 0`，只读 MONITOR 一位；SYSTEM/SESSION 位的累积没有任何生产或测试读者（`grep -rn "healed" src/` 命中全在函数自身）。其二（判定矩阵的仪式层）：`src/suspend.rs:163`-171 的 `ReasonProbe` 枚举与 `:177`-186 的 `should_resume_for_reason` 构成 reason×probe 矩阵，检索 `grep -rn "ReasonProbe\|should_resume_for_reason" src/` 命中全部在 suspend.rs（生产 7 处 + 测试 17 处），模块外零命中；唯一调用方对每个原因严格配对构造探针（SYSTEM→RunningTick `:260`、SESSION→Session `:268`、MONITOR→Monitor `:275`-279），因此 `(SUSPEND_REASON_SYSTEM, _) => true`（`:179`）使 `:260` 的整个合取项恒真（RunningTick 是零数据变体，纯为凑签名而构造），`_ => false`（`:184`）的错配分支没有任何生产者，`Session { unlocked } => unlocked`（`:180`）是布尔透传。

消费者分类：

- 生产消费者：`heal_stale_suspend` 唯一调用 `src/recovery.rs:209`（只读 MONITOR 位）；`should_resume_for_reason` 唯一调用方是同文件 `heal_stale_suspend`；`SUSPEND_REASON_MONITOR` 在 recovery.rs 的 import（`src/recovery.rs:26`）唯一使用点即 `:209`。
- 非生产消费者：`test_should_resume_for_reason_matches_evidence_strength`（`src/suspend.rs:729`-770）——其中 SYSTEM+RunningTick 断言（`:732`-735）钉的是恒真臂，错配断言（`:762`-769）钉的是不可达组合；SESSION 透传（`:738`-745）与 MONITOR TTL 边界（`:748`-759，TTL-1 不清 / TTL 清）钉的是真实判定。

## 提案

两个动作同函数落地。动作 A：签名改 `-> bool`（语义「本轮是否按 TTL 清掉了 MONITOR 位」），删 `let mut healed` 与三处位累积，MONITOR 分支清位后返回 true；`src/recovery.rs:209` 改 `if heal_stale_suspend(hwnd) {`，`src/recovery.rs:26` 删除 `SUSPEND_REASON_MONITOR` import。动作 B：删除 `ReasonProbe` 枚举与矩阵函数，三个分支直接写判据——SYSTEM：`is_set` 即清（恒真合取项消失）；SESSION：`probe_session_unlocked() == Some(true)`；MONITOR：保留纯函数 `fn monitor_ttl_reached(elapsed_secs: u64) -> bool { elapsed_secs >= SUSPEND_MONITOR_TTL_SECS }`。SESSION 判据同样保留一个纯函数 `fn session_unlocked_confirmed(probe: Option<bool>) -> bool { probe == Some(true) }`——它承载「探针失败（None）保持位不变」的受保护方向（`src/suspend.rs:204`-205 文档），不是纯 std 包装。测试改为两个真值表：session 的 None/false/true 三态（比旧透传测试更强，旧测试无法表达 None）、monitor 的 TTL-1/TTL 边界（原样保留）；删除 SYSTEM 恒真与错配断言。三原因判定强度差异的文档（`src/suspend.rs:247`-253）原样保留，探针强度理据（`:164`-170）移入分支注释。预估净删约 45 行。

## 明确不在本次范围

- `resume_system`/`SuspendReasons` 位协议与 paused→running 边沿基线重建不动：本提案只改「是否清位」的判定包装，清位仍唯一经 `resume_system`（`src/suspend.rs:175`-176 文档明确绕过它会漏掉基线重建与网络退避复位）。
- `monitor_suspended_secs` 的 None 兜底（`src/suspend.rs:190`-199）不动：「晚清而非永不清」是刻意保守收敛，任何替代实现仍须为 None 定义行为。
- `probe_session_unlocked` 的 Win32 查询体（`src/suspend.rs:210`-245）与「RDP 未实测组合」语义边界文档不动。
- `src/recovery.rs:209`-217 的 rearm_display_notify / ensure_power_notifications 分支语义不动：只把掩码运算换成 bool 判读。
- 三原因强度差异本身（SYSTEM 自证、SESSION 探针、MONITOR 超长 TTL）是 AGENTS.md 保护的自愈语义，不做统一超时。

## 为什么不保留？

最强反方：① 纯函数矩阵让 TTL 边界与「探针失败保持位不变」在无 Win32 环境下单测可验，全内联后这些判定混入调用 resume_system 的函数，边界只剩冒烟覆盖；② 位集返回与 SUSPEND_REASONS 位协议风格同构，未来消费方可能想按原因区分自愈结果；③ ReasonProbe 文档记录了三档证据强度设计，删枚举即删文档。回应：① 提案刻意保留两个纯函数（monitor_ttl_reached、session_unlocked_confirmed），承重的判定一个不少地仍可单测，且 session 真值表新增 None 态——旧矩阵测试反而表达不了它（None 被调用方 let-Some 挡在矩阵外）；被删的只是恒真 SYSTEM 仪式与不可达错配格。② 「未来可能用」正是本次审计要清的推测性通用性：唯一调用点只读 MONITOR 位有检索证据，真到需要按原因区分时改回位集是机械签名变更。③ 强度理据移入分支注释与既有函数文档（`:247`-253 已完整覆盖三档设计），文档不丢。保守替代方案（只删 RunningTick + SYSTEM 臂 + 相关断言、保留矩阵，净删约 22 行）也成立，但留下仍有不可达格的矩阵与透传臂，不如两个具名纯函数直白。

## 验收标准

- `grep -rn "ReasonProbe\|should_resume_for_reason" src/` 0 命中；`grep -rn "heal_stale_suspend" src/` 显示 `-> bool` 签名与 `src/recovery.rs` 的 `if heal_stale_suspend(hwnd) {`。
- `cargo test --locked` 全绿：TTL 边界用例（TTL-1 判否 / TTL 判真）与 session 三态真值表（None 判否、Some(false) 判否、Some(true) 判真）通过；`test_timer_plan_*` 系列原样通过。
- opt-in 冒烟 `cargo test --locked -- --ignored --test-threads=1` 全绿（真实 Explorer）。
- `cargo fmt -- --check`、`cargo build --release --locked`、`cargo clippy --all-targets --locked -- -D warnings` 全过且无 warning。

## 风险

残留风险：SESSION 的 None 保守方向现在承载于比较运算符的选取（`== Some(true)` 而非 `!= Some(false)`）——缓解：三态真值表的 None 用例专门钉死它，写反即红（证伪依据明确）。行为等价性逐臂可核对：SYSTEM `is_set && true ≡ is_set`；SESSION `is_set && let Some(unlocked) && unlocked ≡ is_set && probe == Some(true)`；MONITOR 判据不变；返回值旧调用方测 MONITOR 位 ≡ 新 bool。若冒烟发现自愈行为差异，即证伪本提案的等价论证，应回退并复查。