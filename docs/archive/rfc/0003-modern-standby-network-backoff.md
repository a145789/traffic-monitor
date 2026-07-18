# RFC 0003: 现代休眠（Modern Standby）兼容性与断网自适应降频

- **状态**: Draft (草案)
- **创建时间**: 2026-06-08
- **关联问题**: 现代休眠兼容与低功耗状态下的退避降频

---

## 1. 背景与现状 (Context)

为了保证极低的系统资源占用，本项目已经在 [main.rs](file:///D:/work_space/life/traffic-monitor/src/main.rs#L89-L134) 中实现了一套挂起机制，即在全屏游戏、系统传统休眠（`PBT_APMSUSPEND`）或锁屏（`WTS_SESSION_LOCK`）时暂停定时器与鼠标轮询线程。

然而，在面对现代 Windows 11 设备和断网等极端场景时，仍有以下优化空间：
1. **现代休眠 (Modern Standby / S0 Low Power Idle)**：许多新型笔记本在合盖或短按电源键时不再进入传统的 S3 睡眠状态（即不触发 `PBT_APMSUSPEND`），而是进入 Modern Standby。此时程序依然会以原有频率（如 1s）运行，从而在后台产生无谓的电量损耗。
2. **断网时的无效轮询**：当网线被拔出或 Wi-Fi 断开时，程序检测不到网络流量，但网速采集定时器仍然会以 1000ms 的频率频繁调用 `GetIfTable2` 并重新计算。在移动办公场景下，这会缩短设备的续航时间。

---

## 2. 方案设计 (Proposed Design)

本 RFC 提出通过**“屏幕电源状态注册”**来兼容 Modern Standby，并结合**“网络地址变更通知”**实现断网自适应降频。

### 2.1 现代休眠兼容：基于 `GUID_MONITOR_POWER_ON` 状态监听

在 Modern Standby 状态下，Windows 会关闭屏幕并将系统挂起。我们可以通过注册屏幕电源状态通知来截获此状态变化：

1. 在 `wnd_proc` 的 `WM_CREATE` 阶段，调用 `RegisterPowerSettingNotification`：
   ```rust
   use windows::Win32::System::Power::{RegisterPowerSettingNotification, HPOWERNOTIFY};
   use windows::core::GUID;

   // 屏幕电源状态 GUID
   const GUID_MONITOR_POWER_ON: GUID = GUID::from_u128(0x0273b28b_2d60_4396_a078_d5f143136a7e);

   // 存储注册句柄以便后续注销
   static mut POWER_NOTIFY_HANDLE: Option<HPOWERNOTIFY> = None;
   ```
2. 在 `WM_POWERBROADCAST` 消息中处理 `PBT_POWERSETTINGCHANGE`：
   * 读取 `POWERBROADCAST_SETTING` 结构体。如果是 `GUID_MONITOR_POWER_ON`，判断其 `Data` 字段：
     * `0`: 屏幕已关闭（进入 Modern Standby），此时将 `SUSPENDED` 设为 `true`，停止定时器并销毁鼠标线程。
     * `1` / `2`: 屏幕已点亮（唤醒），重置 `SUSPENDED` 为 `false`，重新注册定时器并拉起鼠标线程。

---

### 2.2 断网自适应退避降频 (Adaptive Backoff)

当网络完全断开时，我们需要将查询频率拉长，而在网络重新连接时能即时唤醒，避免网速显示滞后。

#### 2.2.1 状态检测与降频
1. 在 [collector.rs](file:///D:/work_space/life/traffic-monitor/src/collector.rs) 的 `collect_network` 轮询中，若连续 5 次获取的网速上下行速率均为 0，且通过 IP Helper 检查到当前没有任何处于 `Up` 状态的物理网络接口。
2. 通过 `PostMessageW` 向主窗口发送自定义消息 `WM_USER_NETWORK_DISCONNECTED`。
3. 主窗口接收到消息后，将 `TIMER_ID_NETWORK` 的周期从 `1000ms` 动态重置为 `15000ms`（15秒）。

#### 2.2.2 实时唤醒机制 (NotifyAddrChange)
为了在网线重新插上或 Wi-Fi 自动连上时**立刻**恢复 1s 刷新，我们使用 Windows 的网络状态监听机制：
1. 启动一个极轻量级的后台线程（或在现有线程中），调用异步 IP Helper 函数 `NotifyAddrChange`：
   ```rust
   use windows::Win32::NetworkManagement::IpHelper::NotifyAddrChange;
   use windows::Win32::Foundation::HANDLE;

   fn spawn_network_listener(hwnd: HWND) {
       std::thread::spawn(move || {
           let mut handle = HANDLE::default();
           let mut overlapped = std::mem::zeroed();
           loop {
               // 阻塞等待 IP 地址或路由表的任何变动
               if unsafe { NotifyAddrChange(&mut handle, &mut overlapped).is_ok() } {
                   // 向主窗体 Post 恢复通知
                   unsafe {
                       let _ = PostMessageW(Some(hwnd), WM_USER_NETWORK_RECONNECTED, WPARAM(0), LPARAM(0));
                   }
               }
               std::thread::sleep(std::time::Duration::from_millis(500));
           }
       });
   }
   ```
2. 主窗口在收到 `WM_USER_NETWORK_RECONNECTED` 消息后，立刻将 `TIMER_ID_NETWORK` 的周期重设回 `1000ms`，从而在网络恢复时实现无感延迟的显示刷新。

---

## 3. 兼容性与副作用 (Drawbacks & Alternatives)

* **线程开销**：`NotifyAddrChange` 机制需要占用一个阻塞等待的网络监听线程。
  * *对比*：该线程绝大部分时间处于阻塞休眠状态，不占用任何 CPU 时间片。这与每秒频繁调用并计算 `GetIfTable2` 相比，功耗开销极低。
* **现代待机中的鼠标轮询**：若鼠标处于充电状态，屏幕熄灭时断开鼠标 HID 连接是否合理？
  * *分析*：即使在充电，屏幕关闭时用户已无法看到屏幕，因此断开连接并挂起轮询完全合理。

---

## 4. 未解决问题 (Unresolved Questions)

1. **虚拟网卡状态变更干扰**：`NotifyAddrChange` 在虚拟网卡（如 Docker 开关）状态发生变化时也会触发。这会导致在没有真实物理网卡连网时，偶尔产生一次无效的“唤醒并再次降频”循环。由于降频机制会在 5 次检查后重新触发，这在实际使用中是完全可以接受的。
