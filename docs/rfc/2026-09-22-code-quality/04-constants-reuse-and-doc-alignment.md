# Agent Note：常量归位、重复消除与文档对齐

Status: proposed

## 问题

本仓在「具体常量统一进 config.rs」与「文档即约束」两条自我约定上已有可见漂移：

1. **魔法数与具名常量缺失。** `src/renderer.rs:582-584` 的 `lfWeight: 400`、`lfQuality: FONT_QUALITY(3)`（应写具名常量 `NONANTIALIASED_QUALITY`）、`src/renderer.rs:587` 的字面量 `"Segoe UI"`；`src/tray.rs:41-43` 的 `PCWSTR(1 as *const u16)`（即 `MAKEINTRESOURCEW(1)`，且带全仓唯一一处 `#[allow(clippy::manual_dangling_ptr)]`），资源 ID 1 与 `assets/icon.ico`、`build.rs` 之间没有命名关联；`src/update/crypto.rs:89` 的 `[0u8; 8192]`。
2. **同一事实两份表示。** `src/main.rs:136-137` 的 `for _ in 0..50 { sleep(100ms) }` 是「5 秒退出等待」的第二份实现，与 `src/config.rs:90-91` 的 `MAIN_EXIT_WAIT_TIMEOUT_MS` / `MAIN_EXIT_POLL_INTERVAL_MS` 同值不同源；`installer.iss:53` 还有第三份（已注明同量级）。
3. **跨文件重复。** ISCC 查找逻辑写在 `.github/workflows/release.yml:68-88`、`.github/workflows/release.yml:98-118` 与 `scripts/package.ts:23-37` 三处（四段）；「定长宽缓冲截断拷贝」在 `src/tray.rs:63-65` 与 `src/renderer.rs:588-590` 各一份；`AtomicHwnd` / `AtomicPowerNotify` 同构双份（已注释「同构单列」，再来第三个才该泛型化）。
4. **个人机器路径入库。** `scripts/package.ts:25` 的 `D:\soft\Inno Setup 7\ISCC.exe` 作为候选列表首位。
5. **两个上帝文件。** `src/main.rs`（839 行：CLI + 启动编排 + 消息路由 + 重建/退出编排 + 测试）与 `src/update/mod.rs`（1157 行：协议 + 网络 + 缓存 + 提权启动 + 注册表）；后者内部已有 `VerifiedInstaller`（`src/update/mod.rs:251`）、`ScanOutcome`（`:720`）、`FetchFailure`（`:383`）等天然切面。
6. **CI 缺 MSRV 对账。** `Cargo.toml:5` 声明 `rust-version = "1.85"`，CI 用最新 stable，两者永不对账；`parse_version` / `parse_update_metadata` 是 fuzz/property 的理想标的，当前没有。
7. **AGENTS 第 3 条措辞有歧义（不是实现漂移）。** 该条写「选取当前流量最大的一张单一物理网卡**锁定**并展示」。读法 (a)：时间维度粘滞（跨 tick 保持同一张卡）；读法 (b)：在一次采样内选定一张卡、而不是把多卡流量求和。实现（`src/collector/rate.rs:12-45` 的 `select_winner_interface`）是每 tick 重新择大——按读法 (b) 与文档**不矛盾**，按读法 (a) 才算漂移。两种读法下「双网卡同时有流量（Windows 11 自动链路切换、手机共享上网）时显示逐秒跳变」这个现象都存在，值得先确认要哪种语义，再决定是否加粘滞。
8. **不可删的兜底与冗余 I/O。** `src/suspend.rs:318` 的 `return true` 在**运行时**不可达（`expected` 含尾 NUL，循环内 `src/suspend.rs:314-316` 必返回），但它是**类型系统要求的兜底**：函数以 `for` 循环结尾，Rust 不认为 `for` 必然执行（迭代器可能为空），删掉它直接编译失败（E0308），且编译器**不会**报 `unreachable_code`——所以它既不是可删死代码，也不是能被 lint 发现的冗余。另：`src/window.rs:326-364` 的 `LAST_RECT` 缓存只在移动成功后提交，而 `embed_in_taskbar` 不走它，于是嵌入成功后的一个 tick 会多做一次 `SetWindowPos`（无害冗余）。

## 提案

1. **常量归位（只收承重语义的）**：`src/config.rs` 增 `FONT_FACE_NAME`、`FONT_WEIGHT_NORMAL`，字体品质改用具名 `NONANTIALIASED_QUALITY`（已核实存在于 windows 0.62：`Graphics/Gdi/mod.rs` 有 `pub const NONANTIALIASED_QUALITY: FONT_QUALITY = FONT_QUALITY(3u8)`），另加 `TRAY_ICON_RESOURCE_ID`、`HASH_READ_BUF_BYTES`；`src/main.rs:136-137` 的等待改用 `MAIN_EXIT_WAIT_TIMEOUT_MS` / `MAIN_EXIT_POLL_INTERVAL_MS`（若确认两处需求确实不同，就改成两个各自命名的常量并各写一句理由，不留同值匿名数字）。纯容量提示（`src/renderer.rs:280`、`src/collector/network.rs:27-28`）**不进** `config.rs`，保留内联并加「容量提示，非约束」注释。
2. **消重复**：`src/util.rs` 增补定长宽缓冲拷贝助手，供 `src/tray.rs:63-65` 与 `src/renderer.rs:588-590` 共用；`scripts/package.ts:25` 删掉硬编码路径（保留系统路径候选 + `where` 兜底）；CI 侧两段 ISCC 查找合并为一步，或改为「安装到固定路径后直接引用」以消掉查找。
3. **拆分 `src/update/mod.rs`** 为 `protocol.rs`（子进程协议与 `test_scan_*`）、`installer.rs`（缓存/校验/启动）、`cache.rs`（临时文件清理与复用），`mod.rs` 只留编排与注册表开关；纯移动、无行为变化，须排在 `01-abort-safety-and-invariants.md` 的改动之后，避免同文件冲突。
4. **CI 加 MSRV job**：用 1.85 工具链跑 `cargo check --locked`，与最新 stable 的 check.yml 并行；首轮若红，在同一 PR 内决定「升 `rust-version`」还是「改代码」。
5. **网卡选择（决策项：实施前必须先裁定读法）**：先定 AGENTS 第 3 条要表达哪种读法，裁定后两条路都在本笔记范围内完成——读法 (b)（采样内选定、不求和）只需把措辞改成不会被读成「跨 tick 粘滞」的表述；读法 (a) 则给 `select_winner_interface` 加赢家粘滞（挑战者需连续 N 个 tick 总流量更高才换卡，纯函数 + 用例）并同步措辞。两条路方向相反但都是小改动：本笔记只登记决策点、不预设答案，**但要在实施前裁定，不留悬置项**。
6. **`src/suspend.rs:318` 保留 + 注释**：写明「运行时不可达的前提是 `expected` 以尾 NUL 结尾；本行是类型系统要求的兜底，不是死代码，删除会编译失败」。若要让代码本身表达这一点，才考虑把循环重写成显式结构（成本更高、收益仅观感，建议不做）。

## 明确不在本次范围

- `AtomicHwnd` / `AtomicPowerNotify` 泛型化：按 AGENTS 第 8 条的归属精神，等第三个同构类型出现再说。
- **不让 `embed_in_taskbar` 提交 `LAST_RECT`**：`LAST_RECT` 是 `update_taskbar_position` **函数体内**的 `thread_local`（`src/window.rs:327-329`），要写它必须先把静态量提到模块作用域——而这正是报告自己批评的「隐式状态藏在函数体里」。为一个「无害冗余」（每次嵌入后多一次 `SetWindowPos`）付这个结构成本不划算，改注释即可。
- 速率显示在 `1023 B/s` 与 `1.0 KB/s` 之间的粒度突变：`test_format_speed_wide_boundaries`（`src/renderer.rs:622-665`）已把闸门与舍入钉死，改它只会让一批用例变红而收益仅是观感。
- 测试注释密度（作者把测试当规格写、每条说明「改哪种实现会红」）保持，不改。
- `CODE_QUALITY_REVIEW.md`（仓库根的审查报告）在四篇笔记拆出后如何处理，由用户自行决定。

## 为什么不保留？

反方其一：「`0..50` 与 `100ms` 和 `MAIN_EXIT_WAIT_TIMEOUT_MS` 本就是不同场景（`--quit` 等看门狗消失 vs 子进程等主进程退出），合并会耦合两处行为」。回应：若真不同就该有两个命名常量与两句理由；现状是同一数字、无理由、无关联，任何一侧调整都会被误以为另一侧也调整了，这才是漂移风险的真实形态。反方其二：「`config.rs` 会变成杂物间」。回应：本笔记明确只收承重语义常量并显式排除容量提示，恰恰是在给 `config.rs` 划边界。反方其三：「拆 `update/mod.rs` 是纯移动，收益只有可读性，还要付 `git blame` 失真」。回应：收益是让协议面有单一边界，且纯移动提交与行为提交分开即可把代价限制在一次提交上。反方其四：「MSRV job 会立刻变红，是自找麻烦」。回应：变红正是价值所在——「声明 1.85 却只跑最新 stable」等于没有承诺。反方其五：「第 6 项只是加注释，等于没做事」。回应：正因为「死代码」这个判断会让人去删（删了就编译不过，见第 8 条），把它写成注释才是防止误改的最小成本手段。

## 验收标准

- `grep -rn 'FONT_QUALITY(3)' src/` 无命中；`grep -rn 'D:\\soft' scripts/` 无命中；`grep -n 'NONANTIALIASED_QUALITY' src/renderer.rs` 有命中。
- `grep -n 'for _ in 0\.\.50' src/main.rs` 无命中；两处 5 秒退出等待在注释里互相指名。
- 拆分后 `grep -c '#\[test\]' src/update/protocol.rs` 与拆分前 `src/update/mod.rs` 的协议用例数一致（纯移动判据）；`cargo test --locked` 全绿。
- `.github/workflows/` 出现使用 1.85 的 job 且通过，或本 PR 内同步提升 `Cargo.toml:5`。
- `src/suspend.rs:318` 的 `true` **仍在**（删了就编译不过），其上方注释说明「运行时不可达、但这是类型系统要求的兜底」。
- AGENTS 第 3 条的措辞与 `src/collector/rate.rs:29-33` 的行为二者明确一致：或澄清措辞（读法 b），或加粘滞（读法 a）。
- `cargo clippy --all-targets --locked -- -D warnings`、`cargo fmt`、CI 全绿。

## 风险

- 拆分 `update/mod.rs` 若与在飞的更新改动并行，冲突面很大；建议把它排在本笔记的最后一笔提交，且避开发布窗口（跨笔记依赖：`01-abort-safety-and-invariants.md` 先实施更稳，但两篇仍是各自独立的实施单元）。
- MSRV job 首轮可能立刻暴露不兼容（含依赖的最低版本要求），需在同一 PR 内做「升 `rust-version` 或降级依赖」的决策——这是真实决策成本，不是纯流程改动。
- 第 5 项若选「加粘滞」，会改动用户可见行为（网卡切换出现延迟感），须与「显示不再跳变」的收益一并权衡。
- 第 5 项若选读法 (b)（只改措辞），需接受「双网卡活跃时显示的是单卡速率、不是两卡之和」这一既有产品语义——本笔记不挑战它，AGENTS 第 3 条的本意正是不被虚拟机/VPN 的流量干扰。

