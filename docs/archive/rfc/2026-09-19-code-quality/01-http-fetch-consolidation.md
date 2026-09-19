# Agent Note：合并 HTTP 抓取重复建连流程

Status: proposed

## 问题

`src/update/http.rs` 的 `fetch_url`（`src/update/http.rs:64-235`）与 `fetch_to_file`（`src/update/http.rs:252-417`）各自完整实现了一遍“WinHttpOpen → SetTimeouts → Connect → OpenRequest → Send → Receive → QueryHeaders 验 200”建连流程，连 `15000` 四段超时（`src/update/http.rs:104,289`）都是各存一份，将来改超时、代理或重定向策略时必然漏改一处，且漏改只影响版本检查或安装包下载中的一条路径，极难发现，这是全仓唯一一处会主动繁殖分叉行为的复制粘贴（P1 缺陷级）。

## 提案

抽“已验证 200 的 GET 响应”类型并收敛读循环，两函数只保留消费差异：新增 `struct HttpGet { handles: WinHttpHandles }`，`HttpGet::open(host, path)` 唯一实现建连到验 200 全序列（含 `WinHttpSetTimeouts`，超时值引用 `config.rs` 新增的 `HTTP_TIMEOUT_MS: i32`，两处 `15000` 就此归一），`for_each_chunk(max_bytes, on_chunk)` 唯一实现上限约束、分块读取与长度防御，`on_chunk` 回调分别由 `fetch_url`（收集进 `Vec`）与 `fetch_to_file`（哈希加写盘）提供；`fetch_url` 保持 `Result<Vec<u8>, String>` 签名不变（`Download/Local → String` 只在边界映射），`fetch_to_file` 的 `FetchFileError::Download/Local` 分类逐 arm 原样保留，`do_update_check` 的主源失败回落代理领域逻辑一字不动。

## 明确不在本次范围

`update/mod.rs` 内 `do_update_check` 的回落决策、任何重定向策略变更、`network.rs:80` 的 `GetIfTable2` 整表语义（G3 已定论不建议动）、`opt-level = "z"`（G5 保持）都不动；`WinHttpHandles` 的 RAII 释放顺序不重排，只换属主（`HttpGet` 持有）；`tray.rs:238` 的 `to_string_lossy` 属路径面，归 02 号 RFC，不在此顺手。

## 为什么不保留？

最强反方是“两函数错误类型不同（`String` vs `FetchFileError`），强行合并会污染分类”。回应：分类点不在建连段而在消费段，建连段错误在两函数里今天已是同一集合（初始化、建连、发送、接收、状态码、查询、读取、超限），统一返回 `FetchFileError::Download` 再由 `fetch_url` 在边界一次性转 `String`，分类信息零损失；次强反方是“回调闭包妨碍内联”，回应：`impl FnMut` 单态化后与今天的手写循环同一形态，调用序列与分配次数不变，性能中性。

## 验收标准

`cargo test --locked`（含既有 `test_winhttp_error_code_mapping` 与全部下载错误映射测试，迁移时原样平移）、`cargo build --release --locked`、`cargo clippy --all-targets --locked -- -D warnings`、`cargo fmt` 全绿；`grep -n "WinHttpOpen\|WinHttpSetTimeouts\|WinHttpQueryHeaders" src/update/http.rs` 建连关键字只剩 `HttpGet::open` 内一份实现；`grep -rn "15000" src/update/http.rs` 零命中且 `grep -n "HTTP_TIMEOUT_MS" src/config.rs` 命中定义；`fetch_url`/`fetch_to_file` 签名与 `do_update_check` 调用点零改动（`git diff` 不含 `update/mod.rs` 回落逻辑）。

## 风险

残留风险是消费段错误归类在合并时贴错 `Download/Local`（`Local` 误标 `Download` 会触发无意义的代理重试浪费整包流量，反之则丢一次重试机会）。证伪：按现有测试逐 arm 对照，`Sha256` 与文件写入相关全部标 `Local`，其余标 `Download`，diff 逐行评审；本 RFC 独立单 PR 加设计评审，不与 02/03 号 RFC 同 PR。
