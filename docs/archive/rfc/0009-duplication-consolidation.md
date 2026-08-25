# RFC 0009: 收敛重复生命周期与样板

- **状态**: Draft (草案)
- **创建时间**: 2026-08-25
- **更新记录**:
  - 2026-08-25: 初稿。由全仓简化审计拆分而来（本 RFC 覆盖「复制面收敛」类候选；删除死面类候选见 RFC 0008，黑名单缓存所有权简化见 RFC 0010）
- **关联文件**: `src/main.rs`, `src/util.rs`, `src/update/mod.rs`, `src/update/http.rs`, `src/tray.rs`, `src/config.rs`

---

## 1. 背景与现状 (Context)

2026-08-25 的全仓简化审计发现，除死面（见 RFC 0008）外，冗余集中在三类「双写即漏改」的复制面：启动与 Explorer 重启重建两条生命周期路径各自重复同一资源重绑序列；MessageBox 样板三处复制；注册表访问在 util 封装之外另有一条直连路径。本 RFC 收编这三项复制面收敛，全部保持现有行为不变，可与 RFC 0008、RFC 0010 各自独立成 PR。

## 2. 核心重构项

### 2.1 启动序列与 Explorer 重启重建的重复生命周期尾段

**现状**：`src/main.rs` 的 `main()`（176-199 行）与 `rebuild_main_window()`（332-344 行）各自重复同一段 4 语句序列：`create_tray_icon(hwnd)` → `renderer::with_renderer(update_dpi + update_text_color)` → `InvalidateRect` → `sync_monitoring_timers` 失败分支。两条路径必须手动保持一致；AGENTS.md 决策 5 又要求 Explorer 重启后「逐一重绑」全部资源——重复书写正是漏改风险的来源。

**方案**：在 main.rs 提取 `fn bind_display_and_timers(hwnd: HWND) -> bool`（内含上述 4 语句，返回 `sync_monitoring_timers` 的结果），两处改为调用它并保留各自的中文错误文案（「创建监测定时器失败」/「Explorer 重启后恢复监测定时器失败」）。两处顺序不同的部分（`register_session_notification` 在 main 位于 sync 之后、在 rebuild 位于托盘块之前）**不**并入 helper，避免改动消息注册顺序。

**放弃什么**：无行为变化；仅消除复制。

### 2.2 三个 MessageBox 包装收敛为一个，并消除标题字面量重复

**现状**：`src/util.rs:59-71` 的 `show_error` 与 `73-85` 的 `show_info` 除图标外逐行相同；`src/update/mod.rs:524-539` 的 `show_yes_no` 第三次复制「to_wide 标题 + MessageBoxW」样板；显示标题字面量 `"Traffic Monitor"` 重复出现在 util ×2、update ×1、tray 托盘 tip（`tray.rs:55`）×1、http User-Agent（`http.rs:68`）×1。AGENTS.md 架构表明确 util 的职责含「MessageBox 弹窗封装」。

**方案**：util 收口 `pub fn message_box(msg: &str, style: MESSAGEBOX_STYLE) -> MESSAGEBOX_RESULT`；`show_error`/`show_info` 保留为一行转发（既有 15+ 调用点不动），`update::show_yes_no` 改调 `message_box(msg, MB_YESNO | MB_ICONINFORMATION) == IDYES`；新增 `config::APP_TITLE: &str = "Traffic Monitor"` 替换各显示处字面量（`WINDOW_TITLE` 是窗口标题职责，顺带可复用但不强求）。

**放弃什么**：无。纯样板收敛。

### 2.3 tray.rs 绕过 util 的注册表封装，与既定职责分裂

**现状**：`src/tray.rs:227-232` 的 `is_autostart_enabled` 与 `234-244` 的 `toggle_autostart` 直接使用 `windows_registry::CURRENT_USER` 的 `get_string/set_string/remove_value`，与 `src/util.rs:138-150` 已有的 `reg_read_string/reg_write_string` 平行重复。这是 RFC 0007 P1 迁移的收尾：tray.rs 的直连是当时迁移留下的半成品形态。AGENTS.md 架构表规定「注册表读写」收口在 util。

**方案**：util 新增 `pub fn reg_remove_value(subkey: &str, value_name: &str) -> bool`；tray 改用 `reg_read_string / reg_write_string / reg_remove_value`，删除对 `windows_registry::CURRENT_USER` 的直接引用。错误吞并语义与现状一致（都是 `is_ok` 判断）。

**放弃什么**：无。`windows-registry` 依赖仍由 util 使用，不涉及依赖删除。

## 3. 验收标准

- main 与 rebuild 共享同一 helper，两处错误文案不变。
- MessageBoxW 调用点收口到 util 一个函数；`"Traffic Monitor"` 显示字面量只剩 config 一处。
- `src/tray.rs` 不再直接 import `windows_registry`。
- `cargo test`、`cargo clippy -- -D warnings`、`cargo fmt -- --check`、`cargo build --release` 全部通过。

## 4. 实施检查清单

- [ ] **2.1 生命周期尾段**：提取 `bind_display_and_timers`；main/rebuild 调用并保留各自错误文案；不并入 `register_session_notification`
- [ ] **2.2 MessageBox 收敛**：util 加 `message_box`；`show_error`/`show_info` 改一行转发；`update::show_yes_no` 改用之；config 加 `APP_TITLE` 并替换 5 处字面量
- [ ] **2.3 注册表收口**：util 加 `reg_remove_value`；tray 改用 util 三个封装；删除 tray 的 `windows_registry` 引用
- [ ] 全量 `cargo test` + `cargo clippy -- -D warnings` + `cargo fmt -- --check` + `cargo build --release`

## 5. 非目标与保留项

- **`TrimBookkeeping` 自适应工作集修剪、`TimerPlan` 定时器状态机、断网退避双状态**（`NETWORK_BACKOFF` + `CONSECUTIVE_ZERO_COUNT`）、渲染值变更缓存：均有注释记录的防御性设计（AGENTS.md 决策 7 的对称性要求直接依赖 `TimerPlan`），无产品决策信号不动。
- **窗口类注册/窗口创建函数两两合并（window.rs）**：会把「看门狗永不嵌入、永不显示」的决策注释（AGENTS.md 决策 5）稀释进参数化通用函数，注释契约价值高于约 25 行收益，不推荐。

## 6. 风险

- **2.1**：合并的 4 条语句在两处逐字一致（仅错误文案不同），helper 化不改执行顺序；建议真机验证一次 Explorer 重启恢复路径。
- **2.2 / 2.3**：低风险，纯样板收敛，语义不变。
