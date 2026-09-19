# Traffic Monitor 代码质量深度审查报告

**审查日期**：2026-09-19
**审查范围**：`src/` 全部 17 个源文件、`build.rs`、`Cargo.toml`、`AGENTS.md` 工程约定（交叉核对）
**审查人视角**：高级 Rust / Win32 工程师，以「任何人阅读都应觉得逻辑严密、维护舒服、性能与内存不打折」为终极标准
**客观基线**（本次实测）：
- `cargo clippy --all-targets --locked -- -D warnings` → **零警告**
- `cargo test --locked` → **76 passed, 0 failed**

> **先说结论**：这个代码库的**架构与不变量设计已是高级水准**，真正残留的"低级痕迹"不在 Clippy 能看见的地方，而在五类工具抓不到的问题：**复制粘贴（http.rs 约 110 行）、编码健壮性（`env::args()`/`to_string_lossy`）、常量散落（违反项目自身的 config.rs 集中规则）、类型表达力（裸 `u32`、裸元组、七参函数）、状态组织（4 个静态量表达 1 个概念）**。以下全部问题均有具体位置、具体改法，且**每一条对性能/内存均为中性或更优**。

---

## 1. 哪些已经是高级水准——审查中确认「禁止倒退」的部分

修复任何问题前，先明确这套资产的价值，防止后续改动者以"整洁"之名破坏正确性：

| # | 设计 | 位置 | 为什么是高级水准 |
|---|------|------|------------------|
| 1 | 看门狗窗口 + 退出幂等门 + 更新交接语义 | `main.rs` | 每条消息路由都注释了「为什么不能转发、为什么落点必须是看门狗」，`claim_exit_request` 把幂等性做成纯函数并可测 |
| 2 | `EMBEDDED` 单点真值 + `reembed_if_lost` 周期兜底 | `window.rs` | 精准回答了 TaskbarCreated 只广播一次导致的永久消失风险，失败路径静默重试避免了弹窗风暴 |
| 3 | 事务式 GDI 构造 + `into_raw` 所有权移交 | `renderer.rs` | 任何一步失败由局部 RAII 守卫自动清理，成功后 `mem::forget` 移交，Drop 里还原 stock 对象的标准释放序 |
| 4 | TOCTOU 加固的安装包校验 | `update/mod.rs` | 「锁句柄上重算哈希、写锁降级只读锁、错误分类决定是否回落代理」三层防护，配篡改回归测试 |
| 5 | 速率归一化的 u128 中转 + 防虚假峰值 | `rate.rs` | 回避了「先截后除」在大流量+长间隔组合下的低估，注释里写明了溢出分析 |
| 6 | 内存卫生体系 | `build.rs` / `util.rs` | DELAYLOAD 隔离 DLL、EcoQoS 只给子进程、低内存优先级给主进程、`trim_working_set` vs `compact_and_trim` 的语义区分 |
| 7 | 测试文化 | 全部测试模块 | 每个用例都注释「这条用例钉死什么不变量、为什么别的写法测不出来」，是钉不变量而非刷覆盖率 |
| 8 | 热路径零分配纪律 | `renderer.rs` | `self.buf` 复用 + `write_u32` 手写整数格式化 + `LAST_RENDERED_VALUES` 变化门控，空闲时零 GDI 绘制 |

**结论：以下所有建议都不触碰这些机制的核心逻辑，只做外围收敛。**

---

## 2. 问题总览表

> **评级定义（v1.1 新增，回应外部评审）**：**P1 = 缺陷**（不修则在某条路径上出错：panic、路径损坏、行为分叉）；**P2 = 高收益维护项**（不改不会错，但持续付利息）；**P3 = 可选 / 收益边际**。严重度与第 9 节的执行顺序是两个独立维度——先做什么取决于风险/收益比，不取决于严重度标签。

| 编号 | 位置 | 维度 | 级别 | 一句话 |
|------|------|------|------|--------|
| A1 | `main.rs:111` | 健壮性 | **P1** | `env::args()` 遇非法 Unicode 直接 panic |
| B1 | `update/http.rs:64-235 / 252-417` | DRY | **P1** | `fetch_url` 与 `fetch_to_file` 约 110 行建连流程复制粘贴，全项目最重单项 |
| A2 | `update/mod.rs:93,623,838` | 健壮性 | **P2** | 路径经 `to_string_lossy()` 静默替换，非 UTF-8 路径下安装器启动失败（低概率、失败可见可恢复） |
| B2 | `window.rs:190-194`、`renderer.rs:431-434` | DRY | **P2** | DPI 缩放数学三处独立实现 |
| B3 | `main.rs` + `window.rs` 共 8 处 | DRY | **P2** | `HWND ↔ isize` 裸转换样板重复，应收敛为 `AtomicHwnd` newtype |
| B4 | `suspend.rs:222-233 / 270-285` | DRY | **P2** | `check_fullscreen` 里全屏退出边沿处理写了两遍 |
| C1 | `http.rs:104,289` 等 | 工程约定 | **P2** | 15000ms 超时、500ms 重试、64KB 栈等**可调域常量**散落（范围见第 5 节 v1.1 收窄） |
| D2 | `update/mod.rs:709-744` | 表达力 | **P2** | `scan_subprocess_protocol` 返回 4 元组裸布尔，与旁边 `SubprocessOutcome` 结构体的做法自相矛盾 |
| D4 | `main.rs:111-121,220` | 表达力 | **P2** | CLI 参数 4 次线性扫描 + 字符串比较，无单一事实来源 |
| D3 | `window.rs:54-63` | 表达力 | **P2** | `create_window` 七参数 + `#[allow(too_many_arguments)]`——allow 是对坏味道的官方承认 |
| E1 | `cpu_mem.rs:8-11` | 状态组织 | **P2** | 4 个静态原子量表达「CPU 基线」这一个概念，且内存序注释与现实（同线程）不符 |
| E3 | `main.rs:110-267` | 模块组织 | **P2** | `main()` 155 行承担 9 个阶段的编排 |
| A3 | `renderer.rs:562-564`、`tray.rs:58-60` | 健壮性 | **P2** | 定长数组截断复制不保证尾 NUL，不变量靠「当前字符串恰好很短」隐式成立 |
| A4 | `tray.rs:63-66` | 健壮性 | **P2** | `NIM_ADD`/`NIM_SETVERSION` 失败被吞，托盘菜单可能静默失效 |
| E2 | `network.rs:88-131` | 状态组织 | **P2·暂缓** | 三层嵌套 `with_` 闭包，收敛为单一 `SamplerState` 前须先证明无嵌套借用（见该节前置条件） |
| D1 | `state.rs:15-19` | 表达力 | **P3** | 暂停原因用裸 `u32` 位集——3 个标志位配 newtype 的成本高于收益（v1.1 降级） |
| E4 | `main.rs:235-252` 等 | 整洁 | **P3** | 消息循环内全限定路径重复出现 5 次；`handle_timer` 中一个 arm 用 match guard、兄弟 arm 用函数体 if，风格不一致 |
| A5 | `renderer.rs:443-455` | 健壮性 | **P3** | `update_dpi` 半失败状态：窗口已按新 DPI 改尺寸但位图保持旧尺寸 |
| F1 | 全局 | 可维护性 | **P3** | 数十处 `let _ =` 静默失败，无诊断通道（v1.1 方案重做为 OutputDebugStringW） |
| F2 | `Cargo.toml` | 工程化 | **P3** | 缺 `rust-version`（edition 2024 ⇒ ≥1.85）与 `[lints]` 表 |
| G1-G4 | `renderer.rs` | 性能 | **P3** | 微优化项（详见第 7 节，多数收益极小，列为完整性） |

---

## 3. 健壮性问题（A 类）

### A1【P1】`std::env::args()` 在非法 Unicode 参数上 panic

`main.rs:111`：

```rust
let args: Vec<String> = std::env::args().collect();
```

`env::args()` 的文档行为是：**任一参数非合法 Unicode 即 panic**。这个程序是 GUI 子系统 + 接受 `--quit` / `--check-update` 的 CLI 入口，安装器、脚本、用户手工传参都会到达这里。一个含非 UTF-16 可表示字符的无关参数（例如从旧代码页路径拖拽产生的参数）就能让进程带着 panic 退出——对一个"常驻任务栏、更新流程依赖 CLI 协议"的程序，这是必修项。

**改法**（与其他 A/D 类问题合并解决，见 D4 的 `CliArgs`）：

```rust
struct CliArgs {
    quit: bool,
    check_update: bool,
    manual: bool,
    relaunched_by_update: bool,
}

impl CliArgs {
    fn parse() -> Self {
        let mut out = Self {
            quit: false,
            check_update: false,
            manual: false,
            relaunched_by_update: false,
        };
        // args_os 不 panic；用 OsStr 比较，无需合法 Unicode。
        for arg in std::env::args_os().skip(1) {
            if arg == *std::ffi::OsStr::new("--quit") {
                out.quit = true;
            } else if arg == *std::ffi::OsStr::new("--check-update") {
                out.check_update = true;
            } else if arg == *std::ffi::OsStr::new("--manual") {
                out.manual = true;
            } else if arg == *std::ffi::OsStr::new(RELAUNCHED_BY_UPDATE_ARG) {
                out.relaunched_by_update = true;
            }
        }
        out
    }
}
```

收益：panic 消除；`--quit` 与 `--check-update` 同时出现时的优先级从「散落在 if 顺序里的隐式知识」变成 `main()` 开头一处显式判断；`RELAAUNCHED` 检查不再第三次扫描向量。**性能：启动期一次性，零影响。**

### A2【P2】路径处理经 `to_string_lossy()`，非 UTF-8 路径下静默损坏

三处：

- `update/mod.rs:623`（`relaunch_main_app`）：`to_wide(&exe.to_string_lossy())`
- `update/mod.rs:838`（`try_launch_installer`）：`let path_str = path.to_string_lossy();`
- `update/mod.rs:93-94`（`get_temp_installer_path`）：`std::env::var("LOCALAPPDATA")`

`to_string_lossy()` 把无法解码的路径字符替换成 U+FFFD。用户名含旧代码页字符（Windows 上真实存在）时：安装包写到「正确」路径、却以「被替换字符的路径」传给 `ShellExecuteExW`，**启动安装器失败，且错误码完全看不出原因**。`std::env::var` 同理，非法 Unicode 的环境变量会返回 `Err` 而走 temp_dir 回退——LOCALAPPDATA 明明存在却当不存在。

**改法**：Windows 上 `OsStr` 可无损转宽字符，`util.rs` 增加与 `to_wide` 并列的入口：

```rust
use std::os::windows::ffi::OsStrExt;

/// OsStr → NUL 结尾 UTF-16，无损（不走 lossy 替换）。路径类参数的统一入口。
pub fn os_to_wide(s: &std::ffi::OsStr) -> Vec<u16> {
    let mut v: Vec<u16> = s.encode_wide().collect();
    v.push(0);
    v
}
```

`get_temp_installer_path` 改用 `env::var_os` + `PathBuf::from(&OsString)`。**性能：分配次数不变，只是从「有损 String 中转」变「无损直转」。**

（评级说明：v1.1 定为 P2——触发需要路径含不可解码字符，现代 Windows 上概率很低，且失败模式是可见的错误弹窗而非崩溃或数据损坏。）

### A3【P2】定长数组截断复制不保证尾 NUL——不变量靠巧合

`renderer.rs:562-564`（字体名 → `lfFaceName[32]`）与 `tray.rs:58-60`（tip → `szTip[128]`）：

```rust
let copy_len = font_name.len().min(lf.lfFaceName.len());
lf.lfFaceName[..copy_len].copy_from_slice(&font_name[..copy_len]);
// SAFETY: lfFaceName 含尾 NUL；...  ← 注释这么写，但代码并不保证
```

尾 NUL 成立的唯一原因是「"Segoe UI" 只有 9 个字符」。注释声明了代码没有强制的不变量——这是典型的「文档与实现漂移」缺口。改成让代码兑现注释：

```rust
// 需要完整放入 + 1 个尾 NUL；放不下就截断到 31 并手动补 NUL。
let copy_len = (font_name.len() + 1).min(lf.lfFaceName.len()) - 1;
lf.lfFaceName[..copy_len].copy_from_slice(&font_name[..copy_len]);
lf.lfFaceName[copy_len] = 0;
```

tip 同理。**性能：零变化。**

### A4【P2】托盘图标注册失败被完全吞掉

`tray.rs:63-66`：`NIM_ADD` 与 `NIM_SETVERSION` 的返回值都被 `let _ =` 丢弃，之后无条件把 `nid` 存入 `TRAY_DATA`。两个后果：

1. `NIM_ADD` 失败时（Explorer 忙、通知区满）图标不存在，但状态机以为存在；
2. `NIM_SETVERSION` 失败时回调仍是 v0 语义——右键发的是 `WM_RBUTTONUP` 而非 `WM_CONTEXTMENU`，**菜单静默失灵**，用户无任何感知，也没有任何日志。

最小改法：`create_tray_icon` 返回 `bool`（或至少在 `NIM_SETVERSION` 失败时重试一次 v0 兼容路径），失败时不写入 `TRAY_DATA`（`remove_tray_icon` 对不存在的图标做 `NIM_DELETE` 无害）。若与 F1 的诊断日志联动，此处应有一条日志。

### A5【P3】`update_dpi` 半失败态

`renderer.rs:443-455`：位图或字体创建失败时直接 `return`，保持旧尺寸。但调用方 `WM_DPICHANGED` → `embed_in_taskbar` 会按新 DPI 改窗口物理尺寸，结果是「窗口新尺寸 + 位图旧尺寸」的 BitBlt 部分覆盖，边缘露出色键底色。概率极低（GDI 内存耗尽才触发），列 P3：可在失败时**回滚 DPI 缩放前的窗口尺寸**（即维持旧尺寸嵌入）保证两者一致，代价是跨屏后组件物理大小暂不合身，由下个成功周期自愈。

---

## 4. DRY 问题（B 类）——「低级感」最重的来源

### B1【P1】`fetch_url` 与 `fetch_to_file`：约 110 行建连流程复制粘贴

`http.rs` 两个函数各自完整实现了一遍「User-Agent → WinHttpOpen → SetTimeouts → WinHttpConnect → WinHttpOpenRequest → SendRequest → ReceiveResponse → QueryHeaders 验 200」，连 SAFETY 注释都是复制后微调。之后 `do_update_check` 在更高层又为「主源失败回落代理」复制了一遍调用（`update/mod.rs:343-371`，这段是合理的领域逻辑，不算重复）。

这是整个代码库里**唯一一处一眼看过去就像两个人各写一半的代码**。它的危害不是行数，而是：将来改超时、改代理策略、加重定向处理时必须改两处，漏一处就是只影响安装包下载（或只影响版本检查）的隐性分叉。

**改法**：抽一个"已验证 200 的响应"类型，读循环做成回调消费：

```rust
/// 已建立并校验过状态码 200 的 GET 响应。句柄由 WinHttpHandles RAII 托管。
struct HttpGet {
    handles: WinHttpHandles,
}

impl HttpGet {
    fn open(host: &str, path: &str) -> Result<Self, FetchFileError> {
        // WinHttpOpen → SetTimeouts → Connect → OpenRequest → Send →
        // Receive → QueryHeaders(200)：唯一实现，两处消费。
    }

    /// 读循环骨架：上限约束、分块读取、长度防御在此唯一实现。
    /// on_chunk 分别由 fetch_url（收集）与 fetch_to_file（哈希+写盘）提供。
    fn for_each_chunk(
        &mut self,
        max_bytes: usize,
        mut on_chunk: impl FnMut(&[u8]) -> Result<(), FetchFileError>,
    ) -> Result<(), FetchFileError> {
        // WinHttpQueryDataAvailable / WinHttpReadData 循环。
    }
}
```

`fetch_url` 的错误类型 `Result<Vec<u8>, String>` 在边界处统一映射（`Download/Local → String`），外部签名不变，`do_update_check` 无需任何改动。**性能：调用序列与分配完全一致，纯结构收敛。** 建议迁移时保留两个函数现有的全部单测（错误码映射测试直接平移）。

### B2【P2】DPI 缩放数学三处独立实现

- `window.rs:190-194`：`scale = dpi / 96.0`，对 `DISPLAY_WIDTH/HEIGHT/GAP` 缩放；
- `renderer.rs:431-434`：同一公式，对 `DISPLAY_WIDTH/HEIGHT/FONT_BASE_SIZE` 缩放；
- `renderer.rs:516-520`：`Layout::new` 用 `width / DISPLAY_WIDTH` **反推** scale。

三处一旦有一处改舍入策略（比如 `.round()` 改 `ceil()`），就会出现「窗口与位图差一像素」的极难排查错位。收敛到 `config.rs` 或 `util.rs`：

```rust
/// 96-DPI 基准像素按窗口 DPI 缩放（四舍五入），全项目唯一实现。
pub fn dpi_scaled(base: i32, dpi: u32) -> i32 {
    ((base as f64) * (dpi as f64) / 96.0).round() as i32
}
```

### B3【P2】`HWND ↔ isize` 裸转换样板 8 处

`CURRENT_MAIN_HWND`、`SESSION_NOTIFY_HWND`、`POWER_NOTIFY_HANDLE`（main.rs）、`TASKBAR_HWND`、`WATCHDOG_HWND`（window.rs）——同一模式 `store(hwnd.0 as isize)` / `load() → raw != 0 → HWND(raw as *mut c_void)` 复制了 8 遍，每遍都在重写「0 是空位」这个约定。收敛为 newtype：

```rust
/// HWND 的原子槽：0 为空位；存取自动完成指针/整数转换。
pub struct AtomicHwnd(AtomicIsize);

impl AtomicHwnd {
    pub const fn new() -> Self {
        Self(AtomicIsize::new(0))
    }
    pub fn store(&self, hwnd: HWND) {
        self.0.store(hwnd.0 as isize, Ordering::Release);
    }
    /// 取出当前值并清零；空槽返回 None。
    pub fn take(&self) -> Option<HWND> {
        let raw = self.0.swap(0, Ordering::AcqRel);
        (raw != 0).then(|| HWND(raw as *mut std::ffi::c_void))
    }
    pub fn load(&self) -> Option<HWND> {
        let raw = self.0.load(Ordering::Acquire);
        (raw != 0).then(|| HWND(raw as *mut std::ffi::c_void))
    }
}
```

`live_main_hwnd` / `watchdog_hwnd` 退化为「`load()` + `IsWindow` 校验」两行；`unregister_session_notification` 里的裸 cast 全部消失。`POWER_NOTIFY_HANDLE` 是 `HPOWERNOTIFY`，同构写第二个或做泛型 `AtomicHandle<T>` 均可。**性能：与现有原子读写完全同构。**

### B4【P2】`check_fullscreen` 全屏退出边沿写了两遍

`suspend.rs:222-233`（前台无效/桌面/自身 分支）与 `270-285`（正常判定分支）都实现了「退出全屏边沿 → 重建基线 → 同步定时器 → 强制重绘」。抽：

```rust
/// 全屏状态切换的边沿处理：离开全屏时重建差分基线并恢复监测。
fn on_fullscreen_edge(hwnd: HWND, was: bool, now: bool) {
    if was == now {
        return;
    }
    if !now {
        reset_network_baseline();
        reset_cpu_baseline();
    }
    let _ = sync_monitoring_timers(hwnd);
    if !now {
        force_repaint(hwnd);
    }
}
```

主判定分支调用后删掉重复段。逻辑不变，测试（`timer_plan` 系列）不受影响。

---

## 5. 违反项目自身铁律：可调域常量散落（C 类）【P2】

AGENTS.md 明文：「**所有的具体常量数值（如像素宽、高、定时器间隔、颜色等）均统一定义在 src/config.rs 中**」。以下**可调域数值**（超时/重试/栈大小——会被人调、需要跨模块对照的参数）散落在业务文件里，应搬进 `config.rs`：

| 数值 | 位置 | 建议 const 名 |
|------|------|---------------|
| `15000`（WinHTTP 四段超时，**两个函数各一份**） | `http.rs:104,289` | `HTTP_TIMEOUT_MS: i32`（B1 合并后自然只剩一份） |
| `500`（版本文件重试等待） | `update/mod.rs:291` | `UPDATE_FETCH_RETRY_DELAY_MS: u64` |
| `64 * 1024`（更新线程栈） | `update/mod.rs:181` | `UPDATE_WORKER_STACK_BYTES: usize` |

这不是洁癖：**B1 的两个 15000 已经演示了散落的代价**——改一处忘一处。搬入 config.rs 是零风险机械操作。

**v1.1 范围收窄（接受外部评审）——以下不搬**：
- `CREATE_NO_WINDOW`（`update/mod.rs:658`）是 Win32 语义常量且已是具名局部 const。正确改法不是搬进 config，而是**直接导入 windows crate 自带的 `Win32::System::Threading::CREATE_NO_WINDOW`**（`Win32_System_Threading` feature 已在 `Cargo.toml` 启用），连本地定义一并消灭；
- `with_capacity(16)` / `(32)` 一类容量 hint 属实现细节，硬搬只会让 config.rs 膨胀；
- 由此把 AGENTS.md 铁律的合理读法明确为「**可调域参数进 config，API 语义常量留在使用点**」。若要把这层区分写进 AGENTS.md，需按其自身的修改门槛单独评估，不在本报告范围内。

---

## 6. 类型表达力与状态组织（D/E 类）——「读起来像谁写的」的主要决定因素

### D1【P3】暂停原因用裸 `u32` 位集

`state.rs:15-19` 定义了三个 `pub const SUSPEND_REASON_*: u32`，然后 `suspend_system(hwnd, reason: u32)` 接受任意整数。调用点写 `SuspendReasons::suspend(SUSPEND_REASON_SESSION)`——**传个 `0b101`、传个 `7`、传个定时器 ID 都能编译**。项目已经为 `SuspendReasons` 做了位协议封装，却让入口参数停留在裸整数，功亏一篑。

```rust
/// 暂停原因（位标志）。构造仅限本模块常量，外界无法凭空造出非法位集。
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct SuspendReason(u32);

impl SuspendReason {
    pub const SYSTEM: Self = Self(1 << 0);
    pub const SESSION: Self = Self(1 << 1);
    pub const MONITOR: Self = Self(1 << 2);
    fn bits(self) -> u32 {
        self.0
    }
}
```

`SuspendReasons::suspend/resume` 改收 `SuspendReason`，`suspend(SUSPEND_REASON_SESSION | SUSPEND_REASON_MONITOR)`（现有测试里这种位或写法）改由 `bits()` 内部处理或提供 `SuspendReason::union`。**零运行时成本，调用点全部获得编译期检查。**

**v1.1 降级为 P3（接受外部评审，附重评条件）**：本节上文虽已提到 union/位或的适配成本，但结论没算总账——`state.rs:139` 的 `SUSPEND_REASON_SESSION | SUSPEND_REASON_MONITOR` 位或写法要求 newtype 补 `BitOr` 或 `union` 方法并改写相关测试；而三个标志位、全部写入点都定义在同一模块十行之内，误用风险本就被紧邻性覆盖。为 3 个标志付这笔钱不值。**触发重评条件**：新增第 4 个暂停原因、或 `SuspendReasons` API 需要对模块外暴露时，升级回 P2。

### D2【P2】`scan_subprocess_protocol` 返回裸 4 元组

`update/mod.rs:709-744`：

```rust
fn scan_subprocess_protocol(...) -> (Option<UpdateAction>, bool, bool, bool)
```

三个 `bool` 在位置上无法区分（哪个是 `read_failed`、哪个是 `exit_forwarded`？解构处必查注释）。而同文件 80 行外的 `SubprocessOutcome` 已经是带文档注释的结构体——**同一个模块里两种风格并存，是「多人协作痕迹」最明显的一处**。改：

```rust
struct ScanOutcome {
    /// 首个有效动作（DONE/EXIT_MAIN），无效流为 None。
    action: Option<UpdateAction>,
    /// 读到过 EXIT_MAIN（协议层事实）。
    exit_signalled: bool,
    /// 读取中途失败（EOF 前的 IO 错误）。
    read_failed: bool,
    /// EXIT_MAIN 已成功投递给看门狗。
    exit_forwarded: bool,
}
```

现有 9 个协议测试用 `Cursor` 喂流，断言改为字段访问，测试逻辑一行不改。

### D3【P2】`create_window` 七参数 + allow

`window.rs:54-63` 的 `#[allow(clippy::too_many_arguments)]` 是对坏味道的官方承认。错误文案作为参数传来传去尤其别扭（调用方既传了上下文又交出了文案控制权）。改：

```rust
struct WindowSpec<'a> {
    class_name: &'a str,
    title: &'a [u16],
    width: i32,
    height: i32,
    style: WINDOW_STYLE,
    ex_style: WINDOW_EX_STYLE,
}
```

错误文案由 `create_window` 统一生成（`"创建{类名}窗口失败"` 之类），调用方不再传 `err: &str`。

### D4【P2】CLI 参数无单一事实来源

见 A1 的 `CliArgs`——四个布尔现在散布在 `main()` 里四次 `args.iter().any(...)`，每次都是 O(n) 重扫。`CliArgs` 一次解析后，`main()` 的启动编排变成：

```rust
let cli = CliArgs::parse();
if cli.quit {
    quit_existing_instance();
    return;
}
if cli.check_update {
    std::process::exit(subprocess_main(cli.manual));
}
// ...
if cli.relaunched_by_update {
    defer_initial_auto_check();
}
```

### E1【P2】cpu_mem 的 4 个静态原子量 vs 1 个概念

`cpu_mem.rs:8-11` 用 `PREV_IDLE_TIME / PREV_KERNEL_TIME / PREV_USER_TIME / CPU_INITIALIZED` 四个**全局原子**表达「上次 CPU 采样基线」这一个概念。这些量只在 UI 线程的 `WM_TIMER` 里读写（`reset_cpu_baseline` 也来自同一消息循环），原子与内存序在此**没有提供任何真实保护**——`state.rs` 模块头声称的「Acquire/Release 跨线程握手」对这些量并不成立，注释在误导后来者。

```rust
struct CpuTimes {
    idle: u64,
    kernel: u64,
    user: u64,
}

thread_local! {
    /// None = 尚未建立基线。仅 UI 线程读写，无需原子量。
    static CPU_BASELINE: Cell<Option<CpuTimes>> = const { Cell::new(None) };
}
```

`collect_cpu` 变成一次 `with` + 一个 `match`；`reset_cpu_baseline` 变 `set(None)`。四个全局静态消失，虚假的内存序声明消失，数据结构本身就表达了「基线存在与否」。**性能：x86 上 Relaxed 原子与普通 Cell 本就同价，此改动中性偏正。**

> 若刻意保留原子量以备「将来把采集挪到独立线程」，那么应当把该意图写成注释并统一序——但现在没有任何此计划，YAGNI。

### E2【P2·暂缓】network.rs 的三层闭包嵌套

`collect_network`（`network.rs:88-131`）是 `with_virtual_blacklist(|blacklist| CURRENT_DATA.with(|cell| { ... INTERFACE_HISTORY.with(|hist| ...) }) })` 三层缩进、三种借用混合。中间任何一层再加状态就要四层。收敛：

```rust
struct SamplerState {
    current: HashMap<u64, (u64, u64)>,
    history: HashMap<u64, Sample>,
    blacklist: HashSet<u64>,
    blacklist_refreshed_at: Option<Instant>,
}

thread_local! {
    static SAMPLER: RefCell<SamplerState> = RefCell::new(SamplerState::new());
}
```

`collect_network` 变为一次 `with`、单层借用、顺序语句；黑名单刷新成为 `SamplerState` 的一个方法（现有 `blacklist_needs_refresh` / `rebuild_virtual_blacklist` 纯函数与它们的测试**原样保留**，只是挂在状态结构旁边）。现有 8 个测试的语义全部不变。**性能：哈希操作次数不变。**

**v1.1 前置条件与事实澄清（部分接受外部评审）**：评审要求「证明无重入借用才能做」——这个要求是对的，当前调用图也确实满足：`with_virtual_blacklist` 的唯一调用方是 `collect_network`；`CURRENT_DATA` 只在其闭包内访问；`INTERFACE_HISTORY` 仅在采样与测试中访问；模块内不存在任何持借用状态下对同一 cell 的再次 `with`。但评审「现三个 thread_local 借用期独立」的前提与代码事实不符：**今天的借用域本就是链式拉满的**——`with_virtual_blacklist` 的不可变借用横跨整个采样闭包（`network.rs:265-267`：`let cache = cell.borrow(); f(blacklist)`，而 `f` 覆盖 `CURRENT_DATA` 与 `INTERFACE_HISTORY` 全部工作），`CURRENT_DATA` 的可变借用又包住 `INTERFACE_HISTORY` 借用。合并不实质延长任何借用期，真正的变化只有一处：**重入同一 cell 的 panic 面从三个分散锁域合并为一个**。综上维持 P2 但标记**暂缓**：落地前把上述调用图证明写进 PR 描述；且接受「评审若仍不买账，维持现状完全可辩护」——三层嵌套是 thread_local 同时借用三物的固有代价，不是缺陷。

### E3【P2】`main()` 155 行的编排

`main()` 目前是九个阶段的直线：参数 → 单例 → 优先级 → IME → 类注册 → 消息注册 → 看门狗 → 主窗口 → 各类绑定 → 消息循环 → 清理。有两个自然切面，且都不改变行为：

1. `run_message_loop() -> ()`：把 `main.rs:235-253` 的循环提出来（顺带解决 E4 的全限定路径问题——提取时把 `GetMessageW/TranslateMessage/DispatchMessage/MSG` 加进 use）；
2. 单例锁段（124-142）提为 `init_single_instance() -> Option<MutexGuard>`。

`main()` 缩到 ~90 行后，每个阶段的错误退出路径一眼可比对。

### E4【P3】整洁度杂项

- `main.rs:600` `TIMER_ID_AUTO_UPDATE if !is_suspended() && ...` 用 match guard，而兄弟 arm（566-599）在函数体内 if——统一为函数体内 if（与大多数 arm 一致）；
- `cpu_mem.rs:28-32` 的 `Some(&mut idle_time as *mut u64 as *mut _)` 双重 cast 可用 `FILETIME` 类型直接消除；
- `suspend.rs:299` 每次 `WM_SETTINGCHANGE` 都 `encode_utf16().collect()` 一个 Vec——注释已声明低频可接受，若追求极致可换 `const EXPECTED: [u16; 18]`（需要手写字面量，权衡后维持现状也合理）。

---

## 7. 性能与内存专项审查（G 类）

用户点名要求「保证性能和内存」，逐项核对热路径后的结论：**现有性能/内存设计全部应保持，下列微项收益有限，列出仅为完整性，均为 P3。**

| # | 位置 | 现状 | 评估 |
|---|------|------|------|
| G1 | `renderer.rs:336` | 每次 `render` 都 `Layout::new`（5 次 f64 乘除 + round） | 绘制仅在数值变化时发生（门控已做），每帧省 ~100ns 级。可把 `Layout` 缓存为 `Renderer` 字段、`update_dpi` 时重算。**顺手做，别专项做** |
| G2 | `renderer.rs:356,374` | 箭头字符每帧 `encode_utf16` | 同上量级。`const ARROW_UP: [u16; 2] = [0x2191, 0];` 直接消掉 |
| G3 | `network.rs:80` | `GetIfTable2` 每秒分配/释放整表 | 这是 API 语义，换 `GetIfEntry2` 逐 LUID 查询省不了 syscall 次数，**不建议动** |
| G4 | `renderer.rs:616-619` | `1024*1024-1` 字节显示为「1024.0 KB/s」 | 显示学上应进位为「1.0 MB/s」。改法：KB 分支先算 `x`，`x >= 10240` 时落入 MB 分支（含测试更新）。cosmetic |
| G5 | `Cargo.toml` `opt-level = "z"` | 尺寸优先 | 热路径全是 Win32 调用与 GDI blit，自有计算量极小，`"z"` 无实际瓶颈。**保持** |

**内存卫生体系复核**（DELAYLOAD 隔离、低内存优先级、EcoQoS 作用域、trim 时点、堆压缩语义、64KB 更新线程栈、`buf` 复用、`LAST_RENDERED_VALUES` 门控）：设计意图全部正确且有注释支撑，本报告的所有建议（newtype、结构体收敛、常量搬家、http 合并）**均不新增任何运行时分配**，其中 E1 从原子量改 `Cell` 在语义上还略去了不必要的同步指令。

---

## 8. 工程化建议（F 类）【P3，但建议尽早上 F1/F2】

### F1 静默失败无诊断通道

全库数十处 `let _ =`（`SetTimer`、`InvalidateRect`、`PostMessageW`、`Shell_NotifyIconW`、`DeleteObject`…）。多数失败确实无害（注释也说明了原因），但**排查现场问题时，这些静默点让「托盘菜单失灵」「定时器没起来」这类 bug 只能靠读代码猜**。

**v1.1 方案重做（原方案有硬伤）**：v1.0 用 `eprintln!` 宏是**错的**——`main.rs:1` 是 `#![windows_subsystem = "windows"]`，GUI 子系统进程没有控制台，debug 构建下 `eprintln!` 遇无效 stderr 句柄会直接 panic，等于在想要诊断的场景里埋 panic，且输出无处可看。正确通道是 `OutputDebugStringW`：无句柄依赖、对任意进程安全、由调试器或 Sysinternals DebugView 捕获：

```rust
/// 诊断输出。GUI 子系统进程无控制台（eprintln! 会 panic 且无处可看），
/// 走 OutputDebugStringW，由调试器 / DebugView 捕获。
macro_rules! diag {
    ($($arg:tt)*) => {{
        #[cfg(debug_assertions)]
        {
            let msg = format!($($arg)*);
            let wide = crate::util::to_wide(&msg);
            // SAFETY: wide 含尾 NUL；ODS 无句柄依赖，对任意进程安全。
            unsafe {
                windows::Win32::System::Diagnostics::Debug::OutputDebugStringW(
                    windows::core::PCWSTR(wide.as_ptr()),
                )
            };
        }
    }};
}
```

配套：`Cargo.toml` 需补 `"Win32_System_Diagnostics_Debug"` feature。release 构建宏体整体被 cfg 掉，零成本。埋点位置不变：A4（托盘失败）、`sync_monitoring_timers` 失败、`embed_in_taskbar` 失败等十余个关键静默点。仍**不引入 tracing/log**——与「纯 Rust、零运行时配置」的项目定位冲突。

### F2 Cargo.toml 补两项

```toml
rust-version = "1.85"   # edition 2024 的下限，声明 MSRV，防止旧工具链莫名编译失败

[lints]
rust.unsafe_op_in_unsafe_fn = "deny"
clippy.enum_glob_use = "warn"
clippy.items_after_statements = "warn"
clippy.undocumented_unsafe_blocks = "warn"   # 本库 unsafe 旁注纪律极好，直接用 lint 钉死
```

把已经在 CI 兑现的纪律固化进仓库本身，贡献者不跑 CI 也能被本地 cargo 拦住。

---

## 9. 实施路线图（v1.1 按外部评审意见重排）

分批 PR，每批独立可验证、可回滚。**每批完成后必须过全套门禁**（来自 AGENTS.md，不可省）：

```
cargo test --locked
cargo build --release --locked
cargo clippy --all-targets --locked -- -D warnings
cargo fmt
```

> **v1.1 更正**：原「第一批＝纯机械、零行为变化」的说法不成立。CliArgs、os_to_wide、AtomicHwnd 都触碰行为面或并发面，属**低风险**而非**零风险**。下方每项附不变量核对单，落地时逐条过。

### 第一批：最高价值项（外部评审与本报告一致的先做集合）

| 项 | 内容 | 不变量核对单 |
|----|------|--------------|
| B1 | http.rs 合并为 `HttpGet::open + for_each_chunk`（保留全部现有测试；单独 PR + 设计评审） | 两函数对外签名与错误映射不变；`FetchFileError` 的 Download/Local 分类逐 arm 对照 |
| A1+D4 | `CliArgs::parse()`（args_os，消灭 panic 与 4 次扫描） | 优先级保持 `--quit` 先于 `--check-update`；**补一条组合参数单测钉死该语义** |
| A2 | `os_to_wide` + `var_os` 三处替换 | 这是**有意的行为变化**：非 UTF-8 路径从「损坏后失败」变「正确」；常规路径输出逐字节不变 |
| D2 | `ScanOutcome` 结构体 + 测试断言改字段访问 | 9 个协议测试语义不变 |
| B2 | `dpi_scaled` 唯一实现 | 三处调用点数值不变 |
| A3 | 尾 NUL 防御（字体名 / szTip） | 正常长度字符串输出不变 |

### 第二批：机械收敛（低风险，一个 PR 可打包）

| 项 | 内容 | 不变量核对单 |
|----|------|--------------|
| B3 | `AtomicHwnd` 替换 5 个 isize 静态量 | 逐点核对 swap/load 语义：`unregister_session_notification`/`rebuild_main_window` 用 `take()`（=swap(0)），`live_main_hwnd` 用 `load()` |
| C1 | 可调域常量搬入 config.rs（范围按第 5 节 v1.1 收窄）；`CREATE_NO_WINDOW` 改用 windows crate 导入 | 常量值不变 |
| E4 | match guard 统一、FILETIME 消除双重 cast | 纯等价改写 |
| F2 | Cargo.toml 补 rust-version / [lints] | 不引入新警告即通过 |

### 第三批：结构收敛（逐项评审）与可选项

E1（`CpuBaseline` 收敛）、E3（main 分解）、D3（`WindowSpec`）、B4（`on_fullscreen_edge`）需逐项评审；A4 托盘失败处理、A5 DPI 半失败回滚、F1（ODS 方案）、G 微项按需；**E2 满足前置证明后再议，D1 触发重评条件后再议**。

### 明确不建议做的

- **不要**引入 clap/thiserror/log 等依赖解决 D4/错误类型/日志——与零依赖定位冲突，现有规模用不上；
- **不要**把 UI 线程采集挪到工作线程「为了架构好看」——会引入句柄校验与定时器重建的全套复杂度，而现状性能绰绰有余；
- **不要**把 `with_renderer` 的 thread_local 服务定位改成 `GWLP_USERDATA`——现有注释已论证重入退化策略，替换的侵入性远大于收益；
- **不要**动 AGENTS.md 第 1-7 条钉死的 Win32 时序——那是本项目的核心资产。

---

## 10. 审查边界

- 静态审查无法验证运行时行为：Explorer 崩溃注入、多显示器 DPI 切换、UAC 取消回放等场景依赖现有注释与测试钉住；
- `installer.iss`、`scripts/`、CI workflow 未在本报告范围内（非 Rust 交付物）；
- 行号以当前 HEAD 为准（v1.5.0），后续提交可能漂移，问题编号以函数名交叉定位为准。

---

## 11. 修订记录

- **v1.1（2026-09-19）**：吸收外部评审（要点：P1 评级虚高、F1 方案无效、E2 需前置证明、C1 误读铁律、D1 代价未算足、批次一「零行为变化」不成立）。变更：
  1. 新增评级定义（P1=缺陷 / P2=高收益维护项 / P3=可选），P1 收敛为 A1、B1；A2、B3、C1、D2、D4 降 P2；D1 降 P3（附重评触发条件）；
  2. **F1 方案重做**：`eprintln!` 在 `windows_subsystem="windows"` 下会 panic 且无输出，改为 `OutputDebugStringW`（需补 `Win32_System_Diagnostics_Debug` feature）；
  3. **C1 范围收窄**：仅搬超时/重试/栈大小；`CREATE_NO_WINDOW` 改为直接导入 windows crate 常量；容量 hint 不搬；
  4. **E2 标记暂缓**：补「无嵌套借用」证明要求，并澄清「三 thread_local 借用期独立」的前提与代码事实不符（blacklist 借用本已横跨整个采样闭包）；
  5. **批次一重新定性**为「低风险 + 逐项不变量核对单」，执行顺序按评审意见重排（B1、A1、D2、B2、A3 先行）。
  - 未采纳的两处（记录备查）：评审称 D1「代价没算」——原报告已列 union 适配成本，但接受其降级结论；评审称 E2「借用期独立」——与代码事实不符（见 E2 节澄清），但不影响其「需先证明」的结论本身。
