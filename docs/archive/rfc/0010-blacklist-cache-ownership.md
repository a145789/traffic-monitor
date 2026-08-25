# RFC 0010: 黑名单缓存所有权简化（去掉单线程场景下的 `Rc`）

- **状态**: Draft (草案)
- **创建时间**: 2026-08-25
- **更新记录**:
  - 2026-08-25: 初稿。由全仓简化审计拆分而来（原并入 RFC 0009，应要求独立成 PR；删除死面类候选见 RFC 0008，复制面收敛类候选见 RFC 0009）
- **关联文件**: `src/collector/network.rs`

---

## 1. 背景与现状 (Context)

2026-08-25 的全仓简化审计发现，`src/collector/network.rs` 的虚拟网卡黑名单缓存为单线程场景引入了不必要的共享所有权（`Rc`）。本项是 8 个审计候选中唯一改变运行时所有权形状的一项，且带一处需专门守护的失败路径语义，故独立成 PR，与 RFC 0008、RFC 0009 均可并行。

## 2. 核心重构项

### 2.1 黑名单缓存扁平化：去掉 `Rc`

**现状**：`src/collector/network.rs:31` 的 `type BlacklistCache = Option<(Rc<HashSet<u64>>, Instant)>` 引入了共享所有权，但其运行环境不存在任何共享：采样由主窗口 `WM_TIMER` 驱动，缓存读写全程在 UI 线程 thread_local 中；返回的 `Rc` 从不逃逸出单个采样 tick、从不跨线程。黑名单在进入 `CURRENT_DATA.with(...)`（85 行）**之前**取值（82 行），闭包内仅 `contains`（96 行），与其余两个 thread_local 无嵌套借用冲突——`Rc` 是在绕一个实际不存在的自造约束。失败回退路径（287-296 行）为凑出 `Rc` 返回值写了「clone 旧表 + 回填时间戳」的双层闭包，复杂度显著高于其保护的价值。

**方案**：缓存改为 `RefCell<Option<(HashSet<u64>, Instant)>>`，删除 `Rc` 导入与 `BlacklistCache` 别名。`get_virtual_blacklist` 改为「过期则就地重建」：成功覆盖缓存，失败保留旧表并刷新时间戳（保持现有注释语义：失败沿用旧表一个缓存周期，**勿在重构中变成每 tick 重试 `GetAdaptersAddresses`**）。`collect_network` 在进入 `CURRENT_DATA.with` 前以不可变借用读取黑名单；若借用跨度不便安排，直接 clone 一次亦可接受——每 30 秒才发生一次、量级仅几十个 u64。

**放弃什么**：「未来多线程采样」的假想收益。跨线程边界已由 state.rs 原子量承担，真要线程化采集属于另一层面的所有权重构，届时应按新所有权图重新设计，而非让单线程代码预先背负共享所有权形状。

## 3. 验收标准

- `src/collector/network.rs` 无 `Rc` 引用；`BlacklistCache` 别名删除。
- 接口过滤/虚拟网卡名单单测与 `rate.rs` 纯函数测试通过；断网退避/恢复消息路径行为不变。
- 黑名单刷新周期（`BLACKLIST_REFRESH_SECS = 30s`）不变；`GetAdaptersAddresses` 失败路径仍「沿用旧表一个缓存周期」，不出现每 tick 重试。
- `cargo test`、`cargo clippy -- -D warnings`、`cargo fmt -- --check`、`cargo build --release` 全部通过。

## 4. 实施检查清单

- [ ] `BlacklistCache` 改为 `RefCell<Option<(HashSet<u64>, Instant)>>`；删除 `Rc` 导入
- [ ] `get_virtual_blacklist` 重构为「过期则就地重建」；成功覆盖、失败保留旧表并刷新时间戳
- [ ] `collect_network` 在黑名单取值处改为不可变借用（或每 30s clone 一次）
- [ ] 专门验证失败路径仍「沿用旧表一个缓存周期」
- [ ] 全量 `cargo test` + `cargo clippy -- -D warnings` + `cargo fmt -- --check` + `cargo build --release`

## 5. 非目标与保留项

- **接口过滤逻辑、断网退避状态机**（`NETWORK_BACKOFF` + `CONSECUTIVE_ZERO_COUNT`、`WM_USER_NETWORK_DISCONNECTED/RECONNECTED` 消息路径）不动：本 RFC 只改缓存的所有权形状，不改过滤与退避语义。
- **`is_virtual_friendly_name` 黑名单关键字、`BLACKLIST_REFRESH_SECS` 周期**不变。

## 6. 风险

- 重构最易破坏「失败沿用旧表一个缓存周期」语义：验收时须专门确认失败路径不会变成每秒重试 `GetAdaptersAddresses`（该 API 每次失败都会走「首调返回 `ERROR_BUFFER_OVERFLOW` 探测大小 + 二调填充」的两段式流程，高频重试会放大开销）。
- 借用范围调整属于编译期即可发现的风险（`RefCell` 借用冲突在编译/运行早期暴露）；其余低风险。
