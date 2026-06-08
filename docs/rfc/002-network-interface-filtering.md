# RFC: 精准过滤网络虚拟接口 (Network Interface Filtering)

## 背景
目前在 `collector.rs` 中，网络速度统计 (`collect_network`) 通过调用 Win32 API `GetIfTable2` 来遍历系统当前所有的网络接口。程序判断一个接口是否为需统计的“真实物理接口”依据以下条件：
1. 接口类型 (Type) 必须为 `IF_TYPE_ETHERNET_CSMACD` (6) 或 `IF_TYPE_IEEE80211` (71)。
2. 物理 MAC 地址长度 (PhysicalAddressLength) 必须大于 0。
3. 接口当前运行状态必须为 `IfOperStatusUp`。

然而，随着 Windows 系统内虚拟化技术的普及，诸如 VMWare、Hyper-V (vEthernet)、WSL、以及各类内网穿透与 VPN 软件（如 ZeroTier、Tailscale）均会向系统注册并创建完全满足上述三个条件的“虚拟网卡”。
这导致在开启上述软件时，同一台主机内部的环回路由包、内部虚拟交换机流量会被误认为是外部网络请求并被重复计入，进而导致状态栏上的网速显示翻倍或产生严重偏差。

## 提议方案
Windows 在 `MIB_IF_ROW2` 结构中，提供了一个极具针对性的内联位域：`InterfaceAndOperStatusFlags`。
在该位域中包含了一个非常重要的布尔值属性：`HardwareInterface`。当且仅当此标志为 `true` 时，该网络接口才是由真实存在的底层物理硬件（PCIe/USB 网卡等）支持的，由软件驱动生成的虚拟网卡该位均为 `false`。

## 具体修改内容

针对 `src/collector.rs` 中 `is_physical_interface` 的修改：

```rust
fn is_physical_interface(row: &MIB_IF_ROW2) -> bool {
    // 1. 保留原本的类型过滤，只处理以太网和 Wi-Fi
    let if_type = row.Type;
    if if_type != IF_TYPE_ETHERNET_CSMACD && if_type != IF_TYPE_IEEE80211 {
        return false;
    }

    if row.PhysicalAddressLength == 0 {
        return false;
    }

    // 2. 新增：利用 HardwareInterface 标志位一击必杀排查绝大多数虚拟网卡
    // windows-rs 0.62 尚未为该位域生成 HardwareInterface() getter，
    // 因此实际实现使用手工掩码（bit 0，mask 0x01）。
    const HARDWARE_INTERFACE_MASK: u8 = 0x01;
    if row.InterfaceAndOperStatusFlags._bitfield & HARDWARE_INTERFACE_MASK == 0 {
        return false;
    }

    // 3. (可选补充防御)：若仍有个别不守规矩的虚拟网卡将硬件标志设为 true，
    // 可将其 Description 字段转换为宽字符串，并过滤掉 "Virtual", "Hyper-V", "VMware" 等特定关键词。

    true
}
```

## 工程量与风险评估
- **工程量**：极小。代码行数变更通常在 5 行以内，不涉及任何状态同步与架构层面的调整。
> **注**：当前项目依赖的 `windows` crate v0.62 尚未为该位域生成 `HardwareInterface()` getter，
> 因此实际实现采用手工位掩码 `_bitfield & 0x01`。待依赖升级后可替换为更可读的 getter 调用。

- **风险**：几乎没有。`HardwareInterface` 标志位从 Windows Vista / Windows 7 时代起就已经实装在 NDIS 协议栈底层，行为在 Windows 11 环境下极其可靠和稳定。合入此改动即可彻底解决因使用虚拟机或 WSL 引发的网速异常翻倍问题。
