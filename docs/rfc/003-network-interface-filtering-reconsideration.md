# RFC: 重新审视网络接口过滤逻辑 (Network Interface Filtering Reconsideration)

## 背景与回顾
在 [RFC 002: 精准过滤网络虚拟接口 (002-network-interface-filtering.md)](file:///D:/work_space/life/traffic-monitor/docs/rfc/002-network-interface-filtering.md) 中，为了解决在启用 VPN、虚拟机（如 VMware）或 WSL 时，系统内大量的内部数据交换（例如回环路由包、虚拟交换机包）被重复计入网速、导致网速虚高或翻倍的问题，我们引入了利用 `MIB_IF_ROW2` 结构中 `HardwareInterface` 标志位的过滤逻辑。

当时的过滤实现：
```rust
fn is_physical_interface(row: &MIB_IF_ROW2) -> bool {
    let if_type = row.Type;
    if if_type != IF_TYPE_ETHERNET_CSMACD && if_type != IF_TYPE_IEEE80211 {
        return false;
    }
    if row.PhysicalAddressLength == 0 {
        return false;
    }
    if row.InterfaceAndOperStatusFlags._bitfield & HARDWARE_INTERFACE_MASK == 0 {
        return false;
    }
    true
}
```

## 新暴露的问题 (The vEthernet Traffic Leak)
虽然 RFC 002 成功地为大部分普通用户过滤掉了纯虚拟网卡流量，但它给开启了 **Hyper-V、WSL2 或 Docker Desktop** 的开发者用户带来了严重的影响：

1. **网络拓扑改变**：当开启 Hyper-V 后，Windows 会将真实的物理网卡（如 PCIe 有线网卡或 Wi-Fi 网卡）绑定到虚拟交换机（Virtual Switch）上。
2. **物理网卡架空**：此时在 IP 协议栈层面，物理网卡不再直接接收和发送 IP 报文，其 `InOctets` 和 `OutOctets` 计数器几乎停止增长。
3. **虚拟网卡承载外网流量**：主机的 TCP/IP 流量在 IP 层实际上全部通过名为 `vEthernet (Default Switch)` 等虚拟网卡来进行发送 and 接收。
4. **网速归零**：由于 `vEthernet` 属于虚拟网卡，其 `HardwareInterface` 标志位为 `0`（`false`）。在 RFC 002 的严格限制下，`vEthernet` 会被 `is_physical_interface` 直接过滤。导致本软件最终获取到的所有硬件接口流量为 0，即**在启用了虚拟化的电脑上，网速显示始终为 0**。

---

## 方案权衡与分析 (Trade-offs Analysis)

为了在“无配置文件、纯 Rust 极简嵌入任务栏”的约束下解决该问题，我们对以下几个方案进行了评估：

### 方案 1：维持现状（仅监控 HardwareInterface）
* **优点**：数据绝对纯净，不受任何虚拟网卡和内网大文件传输的干扰。
* **缺点**：对 Hyper-V、WSL2、Docker 用户极不友好，网速功能在这些环境下完全失效。
* **结论**：不予采用。

### 方案 2：完全放开硬件限制（允许虚拟网口累加）
* **做法**：移除 `InterfaceAndOperStatusFlags._bitfield & HARDWARE_INTERFACE_MASK == 0` 的判断，允许所有 UP 状态、有 MAC 的以太网和 Wi-Fi 接口的流量进行相加。
* **优点**：代码极简。完美支持 `vEthernet` 虚拟网口，彻底解决虚拟化环境下网速为 0 的问题。还能自动支持各种通过虚拟以太网承载的 VPN / 拨号连接。
* **缺点**：如果 WSL2 或本地虚拟机与主机进行高带宽大文件传输时，这些未流向外网的流量也会被统计，导致网速虚高。
* **结论**：**推荐采用**。作为无配置文件的悬浮窗或任务栏小组件，保障“网速不归零、高可用”是第一优先级的，这也是大部分类似流量监控工具的通用折中方案。

### 方案 3：放宽硬件限制 + 关键字黑名单过滤
* **做法**：获取 `MIB_IF_ROW2` 的 `Alias` 或 `Description` 字段，排除掉带有 `"WSL"`, `"VirtualBox"`, `"VMware"`, `"Host-Only"` 等关键字的虚拟网口，保留 `"vEthernet"` 等上网网口。
* **优点**：比方案 2 更精确，能排除掉已知的 WSL 内部流量。
* **缺点**：
  1. Windows 系统具有多语言版本（中文、英文、日语等），网卡描述会被本地化，基于硬编码字符串的匹配极易在非中文/英文系统上失效。
  2. 增加了多余的字符串转换与匹配开销，与极简、无锁的设计初衷存在一定偏离。
* **结论**：暂不作为首选，但在需要更精细控制时可以作为保留优化。

### 方案 4：基于活跃默认网关动态监控单网卡
* **做法**：每次轮询时，不采用“多网卡流量累加”的方案，而是通过路由表 API 或跃点数（Metric）找到当前负责连接外网的唯一默认网关网口，并只统计它的流量。
* **优点**：非常科学精准，不仅能动态适应网络切换，还能直接支持虚拟化下的 `vEthernet`，同时完美规避了其他虚拟机网卡的内网流量干扰。
* **缺点**：开发工作量大，涉及大量底层的 IP 路由表查询 API（如 `GetIpForwardTable2`），大大增加了软件体积和崩坏风险。
* **结论**：不予采用，过度设计。

---

## 最终决策 (Decision)
我们决定选择 **方案 2**（完全放开硬件接口限制，仅保留类型和物理地址校验）。

我们将 `is_physical_interface` 重命名为更契合其职责的 `is_valid_interface`：
1. 仅筛选 `IF_TYPE_ETHERNET_CSMACD` 和 `IF_TYPE_IEEE80211`。
2. 过滤掉 `PhysicalAddressLength == 0` 的接口（如回环等）。
3. 移除 `HardwareInterface` 过滤，以便完整支持 Hyper-V / WSL2 下的 `vEthernet` 等虚拟上网接口。

这种平衡在用户体验上更加稳健：对开发者而言，能够看到带有虚拟化流量的外网网速，远比看到“冰冷的 0”要好得多。
