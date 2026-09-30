# Agent Note：给网卡筛选加"零候选救回"——让名字黑名单不再单独决定生死

Status: proposed

## 问题

`src/collector/network.rs:180-195` 用 13 个子串判定虚拟网卡：`virtual` / `vbox` / `vmware` / `hyper-v` / `wsl` / `tap` / `vpn` / `loopback` / `teredo` / `6to4` / `ppp` / `kvm` / `xen`，命中即把该接口的 LUID 整张塞进黑名单（`src/collector/network.rs:260-264`）。叠加 `src/collector/network.rs:168-170` 的 `PhysicalAddressLength == 0` 直接拒绝，任何一条判据为真，该接口在 `collect_network` 的候选循环里都进不去（`src/collector/network.rs:108-122`），`NET_SPEED_UP/DOWN` 只能是 0。

真正会 **100% 命中**的场景是虚拟化客户机：Hyper-V 客户机里唯一那张网卡就叫 `Microsoft Hyper-V Network Adapter`——本仓库自己的测试正例（`src/collector/network.rs:374`）就把这个名字断言为"必须被拉黑"；VMware / VirtualBox 客户机的网卡名同理（`src/collector/network.rs:373`、`:372`），而客户机里不存在第二张物理网卡。于是 `current_data` 恒空 ⇒ 永久 0 B/s，若干 tick 后还被当成"断网"退避到 15 秒采样。这类用户不是边缘场景，是 100% 命中且无法自救。

本条的救回只覆盖"候选全灭"，它**不**解决另外两类失效，实施时必须把边界写进注释：①类型白名单（`src/collector/network.rs:33-38`）或 MAC 判据（`:168-170`）本身误杀——救回仍走同一套判据，救不回来；②本该用的接口被拉黑、但还剩另一张能过筛——那时显示的是错数字而不是 0，救回按"仍有候选"不触发。另外两条曾经写在这里的推测性论据已删除，因为它们经不起核对：`tap` 命中 `Microsoft ISATAP Adapter`（`src/collector/network.rs:380-381` 的测试注释）**不是误伤**——ISATAP 是隧道伪接口，拉黑它是可辩护的；"WWAN 接口没有 MAC"不成立——WWAN 设备有物理地址，`test_is_valid_interface_wwan_accepted`（`src/collector/network.rs:333-344`）自己就用 `PhysicalAddressLength: 6` 构造。

**没有任何逃生门**：仓库按产品前提不带配置文件（AGENTS.md「网速采样」只要求"不得把名称启发式冒充物理网卡证明"），被误杀的用户看到的是恒 0 B/s，唯一自救手段是开 `EnableDebugLog`（`src/util.rs:375-381`）再提 issue。

还一个被放大的副作用：候选全灭时 `current_data` 为空，`CONSECUTIVE_ZERO_COUNT` 累到 5（`src/config.rs:95`）就投 `WM_USER_NETWORK_DISCONNECTED`（`src/collector/network.rs:134-138`），主窗口据此把网络定时器切到 15 秒退避（`src/main.rs:682-685`、`src/config.rs:81`）。也就是"名字启发式误杀"被系统当成"断网"处理，采样率还降到 1/15。

生产消费者：`src/collector/network.rs:103-146`（`collect_network` 的筛选与选择）、`src/collector/rate.rs` 的 `select_winner_interface`（赢家挑选）、`src/main.rs:633-645`（网络 tick）。符号检索：`grep -rn "is_virtual_friendly_name" src/` 命中 3 行（定义 `src/collector/network.rs:180`、唯一调用点 `:260`、测试 `:390`），除测试外无其他消费者。

非生产消费者：`src/collector/network.rs:368-402` 的 `test_virtual_friendly_name_matrix`（15 个正例 + 3 个"不应误判"负例，注释自认 ISATAP 由 `tap` 承担）；`docs/archive/rfc/` 里的历史笔记（不作本条依据）。

## 提案

1. **加一条"零候选救回"**：在 `collect_network` 的黑名单过滤之后，如果"通过类型白名单 + MAC 判据"的 up 接口一个都没剩、但确实存在这样的接口（即它们只是被名字判据否掉），则**退回忽略名字黑名单**、仅按类型白名单 + MAC 判据选赢家，并记一条 `log_event!`（按"是否处于救回状态"去重，避免每 tick 一条）。这不会引入多卡累加（仍然只选一张、仍走 `select_winner_interface`），也不改变"同一周期同一张卡"的不变量；它只是把名字启发式从**承重判据**降级为**偏好**。
2. **把宽子串的语义写清楚**：`tap` / `ppp` / `vpn` 这类宽子串在"救回"下自然失去杀伤力；在 `src/collector/network.rs:380-381` 的注释里把"ISATAP 被 tap 命中"改写为"这是已知代价，由零候选救回路径承担"，避免下一个人以为这是有意为之的正确行为。
3. 测试按"判据分层"补：黑名单不再单独决定生死后，需要一条用例证明"全部候选被名字否掉时仍能选出一张"，以及一条"确实没有 up 接口时**不**救回（保持 0 与退避）"。

## 明确不在本次范围

- **不引入配置文件/注册表/CLI 开关**（AGENTS.md 的"无配置文件"是产品前提）；逃生门由"零候选才触发"这条自动机制提供，不交给用户。
- **不删 `is_virtual_friendly_name`**：Hyper-V / WSL2 / Docker Desktop 场景下外网流量确实由 vEthernet 之类的虚拟网口承载，`src/collector/network.rs:172-176` 的注释解释了为什么这里必须留名字判据（并刻意不看 `HardwareInterface`）。救回只在"零候选"时生效，正常机器（能选出物理卡）行为完全不变。
- 不动 `SUPPORTED_IF_TYPES`（`src/collector/network.rs:33-38`）与它的三条用例（`test_is_valid_interface_ethernet` / `_wifi` / `_wwan_accepted`）；不含 PPP 的决策 `src/collector/network.rs:30-32` 已写明理由。
- 不改 `BLACKLIST_REFRESH_SECS`（`src/config.rs:97`）的刷新节奏与失败保留旧表的策略（`src/collector/network.rs:296-306`）。

## 为什么不保留？

1. **"无配置文件是卖点，误判是代价"**——代价本身可以接受，但"没有任何自动逃生门"不是代价而是缺陷；本条不需要配置面就能把误判从"永久 0 B/s"变成"退化为不完美但可用"。
2. **"救回会选中用户故意要屏蔽的虚拟卡"**——只在零候选时触发；此时现状是 0 B/s（完全不可用），救回最坏是显示一张虚拟卡的速率（可用但不完美）。方向是严格变好。
3. **"名字黑名单已经足够准"**——`tap` 命中 ISATAP 是测试自己承认的；`6to4`/`teredo` 是隧道伪接口，但对某些只走隧道的环境并非永远错。
4. **"零候选时应该让用户看到 0，以便感知断网"**——0 与"断网"在 UI 上无法区分（`WM_USER_NETWORK_DISCONNECTED` 只影响定时器退避），所以现在这个 0 既不能诊断也不能行动。
5. **"该先做配置化开关"**——配置面与"无配置文件"的产品前提冲突，且把诊断责任推给用户；先做自动救回，若将来仍不够再谈开关。

## 验收标准

- 新增用例（可离线，纯函数化筛选后喂 `MIB_IF_ROW2` 列表）：`test_all_candidates_blacklisted_falls_back`（全被名字否掉仍有 up 接口 ⇒ 选出一张且非零候选）与 `test_no_up_interface_does_not_fall_back`（确实没有 up 接口 ⇒ 保持空候选、仍走退避）。
- 现有 `test_virtual_friendly_name_matrix`（`src/collector/network.rs:368-402`）与三条 `test_is_valid_interface_*` 保持绿且断言不变（黑名单本身不变，只是不再单独决定生死）。
- `grep -n "救回\|fallback" src/collector/network.rs` 能定位到救回分支，且该分支内有 `log_event!` 且有去重（同一个救回状态每进程只记一次或仅在状态切换时记一次）。
- `cargo test --locked`、`cargo clippy --all-targets --locked -- -D warnings` 全绿。

## 风险

- 救回选中一张"不该统计"的卡时，用户看到的数字会变化（例如从 0 变成某个虚拟交换机的流量），可能被理解为"统计不准"；缓解手段是救回时记日志，并让 README 的"网速采样"说明可被引用。
- 救回是"评估顺序"的改变：必须保证它只在"零候选"时生效，且不能改变"同一周期只选一张、不累加"的既有不变量；实现时不要在 `select_winner_interface` 的循环里做二次挑选，而应在候选集合构造阶段就把黑名单降级，让赢家挑选逻辑保持单一。
- 若某台机器长期处于救回状态，"每 tick 探测 + 每 tick 记录"会变成新的日志噪声源——所以去重不是可选优化，而是本条的组成部分。
