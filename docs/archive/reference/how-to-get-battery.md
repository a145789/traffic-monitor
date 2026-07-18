# 如何使用 Rust 获取 MLOONG MX302 鼠标电量及 DPI

本技术文档旨在指导你如何使用 Rust 语言，通过底层的 HID 协议直接与 **MLOONG MX302** 鼠标通信，获取其**电池电量**、**充电状态**以及**当前 DPI 档位与数值**。这样你就可以轻松地将这些状态信息集成到其他软件或第三方小组件中。

---

## 1. 硬件识别参数 (HID Metadata)

要与鼠标通信，首先需要通过 HID 接口发现并连接设备。MLOONG MX302 鼠标的设备特征如下：

| 参数 | 有线模式 (Wired) | 无线 2.4G 模式 (Wireless) |
| :--- | :--- | :--- |
| **Vendor ID (VID)** | `0xA8A4` (十进制: 43172) | `0xA8A5` (十进制: 43173) |
| **Product ID (PID)** | `0x2255` (十进制: 8789) | `0x2255` (十进制: 8789) |
| **Usage Page** | `0xFF01` (厂商自定义) | `0xFF01` |
| **Usage** | `0x10` (十进制: 16) | `0x10` |

> [!NOTE]
> 必须完全匹配 `VID`、`PID`、`Usage Page` 和 `Usage` 才能保证打开正确的配置通道（普通的输入鼠标通道是无法发送自定义配置指令的）。

---

## 2. 通信协议规范

MLOONG MX302 使用 **64 字节** 的固定长度包进行读写交互。响应包通常以魔数响应头 `0xAA` 或 `0x55` 开头。

在某些系统（如 Windows 平台）的 `hidapi` 读回的字节流中，首字节可能是 Report ID（通常为 `0x00`），从而把真实的协议内容往后推了 1 字节。为了应对这种偏移，我们需要寻找 `0x55` 或 `0xAA` 在数据包中的起始位置，并将其标记为基准偏移 `base`（通常为 `0` 或 `1`）。

### 2.1 获取电量协议

#### 发送：请求包 (GetBattery Request)
构造一个 `64` 字节的字节数组，填充以下特定值，其余字节填充为 `0`：

| 字节偏移 | 字段名称 | 字节值 (十六进制) | 说明 |
| :--- | :--- | :--- | :--- |
| **Byte 0** | 魔数头 (Magic Byte) | `0x55` | 协议固定请求头 |
| **Byte 1** | 命令 ID (Command ID) | `0x30` | 代表 `GetBattery` 命令 |
| **Byte 2** | 公共头高位 | `0xA5` | 协议头部固定值 |
| **Byte 3** | 公共头低位 | `0x0B` | 协议头部固定值 |
| **Byte 4** | 提交标记 | `0x2E` | 固定校验/提交标志 |
| **Byte 5-7**| 填充 | `0x01, 0x01, 0x01` | 固定填充值 |
| **Byte 8-63**| 零填充 (Padding) | `0x00` | 全部填充为 `0` |

#### 接收与解析：响应包
确认响应字节流 `d = &data[base..]` 中 `d[1]` 为 `0x30` 后，提取以下信息：
- **电量百分比**: `d[8]`（单字节，范围 `0` ~ `100`）
- **充电状态**: `d[9]`（若不为 `0` 则表示**充电中 ⚡**，若为 `0` 则表示**未充电**）

---

### 2.2 获取当前 DPI 协议

设备支持双模式 DPI（模式 1: 射击/狙击模式，模式 2: 高速模式）。每一模式有 6 个可用档位。

为了获取最准确的当前 DPI 值，推荐在发起 DPI 查询前，先发送一次状态同步命令。

#### 步骤 A：发送状态同步命令 (可选，推荐)
- **请求包**: `[0x55, 0xED, 0x00, 0x00, ... 0x00]` (64 字节)
- **说明**: 发送此指令后，设备将刷新内部状态，您可以选择读取其响应并丢弃，或直接开始下一步。

#### 步骤 B：发送获取 DPI 请求 (GetDualDpi Request)
构造一个 `64` 字节的请求包：

| 字节偏移 | 字段名称 | 字节值 (十六进制) | 说明 |
| :--- | :--- | :--- | :--- |
| **Byte 0** | 魔数头 (Magic Byte) | `0x55` | 固定请求头 |
| **Byte 1** | 命令 ID | `0x61` | 代表 `GetDualDpi` 读双模式 DPI 命令 |
| **Byte 2-3**| 公共头 | `0xA5, 0x0B` | 固定值 |
| **Byte 4** | 子命令 | `0x1B` (十进制: 27) | 固定值 |
| **Byte 5-7**| 填充 | `0x01, 0x01, 0x01` | 固定值 |
| **Byte 8-63**| 零填充 | `0x00` | 全部填充为 `0` |

#### 步骤 C：接收并解析响应包
确认响应字节流 `d = &data[base..]` 中 `d[1]` 为 `0x61` 或 `0x60` 后，提取以下字段：
1. **当前激活模式**: `d[8]`。`0` 表示模式 2 (高速模式)，`1` 表示模式 1 (射击模式)。
2. **当前激活档位**: `d[10]`。1-based 索引，实际索引为 `(d[10] as usize).saturating_sub(1)`。
3. **DPI 列表数据与转换**:
   设备中存储的是线值，读取后需经过转换：`DPI = round(线值 / 1.173)`。
   - **模式 1 档位数据**: 位于 `d[11..23]` (共 6 个 LE 16 位无符号整数，每档 2 字节)
   - **模式 2 档位数据**: 位于 `d[23..35]` (共 6 个 LE 16 位无符号整数，每档 2 字节)
   - **模式 2 修正**: 模式 2 (高速模式) 的最终 DPI 需四舍五入到最近的百位数，例如 `round(DPI / 100.0) * 100`。
4. 根据当前模式和激活档位索引，获取对应的档位数值即可。

---

## 3. Rust 完整实现代码

### 3.1 Cargo.toml 依赖
```toml
[dependencies]
hidapi = "2.6.3"
```

### 3.2 完整代码 (获取电量与 DPI)
您可以直接运行以下程序，它会先获取鼠标的电量及充电状态，随后获取当前的 DPI 模式、当前档位和具体 DPI 像素数值。

```rust
use std::time::Duration;
use hidapi::{HidApi, HidDevice};

// 硬件识别参数
const VENDOR_IDS: [u16; 2] = [0xA8A4, 0xA8A5]; // 有线 / 无线 2.4G
const PRODUCT_ID: u16 = 0x2255;
const USAGE_PAGE: u16 = 0xFF01;
const USAGE: u16 = 0x10;

// DPI 换算因子
const DPI_SCALE_FACTOR: f64 = 1.173;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 1. 初始化 HID 并查找设备
    let api = HidApi::new()?;
    let mut target_info = None;
    for dev in api.device_list() {
        if dev.product_id() == PRODUCT_ID
            && VENDOR_IDS.contains(&dev.vendor_id())
            && dev.usage_page() == USAGE_PAGE
            && dev.usage() == USAGE
        {
            target_info = Some(dev.clone());
            break;
        }
    }

    let dev_info = match target_info {
        Some(info) => info,
        None => {
            println!("未找到 MLOONG MX302 鼠标，请确认连接状态及占用情况。");
            return Ok(());
        }
    };

    let device = api.open_path(dev_info.path())?;
    println!("成功连接到设备: {:?}", dev_info.product_string().unwrap_or("MX302"));

    // 2. 获取电量
    let (battery_level, is_charging) = get_battery(&device)?;

    // 3. 获取当前 DPI
    let (dpi_mode, active_stage, current_dpi) = get_current_dpi(&device)?;

    // 4. 控制台美化输出
    println!("┌──────────────────────────────────┐");
    println!("│ 鼠标当前状态信息                 │");
    println!("├──────────────────────────────────┤");
    println!("│ 剩余电量: {:<22} │", format!("{}%", battery_level));
    println!("│ 充电状态: {:<22} │", if is_charging { "充电中 ⚡" } else { "未充电" });
    println!("├──────────────────────────────────┤");
    println!("│ DPI 模式: {:<22} │", if dpi_mode == 2 { "高速模式" } else { "射击模式" });
    println!("│ 当前档位: {:<22} │", format!("第 {} 档", active_stage + 1));
    println!("│ 当前 DPI: {:<22} │", format!("{} DPI", current_dpi));
    println!("└──────────────────────────────────┘");

    Ok(())
}

/// 发送 HID 包的通用兼容方法 (支持 Feature Report 及退化到 Write 模式)
fn send_packet(device: &HidDevice, packet: &[u8; 64]) -> Result<(), Box<dyn std::error::Error>> {
    if let Err(_) = device.send_feature_report(packet) {
        let mut write_buf = Vec::with_capacity(65);
        write_buf.push(0x00); // 报告 ID
        write_buf.extend_from_slice(packet);
        device.write(&write_buf)?;
    }
    Ok(())
}

/// 查找有效响应包的协议魔数头部偏移 (base)
fn find_base_offset(buf: &[u8]) -> Option<usize> {
    if buf.is_empty() { return None; }
    if buf[0] == 0x55 || buf[0] == 0xAA {
        return Some(0);
    }
    if buf.len() > 1 && (buf[1] == 0x55 || buf[1] == 0xAA) {
        return Some(1);
    }
    None
}

/// 获取电量及充电状态
fn get_battery(device: &HidDevice) -> Result<(u8, bool), Box<dyn std::error::Error>> {
    let mut packet = [0u8; 64];
    packet[0] = 0x55;
    packet[1] = 0x30; // GetBattery
    packet[2] = 0xA5;
    packet[3] = 0x0B;
    packet[4] = 46;
    packet[5..8].copy_from_slice(&[0x01, 0x01, 0x01]);

    send_packet(device, &packet)?;

    let mut read_buf = vec![0u8; 65];
    let bytes_read = device.read_timeout(&mut read_buf, 1000)?;
    if bytes_read == 0 {
        return Err("读取电量超时")?;
    }
    read_buf.truncate(bytes_read);

    let base = find_base_offset(&read_buf).ok_or("找不到有效响应头")?;
    if read_buf.len() < base + 10 || read_buf[base + 1] != 0x30 {
        return Err("电量响应格式错误")?;
    }

    let level = read_buf[base + 8];
    let is_charging = read_buf[base + 9] != 0;
    Ok((level, is_charging))
}

/// 获取当前 DPI 数据
fn get_current_dpi(device: &HidDevice) -> Result<(u8, usize, u16), Box<dyn std::error::Error>> {
    // A. 发送 MOUSE_STATUS 同步状态 (可选)
    let mut status_packet = [0u8; 64];
    status_packet[0] = 0x55;
    status_packet[1] = 0xED; // MouseStatus
    let _ = send_packet(device, &status_packet);
    let mut dummy = vec![0u8; 65];
    let _ = device.read_timeout(&mut dummy, 100).ok();

    // B. 发送 GetDualDpi 请求
    let mut packet = [0u8; 64];
    packet[0] = 0x55;
    packet[1] = 0x61; // GetDualDpi
    packet[2] = 0xA5;
    packet[3] = 0x0B;
    packet[4] = 27; // 子命令
    packet[5..8].copy_from_slice(&[0x01, 0x01, 0x01]);

    send_packet(device, &packet)?;

    let mut read_buf = vec![0u8; 65];
    let bytes_read = device.read_timeout(&mut read_buf, 3000)?; // 允许较长的超时
    if bytes_read == 0 {
        return Err("读取 DPI 超时")?;
    }
    read_buf.truncate(bytes_read);

    let base = find_base_offset(&read_buf).ok_or("找不到有效响应头")?;
    if read_buf.len() < base + 35 || (read_buf[base + 1] != 0x61 && read_buf[base + 1] != 0x60) {
        return Err("DPI 响应格式错误")?;
    }

    let d = &read_buf[base..];

    // C. 解析模式与激活档位
    let mode_byte = d[8];
    let active_mode = if mode_byte == 0 { 2 } else { 1 }; // 2=高速, 1=射击
    let active_stage = (d[10] as usize).saturating_sub(1); // 1-based 转 0-based 档位索引

    // LE 16 位整数读取
    let read_u16_le = |buf: &[u8], offset: usize| -> u16 {
        (buf[offset] as u16) | ((buf[offset + 1] as u16) << 8)
    };

    // 将设备线值转换为 DPI 值
    let wire_to_dpi = |wire: u16| -> u16 {
        (wire as f64 / DPI_SCALE_FACTOR).round() as u16
    };

    let current_dpi = if active_mode == 1 {
        // 模式 1 DPI，起始偏移为 11，每档 2 字节
        let raw = read_u16_le(d, 11 + active_stage * 2);
        wire_to_dpi(raw)
    } else {
        // 模式 2 DPI，起始偏移为 23，每档 2 字节
        let raw = read_u16_le(d, 23 + active_stage * 2);
        let mut dpi = wire_to_dpi(raw);
        // 模式 2 的 DPI 通常需要进行百位四舍五入对齐
        dpi = (dpi as f64 / 100.0).round() as u16 * 100;
        current_dpi = dpi;
    };

    Ok((active_mode, active_stage, current_dpi))
}
```
