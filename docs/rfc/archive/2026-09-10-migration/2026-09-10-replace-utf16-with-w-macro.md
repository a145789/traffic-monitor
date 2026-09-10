# Agent Note：删除手写 const utf16，改用运行时宽串比较

Status: implemented

## 问题

[`util::utf16`](../../../../src/util.rs) 是一个手写的 `const fn` ASCII 展开器（逐字节 `as u16`，非 ASCII 会产生错误结果，注释明确要求调用方保证 ASCII），生产侧唯一消费者是 [`is_immersive_color_set`](../../../../src/suspend.rs) 中的 `const EXPECTED: &[u16] = &utf16::<18>("ImmersiveColorSet\0")` 一处；其余全部是专用测试（`test_utf16_ascii`、`test_utf16_exact_fit`）。该比较每 `WM_SETTINGCHANGE` 才触发一次（一天至多数次），根本不需要 `const` 求值；留着它反而长期背一条“误传非 ASCII 即错”的隐式契约，未来调用者极易踩坑。注意：不可改用 `w!("ImmersiveColorSet").as_wide()` 静态片——`windows-strings 0.5.1` 中 `as_wide()` 是 `unsafe fn` 且明确“String data without the trailing 0”（长 17 而非 18），换上后比较会从含 NUL 精确匹配退化为前缀匹配，`"ImmersiveColorSetFoo\0"` 会被误判为主题变更。

## 提案

删除 `util::utf16` 定义及其两个专用单测，把 `is_immersive_color_set` 中的 `const EXPECTED` 改为运行时 `"ImmersiveColorSet\0".encode_utf16().collect::<Vec<_>>()` 一次性比较（与现 `utf16::<18>` 逐字等价，含尾 NUL，无 `unsafe`）；同步新增一个 `"ImmersiveColorSetFoo\0" -> false` 的前缀回归用例，钉死含 NUL 精确匹配语义。`to_wide` / `push_wide` 不受影响（它们处理运行时中文与热路径复用缓冲，与本改动正交）。

## 为什么不保留？

最强的反方理由是“`const` 比较零分配，运行时 `encode_utf16` 每次 `WM_SETTINGCHANGE` 都要分配”。但该事件一天至多数次，一次 18 个 `u16` 的临时分配不可观测；留着 `utf16` 反而让未来调用者误以为它是通用宽串工具，误传中文即错。低频路径用运行时比较换掉整套 `const` 展开器加专用测试，是净删除。

## 验收标准

`rg -n "pub const fn utf16|utf16::<" src/` 无命中；`cargo test suspend` 全绿（含原有 `test_immersive_color_*` 四例与新增的前缀回归用例）；浅色/深色主题切换时文字颜色仍正确翻转；`cargo clippy -- -D warnings` 无警告。

## 风险

几乎纯重构风险：运行时宽串必须与原 `utf16::<18>("ImmersiveColorSet\0")` 逐字一致（含尾 NUL、长 18），否则主题跟随失效。缓解：保留原有 `valid_string` / `prefix_only` / `wrong_string` 三例不变，新增的前缀回归用例专门覆盖 `as_wide()` 方案会误判的 `"ImmersiveColorSetFoo\0"`；该用例在旧 `utf16::<18>` 语义与新运行时语义下均为 `false`，在错误的 17 元素无 NUL 切片方案下为 `true`，正好锁住本坑。
