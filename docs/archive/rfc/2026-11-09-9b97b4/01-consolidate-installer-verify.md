# Agent Note：合并安装包锁定重验的两份实现，让缓存复用共用同一套瞬态重试

Status: implemented

## 问题

「加只读共享锁 → 对锁定句柄重算哈希 → 与期望值比对 → 构造持锁体」这件事在 `src/update/installer.rs` 里有两个包装入口，核心已经收口在 `verify_locked_installer`（`src/update/installer.rs:131`-`src/update/installer.rs:149`），但两个包装层各自决定「打开失败怎么办」，而决定是相反的：`try_reuse_cached_installer`（`src/update/installer.rs:155`-`src/update/installer.rs:161`）用 `.ok()` 把一切失败折叠成「缓存不可用」，`reverify_installer_for_handoff`（`src/update/installer.rs:171`-`src/update/installer.rs:192`）对 `VerifyFailure::Open` 里的瞬态共享错误做有限次重试（合计 `INSTALLER_LAUNCH_MAX_ATTEMPTS` 次尝试，即最多重试 2 次）、每次间隔 `INSTALLER_LAUNCH_RETRY_DELAY_MS`。

生产消费者两处：更新检查路径调缓存复用（`src/update/mod.rs:311`-`src/update/mod.rs:317`），安装交接副本路径调重验（`src/update/mod.rs:447`-`src/update/mod.rs:450`）。非生产消费者：`src/update/installer.rs:826`-`src/update/installer.rs:839`、`src/update/installer.rs:841`-`src/update/installer.rs:856`（复用侧两条），`src/update/installer.rs:695`-`src/update/installer.rs:717`（只钉归因，不跑重试）。

分歧的后果是可推演的，不是风格问题：缓存文件被杀软实时扫描以写方式持有时，`open_locked_installer` 以 `ERROR_SHARING_VIOLATION` 失败，`.ok()`（`src/update/installer.rs:160`）把它折叠成「缓存不可用」，`src/update/mod.rs:313` 的判断落空，于是走下载路径——先 `remove_file`（`src/update/installer.rs:215`），再 `create_locked_installer` 的 `create_new` 撞上同一个占用者，只重试一次就返回 `FetchFileError::Local("创建安装包文件失败")`（`src/update/installer.rs:219`-`src/update/installer.rs:238`）。一次本可复用缓存的检查于是变成一次报错加弹框。判据 `is_transient_sharing_error`（`src/update/installer.rs:473`-`src/update/installer.rs:476`）与重试常量都在同一个文件里，只差没有共用。

触发概率按代码路径核下来偏低：能进入复用分支的缓存文件来自一次完整下载、且已通过哈希校验，通常已经落盘数分钟以上，而实时扫描多发生在写入期与执行期。所以本条的正当性主要落在「同一事实只有一份表示」，不是「救回一次更新」。

## 提案

1. 保留 `verify_locked_installer` 为唯一实现，不动它的不变量：先加锁再对锁定句柄哈希、验的就是将要持有的同一个句柄、不探 `exists()`。
2. 把重试从 `reverify_installer_for_handoff` 里提出来，做成唯一包装 `verify_installer_with_retry(path, version, expected_hash)`：`Content` 立即返回 `None`（终局，重试无意义）；`Open` 且 `is_transient_sharing_error` 且未达 `INSTALLER_LAUNCH_MAX_ATTEMPTS` 则睡 `INSTALLER_LAUNCH_RETRY_DELAY_MS` 重试；其余返回 `None`。
3. 两个调用方都改调它，差别只剩 `version`：缓存复用传 `latest_version`（该路径会把它写进确认框文案与「跳过此版本」记录），交接副本传空串（副本路径无确认框，`version` 不被消费——`src/update/mod.rs:410` 起的 `install_handoff_main` 全程没有读 `VerifiedInstaller::version`）。这条差异写进函数注释，避免后来人把空串当遗漏。
4. 删掉 `try_reuse_cached_installer`、`reverify_installer_for_handoff` 两个名字，或让它们退化成一行转发。

行为变化只有一处且是本提案的目的：缓存复用现在会为瞬态打开失败等待最多 2 × 400ms，而不是立刻删文件重下。

## 明确不在本次范围

- **不合并 `InstallerLaunch::FailedWithoutCode`**（`src/update/installer.rs:114`、`src/update/installer.rs:541`-`src/update/installer.rs:547`、`src/update/installer.rs:661`-`src/update/installer.rs:667`）。外部评审把它概括成「只为一句文案而存在」，不准确：它有两个生产者，除「HRESULT 取不到裸 Win32 码」外，还有 `src/update/installer.rs:644`-`src/update/installer.rs:647` 的「`ShellExecuteExW` 返回成功但未提供进程句柄」——后者根本没有错误码可言。折叠成 `Failed(Option<u32>)` 会让那条路径上已有的准确日志（`src/update/installer.rs:645`）与紧随其后那条「Err 不带 Win32 错误码」（`src/update/installer.rs:545`）继续互相矛盾。要动先单独修文案，再谈合并。
- **不给交接副本 exe 补锁定**（`src/update/installer.rs:447`-`src/update/installer.rs:448`）。已逐行复核属实：`remove_file` → `copy` → `spawn` 全按 `cache.rs` 的固定路径（`src/update/cache.rs:35`-`src/update/cache.rs:37`，落在用户可写的 `%LOCALAPPDATA%`），中间没有句柄锁，同用户进程可以在 copy 与 spawn 之间替换副本。但那是**新增防护**而非删减，且修复手法与 `src/update/installer.rs:596`-`src/update/installer.rs:599`「持读锁启动安装包」同法，需要独立笔记与独立的验收面。
- 不动 `fetch_verified_installer` 的 `create_new` 争用重试（`src/update/installer.rs:216`-`src/update/installer.rs:238`）：它面对的是「上一次删除被占用挡下」这一种成因，与重验的打开失败不是同一场景，合并会引入新的判别负担。

## 为什么不保留？

- **反方一：重试是交接路径的专属需求，缓存复用不该跟着等。** 回应：两边撞到的占用者同类（刚被写入的文件被实时扫描持有），而 400ms 的等待代价远小于整包重下的流量加随后的失败面。若确实要区分，区分点是「值不值得等」，那就该在唯一包装里用一个参数表达，而不是复制一份实现——现在的分叉正是「新加一个瞬态码只改一处」这条纪律（同文件 `src/update/installer.rs:466`-`src/update/installer.rs:468` 已经明写）被绕开的入口。
- **反方二：两份包装各三行，合并省不了多少。** 回应：省的不是行数，是判据的解释权。「什么算瞬态」现在有两处解释，一处全部折叠、一处按码集判定，将来只会再次漂移。
- **反方三：缓存复用失败会重下，不算 bug。** 回应：重下的第一步 `create_new` 撞的是同一个占用者，且只重试一次；失败面是「本次检查直接报错并弹框」，不是「慢一点」。
- **反方四：`.ok()` 至少保证了「复用永不阻塞」。** 回应：复用发生在更新工作线程（`spawn_update_worker`，`src/update/mod.rs:155`-`src/update/mod.rs:165`），不在 UI 线程，阻塞 800ms 对界面无影响。

## 验收标准

- `grep -rn "try_reuse_cached_installer\|reverify_installer_for_handoff" src` 只剩测试或零命中。
- `grep -rn "verify_installer_with_retry" src/update` 命中实现 1 处、调用 2 处（原 `src/update/mod.rs:314` 与 `src/update/mod.rs:447` 位置）。
- `src/update/installer.rs:695`-`src/update/installer.rs:717`、`src/update/installer.rs:826`-`src/update/installer.rs:856` 三条现有用例保持绿，断言不许放宽：内容不符仍必须终局、不重试。
- 新增纯函数级用例钉住重试上限（`INSTALLER_LAUNCH_MAX_ATTEMPTS` 次）与「`Content` 不重试」；若不可在不真跑文件的情况下覆盖，允许只覆盖归因分支并在用例注释里说明缺口。
- 四道门禁全绿：`cargo fmt -- --check`、`cargo test --locked`、`cargo build --release --locked`、`cargo clippy --all-targets --locked -- -D warnings`。

## 风险

- 重试循环里没有复判 `UpdateContext::abandoned()`（`src/update/mod.rs:319` 的 `still_wanted` 只作用于下载路径）。残留风险真实存在但可接受：最多 800ms 的空等，期间不发起网络动作、不写盘、不弹框。若要更严，把 `still_wanted` 一并传进包装。
- 缓存复用路径的 `version` 与交接路径的空串在同一函数里并存，是把「参数只服务会弹框的路径」这件事写进了调用点的隐式约定。若将来给交接路径补确认框，空串会变成用户可见的空版本号。缓解只能靠注释。
- 本改动不改变缓存文件的删除时机（复用失败后仍由下载路径 `src/update/installer.rs:215` 删除），因此不引入新的残留窗口。
