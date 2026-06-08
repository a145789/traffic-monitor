# RFC 0001: 多网卡环境下的自动锁定与适配器过滤

- **状态**: Draft (草案)
- **创建时间**: 2026-06-08
- **关联问题**: 问题 4 (多网卡环境网速统计冲突)

---

## 1. 背景与现状 (Context)

在目前的 [collector.rs](file:///D:/work_space/life/traffic-monitor/src/collector.rs) 实现中，`collect_network` 函数通过 `GetIfTable2` 获取系统中所有的网络接口，并累加所有符合 `is_valid_interface` 条件的物理网卡（Ethernet 和 Wi-Fi）的上下行字节数（`InOctets` / `OutOctets`）。

这种简单累加的方案在以下多网卡共存的典型场景中存在缺陷：
1. **多网口同时活跃**：用户同时连接了有线网和 Wi-Fi，或者在使用有线网的同时开启了 VPN 虚拟网卡，导致累加出的网速成倍增长或数据失真。
2. **虚拟网卡干扰**：部分第三方软件或 VPN 建立的接口类型虽被识别为 Ethernet，且有物理地址（`PhysicalAddressLength > 0`），但实际为虚拟网卡，导致统计了不必要的后台环路流量。

---

## 2. 方案设计 (Proposed Design)

本 RFC 提议采用**“活跃网卡自动锁定为主网卡”**结合**“基于 FriendlyName 过滤虚拟适配器”**的双重过滤方案。

### 2.1 流量活跃度自动锁定机制

不再将所有有效接口的流量做简单的累加，而是对每个 LUID (Locally Unique Identifier) 接口的网速做独立计算。在每一次轮询周期中，选择**网速最大（或当前有流量）的单一网卡**作为当前活跃的“主网卡”，且只有该网卡的网速会被存入全局变量 [NET_SPEED_UP](file:///D:/work_space/life/traffic-monitor/src/config.rs) 和 [NET_SPEED_DOWN](file:///D:/work_space/life/traffic-monitor/src/config.rs)。

为了维护每个接口的历史状态，需在 [collector.rs](file:///D:/work_space/life/traffic-monitor/src/collector.rs) 中引入一个受互斥锁保护的全局 `HashMap`：
```rust
use std::collections::HashMap;
use std::sync::Mutex;

lazy_static! {
    static ref INTERFACE_HISTORY: Mutex<HashMap<u64, (u64, u64)>> = Mutex::new(HashMap::new());
}
```

### 2.2 虚拟网卡黑名单过滤

为了防止一些隐蔽的虚拟网卡伪装成物理网卡，在 `is_valid_interface` 校验的基础上，通过调用 Windows 的 `GetAdaptersAddresses` API 动态遍历系统适配器列表，提取其 `FriendlyName`（如 `vEthernet (WSL)`）并转为 Rust `String`。

若 `FriendlyName` 或 `Description` 包含以下黑名单关键字，则直接过滤：
* `virtual`, `vbox`, `vmware`, `hyper-v`, `wsl`, `tap`, `vpn`, `loopback`

#### 调用流程设计：
1. 在 [collector.rs](file:///D:/work_space/life/traffic-monitor/src/collector.rs) 增加过滤辅助函数：
   ```rust
   fn is_virtual_friendly_name(name: &str) -> bool {
       let name_lower = name.to_lowercase();
       name_lower.contains("virtual") 
           || name_lower.contains("vbox") 
           || name_lower.contains("vmware") 
           || name_lower.contains("hyper-v") 
           || name_lower.contains("wsl")
           || name_lower.contains("tap")
           || name_lower.contains("vpn")
   }
   ```
2. 在 `collect_network` 内部遍历 `MIB_IF_ROW2` 时，使用 `GetAdaptersAddresses` 过滤，排除匹配黑名单的接口。

---

## 3. 兼容性与副作用 (Drawbacks & Alternatives)

* **性能消耗**：每次执行 `GetAdaptersAddresses` 会导致少量的 CPU 额外开销。
  * *优化方案*：由于硬件适配器列表不经常改变，我们可以只在**检测到 IP 地址或网络变更时（例如接收到 `WM_DEVICECHANGE` 消息）**，或者**每隔 30 秒**重新生成一次“虚拟网卡 LUID 黑名单集合”，在常规的 1s 定时器里只需查表过滤，从而实现零性能损耗。
* **备选方案**：完全无过滤，而是在系统托盘菜单中将所有可用网卡列出，由用户手动勾选锁定。但这违反了本项目“开箱即用、零配置”的极简主义设计初衷。

---

## 4. 未解决问题 (Unresolved Questions)

1. **多网口同时并发合理性**：若用户确实想看多网口聚合的总网速，锁定单一活跃网卡将无法满足。后续可考虑在配置中支持“聚合显示”与“单网卡锁定”的切换（详见 RFC 0002）。
