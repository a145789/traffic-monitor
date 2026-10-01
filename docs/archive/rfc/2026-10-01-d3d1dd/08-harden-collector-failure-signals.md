# Agent Note：加固采集侧的故障信号与赢家确定性

Status: rejected（复核裁决：API 失败频率未知、平局归属用户不可见、黑名单窗口 30 秒自愈，感知不足，不实施）

## 问题

**其一，`GetIfTable2` 失败被完全静默。** `src/collector/network.rs:95-102` 拿到错误码或空表时直接 `return`：既不写 `NET_SPEED_DOWN`/`NET_SPEED_UP`（`:120-121`），也不动 `CONSECUTIVE_ZERO_COUNT`（`:123-133`），更没有任何 `diag!`/`log_event!`。后果有两层：显示层的速率行会**冻结在失败前的最后一个值**（包括看起来完全正常的非零值）；检测层则永远不会因为这个原因累计到 `BACKOFF_ZERO_THRESHOLD`（`src/config.rs:108`），于是「断网」通知（`src/collector/network.rs:125-127` 投递 `WM_USER_NETWORK_DISCONNECTED`，处理见 `src/main.rs:856-866`）与 15 秒退避（`src/config.rs:94` 的 `TIMER_INTERVAL_NETWORK_BACKOFF`）都不会触发。按本仓的埋点纪律（`src/util.rs:428-459`：`diag!` 只加在「失败后无任何感知通道」的调用点），这里正是该有埋点而完全没有的地方。

**其二，平局时赢家不确定。** `src/collector/rate.rs:28` 遍历 `HashMap`（迭代序由 `RandomState` 决定），`:36` 用严格 `total > max_total` 择大，因此两张卡的「上行+下行」总量**完全相等**时（典型是反向等流量），赢家取决于本次迭代顺序：同一台机器两次运行可能显示不同的卡，上下行两行的归属随之对调。这不违反 AGENTS.md 的「同一周期只选一张、不累加、不跨周期粘滞」——那几条都成立（`select_winner_interface` 每周期独立择大、`:44-47` 用 `history.retain` 清离线）——违反的是「同一确定性输入应当给出同一输出」这条工程预期，而现有用例只覆盖不并列的场景（`src/collector/rate.rs:118-133`）。

**其三，黑名单的覆盖窗口。** 虚拟接口黑名单来自 `GetAdaptersAddresses`（`src/collector/network.rs:273-352`），缓存 30 秒（`src/config.rs:110` 的 `BLACKLIST_REFRESH_SECS`，判据见 `src/collector/network.rs:348-352`）。两次调用都用 `flags = 0`（`:277-284`、`:294-302`），而该 API 在默认标志下不返回「没有启用对应地址族」的接口；于是「刷新之后才上线」的虚拟卡（VPN / vEthernet / WSL）在最长 30 秒内不在黑名单里，可能赢下显示。它是有界自愈，且名字判据在本仓只是**偏好**（候选全灭时有零候选救回，见 `src/collector/network.rs:138-183`），但「偏好判据在窗口内静默失效」这件事没有任何可观测点。

## 提案

1. **失败与「零候选」分成两条语义，失败要有埋点。** 在 `src/collector/network.rs:100-102` 的失败分支补一次 `diag!` + 一次 `log_event!`，并按本仓既有的去重范式（`report_zero_candidate_fallback`，`:185-201`）只记录「失败序列首次」。**关键：不得把 API 失败当成断网**——那会触发 15 秒退避与一次用户可见的断网/恢复通知，是对未知故障的过度反应；失败的正确语义是「本周期无数据，保持上次展示值」，只是必须可观测。
2. **平局用 LUID 做第二判据。** 把 `src/collector/rate.rs:36` 的判据改为「总量更大，或总量相等且 LUID 更小」，使同一输入集合在任何进程、任何迭代序下都给出同一赢家；新增一条并列用例（两个 LUID 总量相同，断言赢家是固定的那个，且上/下行归属来自同一张卡）。
3. **黑名单窗口二选一（建议同时做，成本都低）：** (a) 两次 `GetAdaptersAddresses` 加上 `GAA_FLAG_INCLUDE_ALL_INTERFACES`（0x100），让「已存在但尚无地址」的接口也能被名字启发式抓进黑名单；(b) 把刷新触发从「纯 TTL」扩成「TTL 到期**或**本轮候选集合出现缓存中不存在的 LUID」——候选行已经从 `table_wrapper.rows()` 拿到（`src/collector/network.rs:104-110`），因此可以在进黑名单层之前收集一次 LUID 集合，交给 `with_virtual_blacklist`（`:335-346`）判定是否需要强制刷新。代价是每次新接口上线多一次 `GetAdaptersAddresses`（罕见事件）。

## 明确不在本次范围

- 不改「每周期只选一张、不累加、不粘滞赢家」这条不变量：本节的第 2 项只固定平局结果，不引入跨周期状态。
- 不改黑名单的关键字表与零候选救回（`src/collector/network.rs:234-253`、`:138-183`）：名字启发式的定位（偏好而非生死判据）是既有裁定。
- 不改 `BACKOFF_ZERO_THRESHOLD`、退避间隔与断网通知的协议（`src/config.rs:94`、`:108`、`src/main.rs:856-866`）。
- 不改速率归一化（`src/collector/rate.rs:61-65` 的 u128 与饱和）与计数器类型（`InOctets`/`OutOctets` 是 u64，回绕由 `saturating_sub` 吸收）。
- 不改 `MibTable` 的所有权与 `FreeMibTable` 配对（`src/collector/network.rs:51-79`）。

## 为什么不保留？

1. 「早退就是显式错误路径，够了。」—— AGENTS.md 对「可恢复错误不得 panic」的最低要求是显式路径，但本仓另有更具体的埋点纪律：**失败后无任何感知通道的调用点必须留痕**（`src/util.rs:428-436` 的注释逐字如此）。这里两者都不满足：既有冻结显示，也抑制了断网检测。
2. 「把失败也累加进零计数，让它退避到 15 秒，反而更省电。」—— 那是把「API 故障」冒充「网络断开」，会向用户发一次假的断网/恢复通知，并让一次瞬态 API 失败换来 15 秒的低频采样。方向是错的。
3. 「平局几乎不可能出现。」—— 反向等流量（一边下载一边上传同样字节数）在测速、备份、同步类工具下很常见；即便罕见，修法是一次比较加一条用例。
4. 「黑名单窗口内选错卡只是数字不完美。」—— 本仓对「名字启发式不得冒充物理网卡证明」已有明确表述（AGENTS.md 的网速采样不变量），本项不改判据本身，只是让窗口内不发生静默失效，方向与该表述一致。

## 验收标准

- 代码：`grep -n "log_event\|diag" src/collector/network.rs` 在 `collect()` 的失败分支命中 ≥1 处，且**不**出现在 `CONSECUTIVE_ZERO_COUNT` 的自增路径上（失败不得被计成断网）。
- 用例：`src/collector/rate.rs` 新增并列用例（同名同类风格，与 `test_select_winner_interface_multiple_active` 并列），断言平局赢家固定；`src/collector/network.rs` 若做 (b)，为「未知 LUID 触发刷新」补一条纯函数用例（刷新判据已是可测的纯函数形态，见 `:348-352`）。
- 行为一致性：既有 `test_select_winner_interface_*`（`src/collector/rate.rs:118-203`）、`test_is_valid_interface_*` 与 `test_virtual_friendly_name_matrix`（`src/collector/network.rs:371-463`）全部不回归。
- 四条门禁全绿。

## 风险

- 第 3 项的 (b) 会在新接口上线时多一次 `GetAdaptersAddresses` 调用（罕见、且失败时按 `rebuild_virtual_blacklist` 的既有语义保留旧表，`src/collector/network.rs:354-364`），但若「未知 LUID」判定写错（例如把每次 LUID 变动都当未知），可能退化成每周期刷新；实施时必须以「LUID 在缓存中不存在」为唯一判据，并补用例。
- 新增的失败埋点在持续故障时会写日志；复用既有的「失败序列首次」去重范式可避免刷屏，但那条范式目前只服务零候选救回，抽取时注意不改变它的既有语义。
- 未覆盖：`GetIfTable2` 返回非 0 的现场频率本机无法构造（需要资源耗尽类故障），因此第 1 项的收益以「可观测性」为主，冻结显示的实际发生率未知。
