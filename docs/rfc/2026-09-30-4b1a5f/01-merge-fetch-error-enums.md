# Agent Note:合并 update 模块同构的错误枚举 FetchFileError 与 FetchFailure

Status: proposed

## 问题

`src/update/http.rs:273` 定义 `pub(super) enum FetchFileError { Download(String), Local(String), Cancelled }`,`src/update/installer.rs:101` 定义 `pub(super) enum FetchFailure { Download(String), Local(String), Cancelled }`——两者变体名、载荷类型、变体顺序完全一致,是同一事实的两份表示。连接两者的 `src/update/installer.rs:118` `classify_fetch_error` 是逐变体的恒等映射(`FetchFileError::Download(msg) => FetchFailure::Download(msg)`,三个变体全部如此),它存在的唯一理由是让两个同构枚举互相转换。

检索证据(2026-09-30,`grep -rn "FetchFailure|FetchFileError|classify_fetch_error" src/`,共 56 处命中):

- `FetchFileError` 的生产者与消费者全部在 http.rs 抓取路径(`HttpGet::open`、`for_each_chunk`、`fetch_to_file` 写盘失败),以及 installer.rs:203 经 `classify_fetch_error` 转换。
- `fetch_to_file` 的唯一生产调用点是 `src/update/installer.rs:191`(`fetch_verified_installer` 内部);`fetch_url`(`src/update/mod.rs:245`、`src/update/mod.rs:255`,元数据抓取)完全不经过 `FetchFailure`,它把 `FetchFileError` 直接压平为 `String`。
- `FetchFailure` 的唯一模式匹配消费处是 `src/update/mod.rs:324`-`348`(`do_update_check` 的回落代理决策)。
- `classify_fetch_error` 的唯一调用点是 `src/update/installer.rs:203`;唯一测试消费者是 `src/update/installer.rs:436`-`449` 的 `test_cancelled_is_not_classified_as_download`,它断言的内容是恒等映射保持恒等。

## 提案

删除 `FetchFailure` 枚举与 `classify_fetch_error` 函数;`fetch_verified_installer` 的返回类型改为 `Result<VerifiedInstaller, FetchFileError>`,其本地失败(创建/锁定/哈希,`src/update/installer.rs:177`、`186`、`211`、`219`、`225`)直接构造 `FetchFileError::Local`,`src/update/installer.rs:203` 的 `return Err(classify_fetch_error(e))` 改为原样透传 `return Err(e)`;`src/update/mod.rs:35` 的 use 与 `do_update_check` 的 match(`src/update/mod.rs:324`-`348`)改用 `http::FetchFileError`;删除 `test_cancelled_is_not_classified_as_download`;`src/update/http.rs:268`-`272` `FetchFileError` 的文档注释吸收 `src/update/installer.rs:99`-`112` `FetchFailure` 注释里的回落决策要点(「Download 可回落代理、Local 不回落、Cancelled 静默放弃」)。预计净删约 35-40 行(枚举 12 行 + 恒等映射 7 行 + 测试 14 行 + 双份注释合并)。

## 明确不在本次范围

`fetch_url` 的 `Result<Vec<u8>, String>` 压平(`src/update/http.rs:240`-`246` 的 map_err 闭包与两个穷尽臂)不一起动:它的调用方 `do_update_check` 只需要消息字符串进 `CheckResult::Error`,改成透传枚举只是把 match 从一个文件挪到另一个文件,一行代码都不删,还让 `Local`/`Cancelled` 两个在该路径不可达的臂(`src/update/http.rs:242`-`245` 注释已声明是穷尽匹配要求)变成调用方必须处理的真实分支。`SubprocessEnd`(`src/update/mod.rs:394`-`401`)与 `UpdateAction`(`src/update/protocol.rs:39`-`44`)不合并:两者变体集不同(`Busy` vs `Abandoned`),一个是子进程 stdout 协议行动作、一个是子进程收尾方式,语义不同构。`InstallerLaunch`(`src/update/installer.rs:88`-`95`)不动:它描述「启动安装器」这一段的失败归因,与下载/写盘失败是不同接缝,且其 `FailedWithoutCode` 变体没有对应物。

## 为什么不保留?

最强反方一:http 层与 installer 管线层是两个概念层,合并后 http.rs 的枚举承载了 installer 的策略语义,分层被污染。回应:变体名 `Download`/`Local`/`Cancelled` 本身就是回落策略语义,`src/update/http.rs:268`-`272` 的既有文档已写明「调用方按变体映射到回落决策,不要匹配文案猜来源」——策略语义早已在 http.rs,installer.rs 只是复制了一份;真正属于 installer 层的信息(哪些失败算本地失败)由构造点表达,不靠枚举名区分。最强反方二:删除 `test_cancelled_is_not_classified_as_download` 会削弱「Cancelled 不得被归成 Download」的保护。回应:该测试钉的判定对象(`classify_fetch_error`)本身被删;「Cancelled 不回落代理」的真正守卫是 `do_update_check` 的 match 分支(`src/update/mod.rs:343`、`348` 的 `Err(...Cancelled) => CheckResult::Abandoned`),合并后该分支原样保留,而原测试从未覆盖过这个分支——它只测过恒等函数自身,把它换成错误实现(如 `Local` 也映射 `Download`)测试确实会红,但被测函数整个消失后这层保护的对象也不存在了。最强反方三:未来 http 层可能新增变体(如重定向),installer 层不想透传。回应:属推测性产品通用性;真出现时再拆分不迟,且 `do_update_check` 的 match 是穷尽的,新增变体会强制编译期处理,不会静默漏掉。

## 验收标准

`grep -rn "FetchFailure" src/` 与 `grep -rn "classify_fetch_error" src/` 均为 0 命中;`cargo fmt -- --check`、`cargo test --locked`、`cargo build --release --locked`、`cargo clippy --all-targets --locked -- -D warnings` 四条门禁全部通过;除 `test_cancelled_is_not_classified_as_download` 被删外,既有测试一个不删不改——特别是 `src/update/http.rs` 的 `test_winhttp_error_code_mapping`、`src/update/installer.rs` 的 `test_transient_launch_errors_are_retried`/`test_permanent_launch_errors_are_not_retried`/`test_cached_reuse_accepts_matching_locked_content`/`test_cached_reuse_rejects_tampered_content` 照常通过;`do_update_check` 三分支语义逐行对照不变:`Download` 回落代理(`src/update/mod.rs:324`)、`Local` 直接 `CheckResult::Error`(`src/update/mod.rs:346`)、`Cancelled` 静默 `CheckResult::Abandoned`(`src/update/mod.rs:343`、`348`)。

## 风险

`src/update/mod.rs` 与 `src/update/installer.rs` 的引用改写若漏一处会编译失败——由编译器穷尽检查兜底,可证伪、不会静默出错。合并后 `FetchFileError` 的文档注释需同时服务两个调用方(`fetch_url` 压平为 String、`fetch_verified_installer` 透传枚举),若注释写得含糊会误导后来者;这是文档层面的残余风险,不是行为层面的——两个调用方的行为由类型系统与既有测试钉住。
