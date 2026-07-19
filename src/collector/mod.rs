//! 系统数据采集。
//!
//! - [`cpu_mem`]：CPU 与内存使用率采集。
//! - [`network`]：网卡流量采样、虚拟网卡黑名单、断网/恢复判定。
//! - [`rate`]：速率归一化与单网卡选择（纯函数，无 I/O）。

mod cpu_mem;
mod network;
mod rate;

pub use cpu_mem::{collect_cpu, collect_memory};
pub use network::{collect_network, init_network_listener};
