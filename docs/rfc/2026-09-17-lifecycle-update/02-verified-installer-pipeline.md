# Agent Note：安装包流水线改为流式写入加锁后重验

Status: proposed

## 问题

当前安装包流水线分两段都有“校验对象”与“启动对象”不是同一句柄的缺口。缓存路径 `src/update/mod.rs:312` 先按路径 `compute_sha256_hex_file(&temp_path)` 算哈希，再另行 `open_locked_installer(&temp_path)` 取只读锁，两次打开之间文件可被替换；下载路径 `src/update/mod.rs:341` 对内存 `installer_data` 算哈希、写入文件、`src/update/mod.rs:379` 关写句柄后再重开只读锁，关与开之间是无锁窗口，后取的锁只能阻止之后的修改，不能自证锁住的仍是之前校验的内容。同时 `src/update/http.rs:181` 的 `fetch_url` 把整包读进 `Vec`（上限 `src/config.rs:59` 的 256MiB，每轮还 `vec![0u8; chunk_len]` 分配一次临时缓冲），`src/update/mod.rs:328` 再整体哈希加整体写入，峰值内存随安装包大小线性增长，而 `src/update/crypto.rs:32` 的 `Sha256::update` 明明已支持增量喂数却未被利用。检索证据：`Select-String -Pattern "compute_sha256_hex" src/update` 命中缓存校验、下载校验与 `crypto.rs:75`、`crypto.rs:81` 定义共 4 处，确认哈希能力集中但调用点都是“一把梭”；`Select-String -Pattern "fetch_url" src/update` 命中元数据（4KiB）与安装包（256MiB）共用同一内存返回函数，确认大小两种负载被迫走同一路径。

## 提案

把“下载→校验→持锁→启动”收敛为同一句柄流水线，生产消费者为 `do_update_check`（`src/update/mod.rs:263`）与 `launch_installer`（`src/update/mod.rs:686`，持锁到最后一次尝试结束）；非生产消费者为 `crypto.rs:114` 下的 `sha256_known_answer` 与增量一致性单测（行为不得变）。具体改动：安装包新增流式抓取入口（固定缓冲如 64KiB 复用，边 `WinHttpReadData` 边 `Sha256::update` 边写临时文件，全程累计字节数上限沿用 `INSTALLER_MAX_BYTES`，不只信 `Content-Length`），元数据（`VERSION_METADATA_MAX_BYTES`）保留现有内存 `fetch_url` 不动；文件落盘仍用 `create_new(true)` 加 `FILE_SHARE_READ_ONLY` 语义，写完降级为只读锁后，必须对该已锁定句柄重新计算一次哈希（需给 `crypto` 新增以 `File` 或句柄为输入的增量哈希入口，禁止再按路径另开文件做“最终验证”），校验成功才构造 `VerifiedInstaller` 并持锁至 `ShellExecuteExW` 返回。`http.rs` 循环内的每轮分配改为复用缓冲。

## 明确不在本次范围

发布者身份认证（签名元数据、安装器 Authenticode 校验）不在本次范围，本篇只解决完整性绑定的 TOCTOU，不声称解决来源认证；代理回落策略（主源加 `ghproxy.cn`）与重试次数不在本次范围；退出与重建的 HWND 稳定化不在本次范围（另见 `01-stable-control-window`，但本篇的最终锁句柄在重建期间不受 HWND 影响，两篇可独立合入）；不改 `INSTALLER_MAX_BYTES` 数值本身。

## 为什么不保留？

最强的反方是“256MiB 只是上限非常态，整包内存简单可靠，TOCTOU 需要本地写权限才可利用，威胁有限”。逐条回应：峰值内存的受害者正是短生命周期的更新子进程（主进程隔离设计的初衷就是不让网络加解密常驻，子进程峰值随包增大而增大与该初衷相悖，流式改动主要删分配次数而非省常驻，收益明确且局部）；TOCTOU 的利用门槛确实要求本地写入 `%LOCALAPPDATA%\Traffic Monitor`，但修复成本只是一次“锁后重读哈希”（已有增量能力，无新依赖），属于低成本纵深防御，且没有它，持锁的证明力文案（`VerifiedInstaller` 注释自称“拒绝其他进程改写已校验文件”）就是名不副实。第二个反方是“只加锁后重验、不做流式”。不采纳的理由：只要还保留整包 `Vec`，流式就是早晚要还的债，而流式的增量哈希输出正好就是锁后重验需要的输入，拆开做会写两遍哈希管线。

## 验收标准

`Select-String -Pattern "fetch_url" src/update/mod.rs` 在安装包下载调用点不得再出现整包 `Vec` 变量（如 `installer_data`）参与哈希与写入；`Select-String -Pattern "VerifiedInstaller" src/update/mod.rs` 的构造点上游 20 行内必须有对锁定句柄的哈希调用。现有测试必须全过：`cargo test --locked`（点名 `sha256_known_answer`、`sha256_incremental_matches_one_shot`、`transient_launch_errors_are_retried`），`cargo clippy --all-targets --locked -- -D warnings`，`cargo fmt -- --check`。新增单测：以本地大输入驱动流式与整包两种哈希入口结果一致；以“校验后替换文件内容”的模拟（锁后重验前篡改）断言构造被拒绝。人工验证：一次完整下载更新包流程的任务管理器峰值工作集显著低于改前（同一包体对比，不承诺具体数值，只要求趋势向下且安装成功）。

## 风险

流式写入把“下载失败、哈希失败、写入失败、加锁失败”四种错误交织在同一循环，残留风险是错误文案变粗（用户分不清是网络还是磁盘）与半写临时文件残留。缓解是保留现有中文错误映射的 `op` 区分（抓取/哈希/写入/锁定四段各自映射），且失败路径沿用现有“删临时文件”语义；若任一现有 `crypto` 单测改行为或杀软占用下的瞬态重试（`is_transient_launch_error` 覆盖 32/33）被破坏，即判定失败。
