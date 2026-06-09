# Unsafe Code Policy

## Core Principle

`unsafe` 不是性能优化工具。

`unsafe` 仅用于表达编译器无法验证、但开发者能够证明正确性的操作。

每一处 `unsafe` 都必须能够回答：

> 为什么这里不会触发 Undefined Behavior (UB)？

无法证明时禁止引入。

---

# Unsafe Minimization

## Rule 1: 优先 Safe Rust

新增功能时必须优先寻找：

- 标准库方案
- 第三方安全封装
- 类型系统建模
- 生命周期建模

不得因为实现方便而直接使用裸指针或 `unsafe`。

---

## Rule 2: 缩小 Unsafe 范围与原子事务合并

**原则**：`unsafe` 范围应尽可能缩减到单个操作。

禁止将无关的安全 Rust 代码混在 `unsafe` 块中：

```rust
unsafe {
    let row = table.row(i);
    process(row); // 属于 Safe Rust，禁止混入
    render(row);  // 属于 Safe Rust，禁止混入
}
```

应写为：

```rust
let row = unsafe {
    table.row(i)
};

process(row);
render(row);
```

**例外情况（原子事务合并）**：
当一系列 FFI 调用属于**强耦合的原子事务**（即前一步产生的非安全状态必须立刻被下一步消费，或者它们共同构成一个不可分割的状态转换）时，**允许**合并为一个 `unsafe` 块，以保证逻辑连贯并能统一描述不变性证明。例如：嵌入任务栏、创建并配置窗口属性等。

禁止将完全独立的、无状态依赖关系的 FFI 调用强行塞进同一个大 `unsafe` 块中。

---

## Rule 3: Unsafe Core + Safe Interface

优先：

```rust
pub fn rows(&self) -> &[Row] {
    unsafe {
        ...
    }
}
```

避免：

```rust
pub unsafe fn rows(&self) -> &[Row]
```

除非调用者必须承担额外安全责任，否则不得暴露 `unsafe fn`。

---

# Unsafe Function Requirements

## Rule 4: unsafe fn 必须存在调用者责任

以下情况允许使用 `unsafe fn`：

- 接收裸指针
- 接收未验证句柄
- 调用者必须保证生命周期
- 调用者必须保证线程安全
- 调用者必须保证内存布局

示例：

```rust
unsafe fn read_wide_string(ptr: *const u16)
```

因为调用者必须保证：

- 指针有效
- UTF16 数据合法
- NUL 终止
- 生命周期足够长

---

## Rule 5: 所有 unsafe fn 必须包含 Safety 文档

格式：

```rust
/// # Safety
///
/// 调用者必须保证：
/// 1. ...
/// 2. ...
/// 3. ...
unsafe fn foo(...)
```

缺少 Safety 文档禁止合并。

---

# SAFETY Comment Requirements

## Rule 6: 每个 unsafe 块必须有 SAFETY 注释

禁止：

```rust
unsafe {
    FreeMibTable(ptr);
}
```

必须：

```rust
// SAFETY:
// ptr 由 GetIfTable2 成功返回。
// Windows API 要求使用 FreeMibTable 释放。
// 当前作用域拥有唯一释放责任。
unsafe {
    FreeMibTable(ptr);
}
```

---

## Rule 7: SAFETY 注释必须证明契约，禁止形式主义

禁止任何“为了完成而完成”的废话注释。注释必须能够实质解答“为什么没有 Undefined Behavior”。

**禁止以下敷衍表述**：
- `// SAFETY: 应该没问题` / `// SAFETY: 只要参数对就没问题`
- `// SAFETY: Windows 保证`（无具体参考来源）
- `// SAFETY: 句柄有效，调用安全`（未解释句柄有效性的来源）

必须具体说明：
- **数据与句柄的来源**：谁创建的，生命周期如何在此处得到保证？
- **前置条件与验证**：API 有哪些安全性要求，代码在调用前通过什么手段（如判空、有效性校验、操作系统不变性约束）满足了这些要求？

---

## Rule 7.1: 大 unsafe 块必须进行结构化证明

对于包含多行调用或原子事务的 `unsafe` 块，禁止使用一句话概括。必须进行**结构化拆解证明**：

1. **输入与依赖校验**：明确指出传入的所有外部变量/句柄在何处被验证为有效。
2. **状态不变性约束**：说明多行操作之间的状态过渡关系，为什么这个整体事务不会留下中间的非法状态。

### 正反示例对比：以窗口状态转换为例

❌ **反面教材（流于形式的废话注释）**：
```rust
// SAFETY: hwnd 有效，这个调用是安全的。
unsafe {
    let _ = SetParent(hwnd, Some(h_taskbar));
    SetWindowLongPtrW(hwnd, GWL_STYLE, (WS_CHILD.0 | WS_VISIBLE.0) as isize);
}
```

*（理由：完全没有说明 `hwnd` 为什么一定有效，也没有解释为什么要同时设置样式，以及这两步操作之间的关联性。）*

✔️ **正面教材（结构化证明规范）**：
```rust
// SAFETY:
// 1. 输入与依赖校验：
//    - hwnd 为当前窗口过程 (wnd_proc) 接收到的、由操作系统保证有效的当前窗口句柄。
//    - h_taskbar 已在前文通过 FindWindowW 成功检索，并经过了 is_invalid() 校验确保不为空。
// 2. 状态不变性约束：
//    - SetParent 暂时剥离了 WS_EX_LAYERED 样式，属于中间不稳定状态。
//    - 块内必须紧接着调用 SetWindowLongPtrW 覆盖 GWL_STYLE，将其重设为 WS_CHILD 样式，确保窗口层级关系与渲染树的完整性。
//    - 这两步构成了一个不可拆分的状态重置事务，从而避免了窗口渲染状态混乱导致的未定义行为。
unsafe {
    let _ = SetParent(hwnd, Some(h_taskbar));
    SetWindowLongPtrW(hwnd, GWL_STYLE, (WS_CHILD.0 | WS_VISIBLE.0) as isize);
}
```

---

## Rule 7.2: 示例示范
对于常规 FFI 的注释范式：

```rust
// SAFETY:
// adapter 来源于成功返回的 GetAdaptersAddresses。
// Windows 文档保证 FriendlyName 为有效 UTF16 NUL 终止字符串。
// 满足 read_wide_string 的前置条件。
```

---

# Raw Pointer Policy

## Rule 8: 裸指针必须尽快转换为安全类型

优先：

```rust
let slice = unsafe {
    std::slice::from_raw_parts(ptr, len)
};
```

避免：

```rust
for i in 0..len {
    unsafe {
        ptr.add(i)
    }
}
```

---

## Rule 9: FAM 必须封装

对于：

```c
Table[ANY_SIZE]
```

等 Flexible Array Member：

禁止业务代码直接使用：

```rust
ptr.add(i)
```

必须封装为：

```rust
table.rows()
```

等安全接口。

---

# Resource Management

## Rule 10: FFI 资源必须 RAII 化

禁止：

```rust
let ptr = alloc();

...

free(ptr);
```

必须：

```rust
struct Resource(...);

impl Drop for Resource {
    fn drop(&mut self) {
        ...
    }
}
```

适用：

- HANDLE
- HKEY
- HDC
- HBITMAP
- COM 对象
- FFI 分配内存

---

## Rule 11: 禁止依赖手工释放

不得依赖：

- return 路径
- break 路径
- panic 不发生

资源释放必须由 Drop 保证。

---

# Concurrency Policy

## Rule 12: 禁止新增 static mut

项目禁止使用：

```rust
static mut
```

如需全局状态，使用：

- Atomic\*
- OnceLock
- LazyLock
- Mutex
- RwLock

---

## Rule 13: unsafe impl Send/Sync 需要专项审查

任何：

```rust
unsafe impl Send
unsafe impl Sync
```

必须提供：

- 数据所有权分析
- 线程模型分析
- 生命周期证明

且必须经过独立审查。

---

# FFI Policy

## Rule 14: FFI 边界不得传播 panic

禁止：

```rust
extern "system" fn callback(...) {
    panic!();
}
```

必须：

```rust
extern "system" fn callback(...) {
    let _ = std::panic::catch_unwind(|| {
        ...
    });
}
```

或确保：

```toml
panic = "abort"
```

并记录原因。

---

## Rule 15: FFI 输入默认不可信

所有来自：

- Win32
- COM
- 驱动
- C API
- 外部库

的数据均视为不可信。

必须验证：

- null
- 长度
- 对齐
- 生命周期

之后才能转换为 Rust 类型。

---

# Review Checklist

新增 unsafe 代码前必须回答：

- 为什么 Safe Rust 不可行？
- 为什么不能使用现有封装？
- 为什么 unsafe 范围最小？
- 调用者是否承担责任？
- 是否需要 unsafe fn？
- 是否有完整 Safety 文档？
- 是否有 SAFETY 注释？
- 是否可以 RAII 化？
- 是否涉及 Send/Sync？
- 是否可能跨 FFI 传播 panic？

任意问题无法明确回答时，不允许合并。
