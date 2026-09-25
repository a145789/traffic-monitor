//! 窗口创建与任务栏嵌入：窗口类注册、主窗口创建、任务栏查找、嵌入与位置更新。

use std::sync::atomic::{AtomicBool, Ordering};
use windows::Win32::Foundation::{COLORREF, GetLastError, HWND, RECT, SetLastError, WIN32_ERROR};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, FindWindowExW, FindWindowW, GWL_EXSTYLE, GWL_STYLE, GetParent,
    GetWindowLongPtrW, GetWindowRect, HWND_TOP, IsWindow, LWA_COLORKEY, RegisterClassExW,
    SET_WINDOW_POS_FLAGS, SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER,
    SWP_SHOWWINDOW, SetLayeredWindowAttributes, SetParent, SetWindowLongPtrW, SetWindowPos,
    WINDOW_EX_STYLE, WINDOW_STYLE, WNDCLASSEXW, WNDPROC, WS_CHILD, WS_EX_LAYERED, WS_EX_NOACTIVATE,
    WS_EX_TOOLWINDOW, WS_OVERLAPPED, WS_POPUP, WS_VISIBLE,
};
use windows::core::{PCWSTR, w};

use crate::config::{
    COLOR_KEY, DISPLAY_HEIGHT, DISPLAY_WIDTH, GAP, WATCHDOG_CLASS, WINDOW_CLASS, WINDOW_TITLE,
};
use crate::state::DPI_DIRTY;
use crate::util::{AtomicHwnd, diag, dpi_scaled, log_event, module_instance};

static TASKBAR_HWND: AtomicHwnd = AtomicHwnd::new();
/// 看门狗窗口句柄；`None` 表示尚未创建。
///
/// 看门狗是整条生命周期内唯一不重建的顶层窗口，因此它同时是
/// `--quit` 退出请求、更新交接消息与主题广播的稳定落点（见 `crate::main`）。
static WATCHDOG_HWND: AtomicHwnd = AtomicHwnd::new();
/// 当前主窗口是否已完成整条嵌入序列（含分层属性）。
///
/// 只有 `embed_in_taskbar` 全链成功才置位；发起任何一次嵌入前先清位，
/// 使 `reembed_if_lost` 能在半嵌入（父窗口已换但样式/分层未生效，窗口不可见）
/// 的状态下继续重试。新建主窗口必然未嵌入，同样先清位。
static EMBEDDED: AtomicBool = AtomicBool::new(false);

/// 只读观测 `EMBEDDED`（`#[cfg(test)]` 冒烟测试用，不暴露写入口）。
///
/// 写方：`create_main_window`（新建即清位）、`embed_in_taskbar`（先清、五步全成功后置位）；
/// 读方：`reembed_if_lost`、`update_taskbar_position`、`resize_embedded_window` 与冒烟测试。
/// 收敛：任何嵌入失败都保持 false，由周期守卫静默重试到成功为止。
#[cfg(test)]
pub(crate) fn is_embedded() -> bool {
    EMBEDDED.load(Ordering::Acquire)
}

thread_local! {
    /// 上一次成功提交的**完整**目标矩形（含宽高）。只在 `SetWindowPos` 成功且
    /// 尺寸当真生效时提交（见 `update_taskbar_position`）；`invalidate_last_rect`
    /// 无条件失效。
    ///
    /// 提到模块作用域而不是函数内局部：嵌入成功与 DPI 事务都需要主动失效它，
    /// 否则「矩形 == 缓存」会让下个 tick 直接跳过定位，把未生效的几何记成已生效。
    /// 仅 UI 线程访问，故用 `Cell` 而非原子。
    static LAST_RECT: std::cell::Cell<Option<(i32, i32, i32, i32)>> = const { std::cell::Cell::new(None) };
}

/// 失效任务栏位置缓存。
///
/// 调用点即「几何提交路径」：`embed_in_taskbar` 成功（整套几何按当前 DPI 重新算过）
/// 与 DPI 恢复事务第 3 步（尺寸由事务第 2 步提交）。不失效的后果是缓存里那份
/// 未生效的矩形被当成「已到位」，窗口永久停在旧尺寸而位图已是新 DPI 版式。
pub fn invalidate_last_rect() {
    LAST_RECT.with(|c| c.set(None));
}

fn register_class(class_name: &str, proc: WNDPROC, err: &str) -> Result<(), String> {
    // 类名常量已含尾 NUL，直接编码后原样保留；指针仅在本次注册调用期间使用。
    let class_wide: Vec<u16> = class_name.encode_utf16().collect();
    let hinstance = module_instance()?;

    let wnd_class = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        lpfnWndProc: proc,
        hInstance: hinstance,
        lpszClassName: PCWSTR(class_wide.as_ptr()),
        ..Default::default()
    };

    // SAFETY: class_wide 与 wnd_class 在 RegisterClassExW 返回前存活。
    let atom = unsafe { RegisterClassExW(&wnd_class) };
    if atom == 0 {
        return Err(err.to_string());
    }
    Ok(())
}

/// 窗口创建参数：类名 / 标题 / 尺寸 / 样式。错误文案不再由调用方传来，
/// 由 `create_window` 按类名统一生成（调用方不再既给上下文又交文案控制权）。
struct WindowSpec<'a> {
    class_name: &'a str,
    title: &'a [u16],
    width: i32,
    height: i32,
    style: WINDOW_STYLE,
    ex_style: WINDOW_EX_STYLE,
}

fn create_window(spec: &WindowSpec) -> Result<HWND, String> {
    // 类名常量已含尾 NUL，title 由调用方保证含尾 NUL；指针仅在本次创建调用期间使用。
    let class_wide: Vec<u16> = spec.class_name.encode_utf16().collect();
    let hinstance = module_instance()?;

    // SAFETY: 宽字符串缓冲区在 CreateWindowExW 返回前存活；title 含尾 NUL。
    let hwnd = unsafe {
        CreateWindowExW(
            spec.ex_style,
            PCWSTR(class_wide.as_ptr()),
            PCWSTR(spec.title.as_ptr()),
            spec.style,
            0,
            0,
            spec.width,
            spec.height,
            None,
            None,
            Some(hinstance),
            None,
        )
    };

    // 类名常量含尾 NUL，trim 后再嵌入文案。
    hwnd.map_err(|e| {
        format!(
            "创建{}窗口失败: {e:?}",
            spec.class_name.trim_end_matches('\0')
        )
    })
}

pub fn register_window_class() -> Result<(), String> {
    register_class(WINDOW_CLASS, Some(crate::wnd_proc), "注册窗口类失败")
}

pub fn create_main_window() -> Result<HWND, String> {
    // WINDOW_TITLE 常量已含尾 NUL。
    let window_name: Vec<u16> = WINDOW_TITLE.encode_utf16().collect();
    let hwnd = create_window(&WindowSpec {
        class_name: WINDOW_CLASS,
        title: &window_name,
        width: DISPLAY_WIDTH,
        height: DISPLAY_HEIGHT,
        style: WS_POPUP | WS_VISIBLE,
        ex_style: WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
    })?;
    // 新窗口尚未嵌入；清位让随后嵌入失败时由 reembed_if_lost 继续兜底。
    EMBEDDED.store(false, Ordering::Release);
    Ok(hwnd)
}

/// 注册看门狗窗口类：隐藏顶层消息窗口，唯一可靠的 TaskbarCreated 接收者。
pub fn register_watchdog_class() -> Result<(), String> {
    register_class(
        WATCHDOG_CLASS,
        Some(crate::watchdog_wnd_proc),
        "注册看门狗窗口类失败",
    )
}

/// 创建隐藏的顶层看门狗窗口。
///
/// 主窗口 SetParent 进任务栏后成为跨进程子窗口，explorer 销毁任务栏时会将其
/// 级联销毁，且 TaskbarCreated 广播只投递顶层窗口——主窗口自身永远收不到。
/// 看门狗永不嵌入、从不显示（无 GDI 位图/DC），常驻开销可忽略。
pub fn create_watchdog_window() -> Result<HWND, String> {
    const EMPTY_TITLE: [u16; 1] = [0];
    let hwnd = create_window(&WindowSpec {
        class_name: WATCHDOG_CLASS,
        title: &EMPTY_TITLE,
        width: 0,
        height: 0,
        style: WS_OVERLAPPED,
        ex_style: WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
    })?;
    // 发布句柄供 update 等模块投递控制消息：它们不再快照易失效的主窗口句柄。
    WATCHDOG_HWND.store(hwnd);
    Ok(hwnd)
}

/// 当前看门狗窗口句柄；尚未创建（或已销毁）时返回 None。
///
/// 调用方用它作为控制消息落点，句柄有效性由内核在投递时裁决：
/// `PostMessageW` 对已销毁句柄返回错误，调用方据此走兜底路径。
pub fn watchdog_hwnd() -> Option<HWND> {
    let hwnd = WATCHDOG_HWND.load()?;
    // SAFETY: IsWindow 是纯查询，对任意句柄值安全返回布尔。
    unsafe { IsWindow(Some(hwnd)) }.as_bool().then_some(hwnd)
}

pub fn get_taskbar_hwnd() -> Option<HWND> {
    if let Some(hwnd) = TASKBAR_HWND.load() {
        if unsafe { IsWindow(Some(hwnd)) }.as_bool() {
            return Some(hwnd);
        }
        TASKBAR_HWND.clear();
    }
    // SAFETY: 静态类名 "Shell_TrayWnd"；FindWindowW 仅查询句柄。
    let hwnd = unsafe { FindWindowW(w!("Shell_TrayWnd"), w!("")).ok() };
    if let Some(h) = hwnd {
        TASKBAR_HWND.store(h);
    }
    hwnd
}

/// 重置任务栏句柄缓存（由 `TaskbarCreated` 消息触发）。
pub fn invalidate_taskbar_cache() {
    TASKBAR_HWND.clear();
}

/// 计算小组件在任务栏上的目标矩形 (x, y, w, h)；仅 window.rs 内部消费。
fn calc_widget_rect(hwnd: HWND) -> Option<(i32, i32, i32, i32)> {
    let h_taskbar = get_taskbar_hwnd()?;
    // SAFETY: "TrayNotifyWnd" 为系统托盘子窗口类名。
    let h_tray = unsafe { FindWindowExW(Some(h_taskbar), None, w!("TrayNotifyWnd"), w!("")).ok()? };

    let mut rc_tray = RECT::default();
    let mut rc_taskbar = RECT::default();
    unsafe {
        GetWindowRect(h_tray, &mut rc_tray).ok()?;
        GetWindowRect(h_taskbar, &mut rc_taskbar).ok()?;
    }

    let dpi = unsafe { windows::Win32::UI::HiDpi::GetDpiForWindow(hwnd) };
    let display_width = dpi_scaled(DISPLAY_WIDTH, dpi);
    let display_height = dpi_scaled(DISPLAY_HEIGHT, dpi);
    let gap = dpi_scaled(GAP, dpi);

    let display_x = rc_tray.left - rc_taskbar.left - gap - display_width;
    let display_y = (rc_taskbar.bottom - rc_taskbar.top - display_height) / 2;

    Some((display_x, display_y, display_width, display_height))
}

/// 执行完整嵌入序列。成功即分层属性已就位、窗口可见。
///
/// 失败返回具体原因，**不弹框**：文案与是否提示由调用方决定——首轮嵌入需提示用户，
/// 而 `reembed_if_lost` 的周期重试必须静默，否则会退化成模态弹窗风暴。
/// 任何一环失败都会留下不可见或不可用的半嵌入窗口，故全程不置 `EMBEDDED`。
pub fn embed_in_taskbar(hwnd: HWND) -> Result<(), String> {
    // 先清位：序列中途失败时状态位必须保持 false，让周期守卫继续重试。
    EMBEDDED.store(false, Ordering::Release);

    let (display_x, display_y, display_width, display_height) = calc_widget_rect(hwnd)
        .ok_or_else(|| "找不到 Shell_TrayWnd 或 TrayNotifyWnd".to_string())?;

    let h_taskbar = get_taskbar_hwnd().ok_or_else(|| "找不到 Shell_TrayWnd".to_string())?;

    // SAFETY:
    // 1. 输入与依赖校验：hwnd 与 h_taskbar 均为已通过有效性校验的窗口句柄；
    //    rect 由 calc_widget_rect 计算得到的有效几何。
    // 2. 状态不变性约束：必须严格按 AGENTS.md 既定顺序执行
    //    （SetParent → GWL_STYLE → 重新应用 WS_EX_LAYERED → SetWindowPos →
    //    SetLayeredWindowAttributes）。调换会导致分层透明失效或被任务栏图标遮挡。
    //    任一步失败立即返回 Err，避免任务栏嵌入进入不可恢复的中间状态。
    unsafe {
        // 已评估、采取现状：SetParent 的 HWND 返回值有歧义（NULL 既可能是「前值」
        // 也可能表示失败，且 Win32 不保证失败时 set last error），故此处不加
        // GetLastError 判别分支——忽略 Err 会让后续序列作用在未 reparent 的窗口上，
        // 比现状「Err 即失败、由守卫静默重试」更不安全。本机 Win11 实测该路径返回
        // 桌面句柄而非 NULL，现状不误报。同函数内 SetWindowLongPtrW 用的是
        // SetLastError(0) 加事后判别，见下方各段。
        SetParent(hwnd, Some(h_taskbar)).map_err(|e| format!("SetParent 嵌入任务栏失败: {e:?}"))?;

        // SetWindowLongPtrW 返回 isize（前值），0 既可能表示"前值就是 0"也可能表示失败，
        // 必须先 SetLastError(WIN32_ERROR(0)) 再调用，事后用 GetLastError 才能可靠区分。
        // TODO(dedup-setlong): 本段与下方 GWL_EXSTYLE 段的判别协议可抽同一私有 helper 收口；
        // 为保住嵌入五步序列的逐字可核对性，暂各留一份。
        SetLastError(WIN32_ERROR(0));
        let prev_style = SetWindowLongPtrW(hwnd, GWL_STYLE, (WS_CHILD.0 | WS_VISIBLE.0) as isize);
        if prev_style == 0 {
            let last = GetLastError();
            if last.0 != 0 {
                return Err(format!("覆盖 GWL_STYLE 失败: 0x{:08X}", last.0));
            }
        }

        let current_ex_style = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        SetLastError(WIN32_ERROR(0));
        let prev_ex = SetWindowLongPtrW(
            hwnd,
            GWL_EXSTYLE,
            current_ex_style | (WS_EX_LAYERED.0 as isize),
        );
        if prev_ex == 0 {
            let last = GetLastError();
            if last.0 != 0 {
                return Err(format!("重新应用 WS_EX_LAYERED 失败: 0x{:08X}", last.0));
            }
        }

        SetWindowPos(
            hwnd,
            Some(HWND_TOP),
            display_x,
            display_y,
            display_width,
            display_height,
            SWP_NOACTIVATE | SWP_SHOWWINDOW | SWP_FRAMECHANGED,
        )
        .map_err(|e| format!("SetWindowPos 嵌入任务栏失败: {e:?}"))?;

        // 末步决定分层窗口能否显示（WS_EX_LAYERED 窗口在成功调用本函数前不会显示），
        // 故此处失败等同面板消失，必须让守卫重试而不是就此收手。
        SetLayeredWindowAttributes(hwnd, COLORREF(COLOR_KEY), 0, LWA_COLORKEY)
            .map_err(|e| format!("设置分层窗口属性失败: {e:?}"))?;
    }

    // 后置断言：`SetParent` 的返回值有歧义（NULL 既可能是前一个父窗口也可能表示
    // 失败，且 Win32 不保证失败时 set last error，见上方注释），唯一可信的判据是
    // 事后查询真实父子关系。不成立则保持 `EMBEDDED` 为 false，交给 `reembed_if_lost`
    // 的周期重试——绝不留下「父窗口没换却记为已嵌入」的假活状态。
    // SAFETY: hwnd 是刚走完五步序列的窗口句柄；GetParent 只查询父子关系，
    // 对跨进程父窗口同样有效，不引用任何调用方内存。
    if unsafe { GetParent(hwnd) }.ok() != Some(h_taskbar) {
        return Err("SetParent 后 GetParent 与任务栏不一致".to_string());
    }

    // 几何已按当前 DPI 整体重算并提交：失效位置缓存，让下个 tick 重新比对而不是
    // 因「矩形 == 缓存」跳过（多一次 SetWindowPos 属无害冗余）。
    invalidate_last_rect();
    EMBEDDED.store(true, Ordering::Release);
    Ok(())
}

/// 把窗口物理尺寸对齐到指定位图尺寸（**只改尺寸**：`SWP_NOMOVE`，位置与 Z 序不动）。
///
/// 与 [`resize_embedded_window`] 的区别是**不检查** `EMBEDDED`：本函数不应用任何
/// 任务栏客户区坐标（位置分量由 `SWP_NOMOVE` 保持不动），只提交宽高，因此对半嵌入
/// 或未嵌入的窗口同样安全。它要修的正是「位图已按新 DPI 换掉、窗口还是旧尺寸」这一
/// 错配——而嵌入门在那种状态（`embed_in_taskbar` 中途失败会把 `EMBEDDED` 清成 false）
/// 恰好会拒绝回滚，于是错配只能拖到下一次成功嵌入。
pub fn align_window_size_to(hwnd: HWND, width: i32, height: i32) {
    // SAFETY: hwnd 是当前主窗口；SWP_NOMOVE 保留现位置，SWP_FRAMECHANGED 让样式变更
    // 生效，SWP_NOZORDER 不动层级，均不涉及跨进程内存。
    unsafe {
        let _ = SetWindowPos(
            hwnd,
            None,
            0,
            0,
            width,
            height,
            SWP_NOACTIVATE | SWP_FRAMECHANGED | SWP_NOMOVE | SWP_NOZORDER,
        );
    }
}

/// 把已嵌入窗口的物理尺寸重置为指定位图尺寸（DPI 资源重建失败的回滚入口）。
///
/// 只在已嵌入时生效：未嵌入的窗口几何归嵌入序列所有。跨屏后的合身位置与尺寸
/// 由恢复调度器的 DPI 事务或下个成功周期自愈。保证 BitBlt 源（位图）与目标（窗口）
/// 尺寸一致，避免边缘露出色键底色。
pub fn resize_embedded_window(hwnd: HWND, width: i32, height: i32) {
    if !EMBEDDED.load(Ordering::Acquire) {
        return;
    }
    align_window_size_to(hwnd, width, height);
}

/// 嵌入自愈守卫，挂在常驻的全屏检测定时器上（每 2s）。
///
/// `TaskbarCreated` 每次任务栏创建只广播一次：重建那一刻嵌入失败后不会再有第二轮
/// 广播，而没有末步 `SetLayeredWindowAttributes` 的窗口永远不会显示——面板就这么
/// 永久消失。因此必须由周期 tick 静默补做，成功为止。
///
/// 返回 true 表示本轮真的重新嵌入过，调用方需要重绘。
pub fn reembed_if_lost(hwnd: HWND) -> bool {
    if EMBEDDED.load(Ordering::Acquire) && parent_is_current_taskbar(hwnd) {
        return false;
    }
    match embed_in_taskbar(hwnd) {
        Ok(()) => true,
        Err(e) => {
            diag!("周期重嵌入任务栏失败: {e}");
            log_event!("周期重嵌入任务栏失败: {e}");
            false
        }
    }
}

/// 窗口父级是否仍是当前任务栏。explorer 崩溃（非干净退出）时任务栏句柄失效而子
/// 窗口幸存，此时仅凭 `EMBEDDED` 会误判为已嵌入。
fn parent_is_current_taskbar(hwnd: HWND) -> bool {
    match get_taskbar_hwnd() {
        // SAFETY: GetParent 只查询父子关系，跨进程子窗口同样有效；父窗口无效时返回 Err。
        Some(h_taskbar) => unsafe { GetParent(hwnd).ok() == Some(h_taskbar) },
        // 任务栏暂不存在（Explorer 重启竞态）：无法比较，维持现状不抢跑。新任务栏建立
        // 后由 TaskbarCreated 触发完整重建，那才是这条路径的权威处理者。
        None => true,
    }
}

/// 任务栏定位的 `SetWindowPos` 标志集合（纯判定，便于单测）。
///
/// `dpi_dirty` 期间追加 `SWP_NOSIZE`：此时渲染器位图尺寸与实际 DPI 已经错位，
/// 尺寸的提交权归 DPI 恢复事务（`main::recover_dpi`）；若定位路径也改尺寸，事务
/// 失败后的回滚会在下个 tick 被再次推翻，窗口尺寸将停在位图尺寸不匹配的一方。
fn position_flags(dpi_dirty: bool) -> SET_WINDOW_POS_FLAGS {
    let base = SWP_NOACTIVATE | SWP_FRAMECHANGED | SWP_NOZORDER;
    if dpi_dirty { base | SWP_NOSIZE } else { base }
}

pub fn update_taskbar_position(hwnd: HWND) -> bool {
    // 未嵌入时禁止按任务栏几何移动：calc_widget_rect 给出的是相对任务栏客户区的
    // 坐标，对顶层窗口会被 SetWindowPos 当作屏幕坐标，反而把面板钉到无关位置。
    if !EMBEDDED.load(Ordering::Acquire) {
        return false;
    }

    let Some((display_x, display_y, display_width, display_height)) = calc_widget_rect(hwnd) else {
        return false;
    };

    let target = (display_x, display_y, display_width, display_height);
    let dpi_dirty = DPI_DIRTY.load(Ordering::Acquire);
    if LAST_RECT.with(|lp| lp.get()) == Some(target) {
        return false;
    }

    let moved = unsafe {
        SetWindowPos(
            hwnd,
            None,
            display_x,
            display_y,
            display_width,
            display_height,
            position_flags(dpi_dirty),
        )
        .is_ok()
    };
    // 缓存只在「移动成功且本次真的提交了完整几何」时写入：SetWindowPos 瞬时失败
    // （如 Explorer 重启竞态）时下个周期会重试，而不是因「矩形 == 缓存」被永久跳过；
    // 脏位期间的调用带 `SWP_NOSIZE`，尺寸分量并未生效，提交缓存等于把未生效的尺寸
    // 记成已生效。
    if moved && !dpi_dirty {
        LAST_RECT.with(|lp| lp.set(Some(target)));
    }
    moved
}

#[cfg(test)]
mod tests {
    //! 只覆盖纯判定：`position_flags` 的标志选择与位置缓存的失效。真实窗口行为不在本模块单测范围内。

    use super::{LAST_RECT, invalidate_last_rect, position_flags};
    use windows::Win32::UI::WindowsAndMessaging::{SWP_NOSIZE, SWP_NOZORDER};

    #[test]
    fn dpi_dirty_position_keeps_window_size() {
        // 脏位期间只改位置：尺寸由 DPI 事务提交，定位路径不得越权改尺寸。
        assert!(position_flags(true).contains(SWP_NOSIZE));
        assert!(!position_flags(false).contains(SWP_NOSIZE));
        // 两个分支都必须保持「不改 Z 序」，否则面板会被拉到任务栏图标之上/之下。
        assert!(position_flags(true).contains(SWP_NOZORDER));
        assert!(position_flags(false).contains(SWP_NOZORDER));
    }

    #[test]
    fn invalidate_last_rect_clears_committed_cache() {
        LAST_RECT.with(|c| c.set(Some((1, 2, 3, 4))));
        invalidate_last_rect();
        assert_eq!(LAST_RECT.with(|c| c.get()), None);
    }
}
