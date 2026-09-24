#![windows_subsystem = "windows"]

mod collector;
mod config;
mod ffi_guard;
mod renderer;
mod state;
mod suspend;
mod tray;
mod update;
mod util;
mod window;

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use windows::Win32::Foundation::{
    CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HANDLE, HWND, LPARAM, LRESULT, WPARAM,
};
use windows::Win32::Graphics::Gdi::{BeginPaint, EndPaint, InvalidateRect, PAINTSTRUCT};
use windows::Win32::System::Power::{
    RegisterPowerSettingNotification, UnregisterPowerSettingNotification,
};
use windows::Win32::System::RemoteDesktop::{
    NOTIFY_FOR_THIS_SESSION, WTSRegisterSessionNotification, WTSUnRegisterSessionNotification,
};
use windows::Win32::System::SystemServices::GUID_MONITOR_POWER_ON;
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::Input::Ime::ImmDisableIME;
use windows::Win32::UI::WindowsAndMessaging::{
    DEVICE_NOTIFY_WINDOW_HANDLE, DefWindowProcW, DestroyWindow, DispatchMessageW, FindWindowW,
    GetMessageW, IsWindow, KillTimer, MSG, PostMessageW, PostQuitMessage, RegisterWindowMessageW,
    SetTimer, TranslateMessage, WM_CLOSE, WM_CONTEXTMENU, WM_CREATE, WM_DPICHANGED, WM_PAINT,
    WM_POWERBROADCAST, WM_SETTINGCHANGE, WM_TIMER, WM_WTSSESSION_CHANGE,
};
use windows::core::{PCWSTR, w};

use crate::collector::{collect_cpu, collect_memory, collect_network};
use crate::config::{
    LOWORD_MASK, MAIN_EXIT_POLL_INTERVAL_MS, MAIN_EXIT_WAIT_TIMEOUT_MS, RELAUNCHED_BY_UPDATE_ARG,
    TIMER_ID_AUTO_UPDATE, TIMER_ID_CPU_MEM, TIMER_ID_FULLSCREEN, TIMER_ID_INIT_TRIM,
    TIMER_ID_NETWORK, TIMER_ID_REBUILD_RETRY, TIMER_INTERVAL_INIT_TRIM,
    TIMER_INTERVAL_REBUILD_RETRY_MAX, TIMER_INTERVAL_REBUILD_RETRY_MIN, WATCHDOG_CLASS,
    WM_APP_TRAY, WM_USER_NETWORK_DISCONNECTED, WM_USER_NETWORK_RECONNECTED, WM_USER_QUIT_REQUEST,
    WM_USER_UPDATE_ACTION,
};
use crate::renderer::Renderer;
use crate::state::{ENABLE_AUTO_UPDATE, MONITOR_FULLSCREEN, reset_network_backoff};
use crate::suspend::{
    check_fullscreen, handle_power_broadcast, handle_session_change, is_immersive_color_set,
    is_suspended, sync_monitoring_timers,
};
use crate::tray::{create_tray_icon, remove_tray_icon};
use crate::update::{
    acquire_update_mutex, defer_initial_auto_check, emit_protocol_line, init_cleanup_temp,
    load_auto_update_enabled, start_auto_check, subprocess_main,
};
use crate::util::{
    AtomicHwnd, AtomicPowerNotify, diag, log_event, refresh_debug_log_flag,
    set_low_memory_priority, show_error, trim_working_set,
};
use crate::window::{
    create_main_window, create_watchdog_window, embed_in_taskbar, invalidate_taskbar_cache,
    reembed_if_lost, register_watchdog_class, register_window_class, resize_embedded_window,
    update_taskbar_position,
};

static TASKBAR_CREATED_MSG: AtomicU32 = AtomicU32::new(0);
static POWER_NOTIFY_HANDLE: AtomicPowerNotify = AtomicPowerNotify::new();
/// 当前主窗口句柄。Explorer 重启重建后更新；`None` 表示暂无主窗口。
static CURRENT_MAIN_HWND: AtomicHwnd = AtomicHwnd::new();
/// 已注册会话通知的窗口句柄；`None` 表示当前无注册。重建路径据此在销毁
/// 旧窗口前配对注销，避免每次 Explorer 重启留下悬空注册。
static SESSION_NOTIFY_HWND: AtomicHwnd = AtomicHwnd::new();
/// 退出请求是否已受理。`--quit` 可因超时重试或多次调用重复到达，
/// 退出序列（托盘清理 + `PostQuitMessage`）只应执行一次。
static EXIT_REQUESTED: AtomicBool = AtomicBool::new(false);

thread_local! {
    /// 主窗口重建重试的当前间隔（毫秒）；0 表示不在重试序列中。
    /// 仅 UI 线程（看门狗过程与重建路径）读写。
    static REBUILD_RETRY_INTERVAL_MS: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// 启动参数一次性解析结果：`--quit` / `--check-update` / `--manual` / 更新拉起标记
/// / 父身份绑定参数（`--parent-pid` 后的 PID 与 `--parent-start` 后的 FILETIME）。
///
/// 单一事实来源：`main()` 只扫描一次 `args_os`。优先级由 `main()` 开头的
/// 检查顺序钉死（`--quit` 先于 `--check-update`），不再散落于多次线性扫描中。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct CliArgs {
    quit: bool,
    check_update: bool,
    manual: bool,
    relaunched_by_update: bool,
    /// 父进程 PID；值为 `Option`：缺失或非数字时保持 `None`，子进程据此退化为
    /// 「无父可查」（恒视为父进程仍在），而不是按「父已退出」误杀手工调用。
    parent_pid: Option<u32>,
    /// 父进程创建时刻（`GetProcessTimes` 的 FILETIME）。只传 PID 不足以判定父身份：
    /// 长时间开机的机器上 PID 会被复用，必须靠创建时刻复核。
    parent_start: Option<u64>,
}

/// 一次遍历解析启动参数。`args_os` 不要求参数为合法 Unicode，
/// 含非 UTF-8/非 UTF-16 可表示字符的无关参数只会被忽略，不再 panic。
/// 比较为精确匹配：`--quit=1` 这类缀接形式判否，与旧 `==` 语义一致；
/// `--parent-pid` / `--parent-start` 取紧随其后的一个参数为值，值不合形状即当缺席。
fn parse_cli_args(args: impl IntoIterator<Item = std::ffi::OsString>) -> CliArgs {
    use std::ffi::OsStr;
    let mut cli = CliArgs::default();
    let mut args = args.into_iter().peekable();
    while let Some(arg) = args.next() {
        let s = arg.as_os_str();
        if s == OsStr::new("--quit") {
            cli.quit = true;
        } else if s == OsStr::new("--check-update") {
            cli.check_update = true;
        } else if s == OsStr::new("--manual") {
            cli.manual = true;
        } else if s == OsStr::new(RELAUNCHED_BY_UPDATE_ARG) {
            cli.relaunched_by_update = true;
        } else if s == OsStr::new(crate::config::PARENT_PID_ARG) {
            cli.parent_pid = take_parent_value(&mut args);
        } else if s == OsStr::new(crate::config::PARENT_START_ARG) {
            cli.parent_start = take_parent_value(&mut args);
        }
    }
    cli
}

/// 取父身份参数紧随其后的十进制值。
///
/// 值不合形状（下一个参数本身是标志、非 UTF-8、或不是数字）一律按「参数缺席」处理，
/// 子进程据此退化为「无父可查」而不是「父已退出」。标志形态的候选值**不消费**：
/// 否则手打的 `--parent-pid --manual` 会把 `--manual` 吃掉，静默改变本次调用的语义。
fn take_parent_value<T: std::str::FromStr>(
    args: &mut std::iter::Peekable<impl Iterator<Item = std::ffi::OsString>>,
) -> Option<T> {
    match args.peek().and_then(|next| next.to_str()) {
        Some(text) if !text.starts_with("--") => {}
        _ => return None,
    }
    // 值已确认是 UTF-8 且非标志形态，消费掉再解析；解析失败说明参数确实无效。
    args.next()?.to_str()?.parse().ok()
}

/// `--quit` 入口：把退出请求交给现存实例的看门狗窗口。
///
/// 必须查 `WATCHDOG_CLASS` 而不是 `WINDOW_CLASS`：主窗口嵌入任务栏后是
/// `WS_CHILD` 跨进程子窗口，而 `FindWindowW` 只检索顶层窗口，正常嵌入状态下
/// 查主窗口第一次必然 miss、随后的“消失轮询”也会立刻误判为已退出。看门狗是
/// 唯一全生命周期不重建的顶层窗口，既是可被检索到的锚点，也负责在重建间隙
/// （主窗口不存在时）自行完成退出序列。
fn quit_existing_instance() {
    // WATCHDOG_CLASS 常量已含尾 NUL。
    let class_name: Vec<u16> = WATCHDOG_CLASS.encode_utf16().collect();
    let class_pcw = PCWSTR(class_name.as_ptr());
    // SAFETY: class_name 以 NUL 结尾，查询不存在的窗口时安全返回错误。
    let hwnd = unsafe { FindWindowW(class_pcw, PCWSTR(std::ptr::null())) };

    if let Ok(h) = hwnd
        && !h.is_invalid()
    {
        unsafe {
            let _ = PostMessageW(Some(h), WM_USER_QUIT_REQUEST, WPARAM(0), LPARAM(0));
        }
        // 轮询看门狗窗口消失即等价于进程退出：看门狗随进程结束而销毁，
        // 中途被替换的可能性不存在（它不参与重建）。
        // 超时/轮询与子进程 `wait_main_instance_gone` 共用
        // MAIN_EXIT_WAIT_TIMEOUT_MS / MAIN_EXIT_POLL_INTERVAL_MS：两处都是
        // “等主进程退净、上限 5 秒”，仅存在性探针不同（此处看门狗窗口消失，
        // 对方单实例互斥量消失）；`installer.iss` 的 GracefulWaitTimeoutMs 同量级。
        for _ in 0..(MAIN_EXIT_WAIT_TIMEOUT_MS / MAIN_EXIT_POLL_INTERVAL_MS) {
            std::thread::sleep(std::time::Duration::from_millis(MAIN_EXIT_POLL_INTERVAL_MS));
            let exist = unsafe { FindWindowW(class_pcw, PCWSTR(std::ptr::null())) };
            if exist.is_err() {
                break;
            }
        }
    }
}

/// 创建单例互斥量。已存在实例（重复启动）静默返回 None；创建本身失败弹框后
/// 返回 None。两种情况 `main()` 都直接退出，与提取前的早退路径一一对应。
fn init_single_instance() -> Option<crate::ffi_guard::MutexGuard> {
    // MUTEX_NAME 常量已含尾 NUL。
    let mutex_name: Vec<u16> = crate::config::MUTEX_NAME.encode_utf16().collect();
    // SAFETY: mutex_name 以 NUL 结尾；句柄由 MutexGuard 关闭。
    let mutex_handle = unsafe { CreateMutexW(None, true, PCWSTR(mutex_name.as_ptr())) };

    match mutex_handle {
        Ok(handle) => {
            // SAFETY: 紧接 CreateMutexW 读取 last-error，避免被中间调用覆盖。
            let last_error = unsafe { GetLastError() };
            if last_error == ERROR_ALREADY_EXISTS {
                // 重复实例：句柄不会交给 MutexGuard，须在此自行关闭，避免
                // 「拿到句柄却不归还」这条与 RAII 归属相反的路径。
                // SAFETY: handle 由紧邻的 CreateMutexW 成功返回，仅关闭一次。
                let _ = unsafe { CloseHandle(handle) };
                return None;
            }
            Some(crate::ffi_guard::MutexGuard(handle))
        }
        Err(_) => {
            show_error("创建单例互斥量失败");
            None
        }
    }
}

fn main() {
    let cli = parse_cli_args(std::env::args_os().skip(1));
    if cli.quit {
        quit_existing_instance();
        return;
    }

    // 必须在单例 Mutex 之前拦截 --check-update，否则子进程会被当作重复实例退出。
    if cli.check_update {
        // 调试日志开关要在第一处可能写日志的调用之前加载：互斥量创建失败只在这里留痕，
        // 否则「更新总是 BUSY」在 debug.log 里查不到原因。subprocess_main 的既有步骤里
        // 还会再加载一次，幂等，代价仅一次注册表读。
        refresh_debug_log_flag();

        // 跨进程更新互斥：同一会话内只允许一个更新子进程。位置同样必须在
        // 单例 Mutex 之前——本路径刻意不持有单例锁，两者是不同作用域的锁。
        // guard 须活到进程结束：它是「本进程是唯一更新者」的存活证明。
        let Some(_update_mutex) = acquire_update_mutex() else {
            // 另一处更新子进程仍在跑（或互斥量创建失败，见 acquire_update_mutex）：
            // BUSY 是「有效动作但非成功完成」的协议行，父侧据此不把这次结果记成
            // 一次成功检查（冷却不被推进到 1 小时），也刻意不向用户弹任何框。
            let _ = emit_protocol_line("BUSY");
            std::process::exit(0);
        };
        std::process::exit(subprocess_main(
            cli.manual,
            cli.parent_pid,
            cli.parent_start,
        ));
    }

    // guard 必须活到消息循环结束：它是单例互斥量的存活证明，提前 drop 会让
    // 第二个实例通过 CreateMutexW 拦截。绑定留在 main 栈帧上。
    let _mutex_guard = match init_single_instance() {
        Some(guard) => guard,
        None => return,
    };

    // 主进程常驻期间保持低内存优先级：内存紧张时 OS 优先回收本进程页面，
    // 不影响 CPU 调度与核心选择；EcoQoS 仍只用于 --check-update 子进程。
    set_low_memory_priority();

    // 首个顶层窗口创建前禁用进程 IME，避免更新弹窗焦点回落触发第三方 TSF 常驻。
    // SAFETY: ImmDisableIME(u32::MAX) 仅改本进程输入法状态；须在 CreateWindowExW 前调用。
    unsafe {
        let _ = ImmDisableIME(u32::MAX);
    }

    if let Err(e) = register_window_class() {
        show_error(&e);
        return;
    }

    // 看门狗是首个创建的顶层窗口；ImmDisableIME 已在其之前执行。
    if let Err(e) = register_watchdog_class() {
        show_error(&e);
        return;
    }

    // RegisterWindowMessageW 失败返回 0；看门狗过程用 `tcm != 0 && msg == tcm` 防御。
    // 必须在看门狗窗口创建前注册，保证其能收到首次 TaskbarCreated 广播。
    let taskbar_msg = unsafe { RegisterWindowMessageW(w!("TaskbarCreated")) };
    if taskbar_msg == 0 {
        // SAFETY: 紧随 RegisterWindowMessageW，未插入其他可改写 last-error 的调用。
        let last = unsafe { GetLastError() };
        show_error(&format!("注册 TaskbarCreated 消息失败: 0x{:08X}", last.0));
    }
    TASKBAR_CREATED_MSG.store(taskbar_msg, Ordering::Release);

    if let Err(e) = create_watchdog_window() {
        show_error(&e);
        return;
    }

    let hwnd = match create_main_window() {
        Ok(h) => h,
        Err(e) => {
            show_error(&e);
            return;
        }
    };
    CURRENT_MAIN_HWND.store(hwnd);

    register_power_notify(hwnd);

    // 嵌入失败不中止启动：此处先完成托盘/定时器等其余常驻功能，避免留下零定时器的
    // 活窗口；失败后由全屏检测定时器上的 reembed_if_lost 静默重试到成功为止。
    if let Err(e) = embed_in_taskbar(hwnd) {
        show_error(&format!(
            "嵌入任务栏失败: {e}。请确认 explorer.exe 正在运行。"
        ));
    }

    let auto_update = load_auto_update_enabled();
    ENABLE_AUTO_UPDATE.store(auto_update, Ordering::Relaxed);
    refresh_debug_log_flag();

    match Renderer::new() {
        Ok(r) => renderer::set_renderer(r),
        Err(e) => {
            show_error(&format!("初始化渲染器失败: {e}"));
            return;
        }
    }

    if !bind_display_and_timers(hwnd) {
        show_error("创建监测定时器失败");
        // 启动失败早退同样要归还已建资源：否则任务栏会留下悬空托盘图标直到
        // 进程退出，渲染器持有的 GDI 资源也不会提前归还。
        remove_tray_icon();
        renderer::take_renderer();
        return;
    }

    register_session_notification(hwnd);

    init_cleanup_temp();
    // 更新流程 relaunch 拉起的进程：刚发生过 UAC 取消或安装器启动失败，
    // 推迟首个自动检查周期，避免立刻再弹同一版本的更新确认框。
    if cli.relaunched_by_update {
        defer_initial_auto_check();
    }
    start_auto_check();

    // 一次性定时器：到时后 trim 初始化冷页；ID 99 不与监测定时器冲突。
    unsafe {
        let _ = SetTimer(
            Some(hwnd),
            TIMER_ID_INIT_TRIM,
            TIMER_INTERVAL_INIT_TRIM,
            None,
        );
    }

    run_message_loop();

    // 注销须针对当前主窗口：Explorer 重启重建后原局部 hwnd 已陈旧。
    // 会话通知经状态位取当前注册句柄，重建路径已注销过的旧注册不会重复发。
    unregister_session_notification();

    let power_handle = POWER_NOTIFY_HANDLE.load();
    if let Some(handle) = power_handle {
        unsafe {
            let _ = UnregisterPowerSettingNotification(handle);
        }
    }

    renderer::take_renderer();
}

/// 主消息循环。`GetMessageW`：>0 有消息；0 收到 WM_QUIT；-1 致命错误须退出。
fn run_message_loop() {
    let mut msg = MSG::default();
    unsafe {
        loop {
            match GetMessageW(&mut msg, None, 0, 0).0 {
                0 => break,
                -1 => {
                    let last = GetLastError();
                    show_error(&format!("消息循环 GetMessageW 致命错误: 0x{:08X}", last.0));
                    break;
                }
                _ => {
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }
        }
    }
}

// --- 看门狗窗口与 Explorer 重启恢复 ---

/// 注册显示器开关电源通知到指定窗口。失败仅影响显示器开关节能（非致命）。
fn register_power_notify(hwnd: HWND) {
    // DEVICE_NOTIFY_WINDOW_HANDLE = 0；误用 SERVICE_HANDLE(1) 会把 HWND 当服务句柄，
    // 返回 ERROR_SERVICE_NOT_IN_EXE (0x8007043B)。
    let power_notify = unsafe {
        RegisterPowerSettingNotification(
            HANDLE(hwnd.0),
            &GUID_MONITOR_POWER_ON,
            DEVICE_NOTIFY_WINDOW_HANDLE,
        )
    };
    match power_notify {
        Ok(handle) => {
            POWER_NOTIFY_HANDLE.store(handle);
        }
        Err(e) => {
            show_error(&format!("注册电源设置通知失败: {e:?}"));
        }
    }
}

/// 注册会话锁屏通知，并记录句柄供 [`unregister_session_notification`] 配对注销。
/// 失败非致命：锁屏暂停失效，显示器关闭仍由电源通知覆盖。
fn register_session_notification(hwnd: HWND) {
    match unsafe { WTSRegisterSessionNotification(hwnd, NOTIFY_FOR_THIS_SESSION) } {
        // 只有注册成功才记位：失败时无配对可注销，记位会让状态位谎报存在注册。
        Ok(()) => SESSION_NOTIFY_HWND.store(hwnd),
        Err(e) => show_error(&format!("注册会话通知失败: {e:?}")),
    }
}

/// 配对注销会话通知并在成功取出句柄时清零状态位。
///
/// WTS 契约要求每个 `WTSRegisterSessionNotification` 都有对应的
/// `WTSUnRegisterSessionNotification`，且必须在窗口**销毁之前**调用；
/// 故重建路径在 `DestroyWindow` 旧窗口前调用本函数。窗口销毁后句柄即失效，
/// 那时再注销已无意义，这也是不能在销毁之后补做的原因。
///
/// 仅由 UI 线程调用（启动失败早退路径与 `rebuild_main_window`），
/// 与同一线程上的注册调用之间无并发写者。
fn unregister_session_notification() {
    if let Some(registered) = SESSION_NOTIFY_HWND.take() {
        unsafe {
            let _ = WTSUnRegisterSessionNotification(registered);
        }
    }
}

/// 把窗口回滚到渲染器当前的位图尺寸。
///
/// DPI 更新失败时渲染器维持旧尺寸，而窗口已按新 DPI 改过：不拉回同一尺寸，
/// BitBlt 只覆盖旧位图区域、边缘露出色键底色。
fn rollback_window_to_bitmap(hwnd: HWND) {
    renderer::with_renderer(|r| {
        let (width, height) = r.bitmap_size();
        resize_embedded_window(hwnd, width, height);
    });
}

/// 启动与 Explorer 重建共用的资源绑定尾段：托盘图标 → 渲染参数 → 窗口失效
/// → 监测定时器。两条生命周期路径保持唯一实现，失败文案由各自调用方报告。
fn bind_display_and_timers(hwnd: HWND) -> bool {
    // 托盘为 best-effort：失败不阻断监测主功能，由 diag 留痕。
    if !create_tray_icon(hwnd) {
        diag!("绑定显示与定时器: 托盘图标创建失败，本会话无图标");
        log_event!("绑定显示与定时器: 托盘图标创建失败，本会话无图标");
    }

    let mut dpi_ok = true;
    renderer::with_renderer(|r| {
        dpi_ok = r.update_dpi(hwnd);
        r.update_text_color();
    });
    if !dpi_ok {
        // 与 WM_DPICHANGED 失败分支对称，共用同一份回滚实现。
        rollback_window_to_bitmap(hwnd);
    }

    unsafe {
        let _ = InvalidateRect(Some(hwnd), None, false);
    }

    sync_monitoring_timers(hwnd)
}

/// Explorer 重启后的主窗口完整重建。
///
/// 不变量：任务栏销毁会级联销毁嵌入其中的跨进程子窗口（旧主窗口已死），
/// 且 TaskbarCreated 广播只投递顶层窗口——因此重建只能由看门狗触发，
/// 禁止把该处理挂回主窗口过程。所有绑定在旧 hwnd 上的资源
/// （电源/会话通知、托盘、定时器）必须逐一重绑到新 hwnd；
/// 网络采样由 WM_TIMER tick 携带的 hwnd 直接投递，无需重绑。
fn rebuild_main_window(watchdog: HWND) {
    invalidate_taskbar_cache();

    // 会话通知必须在旧窗口销毁前配对注销：WTS 契约要求注销先于窗口销毁，
    // 销毁后句柄失效，届时已无法补做。
    unregister_session_notification();

    let old = CURRENT_MAIN_HWND.take();
    if let Some(old_hwnd) = old {
        // SAFETY: 主窗口与看门狗同在 UI 线程创建；IsWindow 过滤陈旧句柄后销毁安全。
        if unsafe { IsWindow(Some(old_hwnd)) }.as_bool() {
            unsafe {
                let _ = DestroyWindow(old_hwnd);
            }
        }
    }

    let hwnd = match create_main_window() {
        Ok(h) => h,
        Err(e) => {
            // 重建失败不能就此收手：CURRENT_MAIN_HWND 已归零、托盘与定时器全无，
            // 而 TaskbarCreated 不会有第二轮广播。转为看门狗上的退避重试直到成功；
            // 只有失败序列的首次提示用户，后续重试静默（否则退化成弹窗风暴）。
            if REBUILD_RETRY_INTERVAL_MS.with(|c| c.get()) == 0 {
                show_error(&format!("Explorer 重启后重建主窗口失败: {e}"));
            }
            arm_rebuild_retry(watchdog);
            return;
        }
    };
    disarm_rebuild_retry(watchdog);
    CURRENT_MAIN_HWND.store(hwnd);

    // 旧电源通知绑定在已销毁的窗口上，先注销再对新窗口重新注册。
    // 会话通知的旧注册已在函数开头（销毁前）注销，此处只补新窗口的注册。
    if let Some(prev_power) = POWER_NOTIFY_HANDLE.take() {
        unsafe {
            let _ = UnregisterPowerSettingNotification(prev_power);
        }
    }
    register_power_notify(hwnd);
    register_session_notification(hwnd);

    remove_tray_icon();

    // 嵌入失败同样不中止恢复：托盘与定时器必须重建，嵌入由 reembed_if_lost 周期兜底
    // ——TaskbarCreated 每次任务栏创建只广播一次，不会再来第二轮。
    if let Err(e) = embed_in_taskbar(hwnd) {
        show_error(&format!("Explorer 重启后嵌入任务栏失败: {e}"));
    }

    if !bind_display_and_timers(hwnd) {
        show_error("Explorer 重启后恢复监测定时器失败");
    }
}

/// 当前有效的主窗口句柄；无主窗口（重建间隙）或句柄已失效时返回 None。
///
/// 所有跨消息/跨线程的控制动作都必须经此校验后使用句柄：Explorer 重建会替换
/// 主窗口，任何快照下来的旧句柄都可能指向已销毁的窗口，向它投递消息会静默丢失。
fn live_main_hwnd() -> Option<HWND> {
    let hwnd = CURRENT_MAIN_HWND.load()?;
    // SAFETY: IsWindow 是纯查询，对陈旧句柄安全返回布尔，不解引用任何用户内存。
    unsafe { IsWindow(Some(hwnd)) }.as_bool().then_some(hwnd)
}

/// 退出请求的幂等门：`flag` 由 false 翻到 true 的那一次才是真正的执行者。
fn claim_exit_request(flag: &AtomicBool) -> bool {
    !flag.swap(true, Ordering::AcqRel)
}

/// 主窗口重建失败后的退避重试：间隔从 `TIMER_INTERVAL_REBUILD_RETRY_MIN` 翻倍
/// 至 `TIMER_INTERVAL_REBUILD_RETRY_MAX`，直到重建成功。
///
/// `SetTimer` 复用同一 ID 会重设倒计时，因此连续失败无需先 `KillTimer`。
fn arm_rebuild_retry(watchdog: HWND) {
    let next = match REBUILD_RETRY_INTERVAL_MS.with(|c| c.get()) {
        0 => TIMER_INTERVAL_REBUILD_RETRY_MIN,
        current => (current * 2).min(TIMER_INTERVAL_REBUILD_RETRY_MAX),
    };
    REBUILD_RETRY_INTERVAL_MS.with(|c| c.set(next));
    // SAFETY: watchdog 是看门狗窗口句柄，与调用方同属 UI 线程；不使用回调函数。
    unsafe {
        if SetTimer(Some(watchdog), TIMER_ID_REBUILD_RETRY, next, None) == 0 {
            diag!("重建重试定时器({TIMER_ID_REBUILD_RETRY}) 创建失败");
        }
    }
}

/// 结束重建重试：复位退避档位并移除定时器。
fn disarm_rebuild_retry(watchdog: HWND) {
    REBUILD_RETRY_INTERVAL_MS.with(|c| c.set(0));
    // KillTimer 对不存在的定时器只返回错误，不会破坏窗口状态。
    unsafe {
        KillTimer(Some(watchdog), TIMER_ID_REBUILD_RETRY).ok();
    }
}

/// 退出序列：清理托盘并结束消息循环。全仓唯一的退出收尾实现。
///
/// 幂等门收口在此：`EXIT_REQUESTED` 由 false 翻到 true 的那一次才执行。
/// 主窗口过程的 `WM_CLOSE`（托盘菜单「退出」）、看门狗的退出请求与更新交接
/// 三个入口共用这一份实现，「退出序列只执行一次」因此是代码事实而不是注释承诺，
/// 退出语义也不会在多处漂移。
///
/// 看门狗**直接执行**退出序列而不转发给主窗口：转发只能确认「消息已入队」，无法
/// 确认主窗口真的执行了——Explorer 崩溃会由 OS 级联销毁主窗口，队列里那条消息随之
/// 消失。直接执行让幂等门恰好对应「退出序列已执行」这一不可逆事实，主窗口是否存在
/// 都不影响退出，也就不存在「已置位却没退成、后续请求又被吞掉」的失效窗口。
pub(crate) fn begin_exit() {
    if !claim_exit_request(&EXIT_REQUESTED) {
        return;
    }
    remove_tray_icon();
    // SAFETY: 三个入口（主窗口过程、看门狗过程、更新交接）都运行在 UI 消息
    // 循环所属线程上，PostQuitMessage 向该线程队列投递 WM_QUIT。
    unsafe {
        PostQuitMessage(0);
    }
}

/// 看门狗完成退出序列：先撤销重建重试，再执行退出序列。
///
/// 退出在即，重建重试已无意义：留着只会在 WM_QUIT 被处理前再造一个孤儿窗口。
fn finish_exit_from_watchdog(watchdog: HWND) {
    disarm_rebuild_retry(watchdog);
    begin_exit();
}

/// 主题（浅色/深色）变更的共享处理：重算文字颜色并整幅重绘。
///
/// 两个窗口过程共用：主窗口分支只在启动后、嵌入任务栏之前的窗口期可达
/// （`SetParent` 之后它是 `WS_CHILD`，收不到 `HWND_BROADCAST` 顶层广播），
/// 看门狗分支是嵌入后的常驻路径。启动后到嵌入前的短暂窗口期两者同为顶层，
/// 一次广播会各处理一次——两次处理都是幂等的（重算颜色 + 置脏重绘），
/// 代价只是一次多余重绘，因此不需要为去重引入新状态。
fn apply_theme_change(hwnd: HWND) {
    renderer::with_renderer(|r| r.update_text_color());
    unsafe {
        let _ = InvalidateRect(Some(hwnd), None, false);
    }
}

/// 看门狗收到主题广播后把重绘落到当前主窗口；重建间隙无主窗口则无事可做。
fn apply_theme_change_to_main() {
    if let Some(hwnd) = live_main_hwnd() {
        apply_theme_change(hwnd);
    }
}

pub extern "system" fn watchdog_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let tcm = TASKBAR_CREATED_MSG.load(Ordering::Acquire);
    if tcm != 0 && msg == tcm {
        rebuild_main_window(hwnd);
        return LRESULT(0);
    }

    match msg {
        // 退出请求（--quit 按 WATCHDOG_CLASS 检索到本窗口后投递）：主窗口嵌入
        // 任务栏后检索不到，重建间隙更是完全不存在；请求可能重复到达，重复的
        // 由 [`begin_exit`] 的幂等门吞掉。
        WM_USER_QUIT_REQUEST => {
            finish_exit_from_watchdog(hwnd);
            LRESULT(0)
        }

        // 更新交接（子进程读到 EXIT_MAIN 后投递）：看门狗直接执行收尾语义。
        //
        // 不转发给主窗口：转发成功只代表消息已入队，检查/下载/确认弹窗期间若发生
        // Explorer 重建，消息会随旧主窗口一起消失，UPDATE_IN_PROGRESS 无人复位，
        // 后续所有检查被挡到进程重启。看门狗与主窗口同属 UI 消息循环线程，
        // handle_update_action 的线程前提不变。
        WM_USER_UPDATE_ACTION => {
            crate::update::handle_update_action();
            LRESULT(0)
        }

        // 主窗口重建失败后的退避重试。
        WM_TIMER if wparam.0 == TIMER_ID_REBUILD_RETRY => {
            rebuild_main_window(hwnd);
            LRESULT(0)
        }

        // 主题变更。WM_SETTINGCHANGE 走 HWND_BROADCAST 顶层广播，嵌入后的主窗口是
        // WS_CHILD 收不到，必须由常驻顶层窗口接收；这里直接执行共享处理而不转发，
        // 因为重建间隙主窗口可能不存在。电源与锁屏通知是定向消息，不经此路由。
        WM_SETTINGCHANGE => {
            // SAFETY: OS 保证 lparam 指向 NUL 结尾宽字符串（或 null）。
            if unsafe { is_immersive_color_set(lparam) } {
                apply_theme_change_to_main();
            }
            LRESULT(0)
        }

        // 看门狗没有 UI 也不参与重建，任何 WM_CLOSE 都只能是误发；让它被销毁
        // 等于永久失去 TaskbarCreated 接收者与退出/更新入口。
        WM_CLOSE => LRESULT(0),

        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

fn handle_timer(hwnd: HWND, wparam: WPARAM) -> LRESULT {
    match wparam.0 {
        TIMER_ID_INIT_TRIM => {
            trim_working_set();
            // KillTimer 对不存在的定时器仅返回错误，不触发 UB。
            unsafe {
                KillTimer(Some(hwnd), TIMER_ID_INIT_TRIM).ok();
            }
        }
        TIMER_ID_FULLSCREEN => {
            if !is_suspended() {
                // 常驻 tick（三种非挂起状态下都存在）兼作嵌入自愈：嵌入失败后
                // TaskbarCreated 不会再来，只能靠这里静默重试；重嵌入成功必须重绘，
                // 否则刚恢复可见的分层窗口仍是空白画布。
                if reembed_if_lost(hwnd) {
                    unsafe {
                        let _ = InvalidateRect(Some(hwnd), None, false);
                    }
                }
                check_fullscreen(hwnd);
            }
        }
        TIMER_ID_NETWORK => {
            if !is_suspended() && !MONITOR_FULLSCREEN.load(Ordering::Acquire) {
                // 任务栏通知区域变化时，即使数值不变也必须刷新新位置的画布。
                let position_changed = update_taskbar_position(hwnd);
                collect_network(hwnd);
                if position_changed {
                    unsafe {
                        let _ = InvalidateRect(Some(hwnd), None, false);
                    }
                } else {
                    renderer::invalidate_if_values_changed(hwnd);
                }
            }
        }
        TIMER_ID_CPU_MEM => {
            if !is_suspended() && !MONITOR_FULLSCREEN.load(Ordering::Acquire) {
                collect_cpu();
                collect_memory();
                renderer::invalidate_if_values_changed(hwnd);
            }
        }
        TIMER_ID_AUTO_UPDATE => {
            // 条件先命名：arm 体若只剩单个 if 会触发 collapsible_match，
            // 与兄弟 arm 同保持函数体内 if（无 match guard）。
            let active = !is_suspended() && !MONITOR_FULLSCREEN.load(Ordering::Acquire);
            if active {
                start_auto_check();
            }
        }
        _ => {}
    }
    LRESULT(0)
}

pub extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_CREATE => LRESULT(0),

        WM_PAINT => {
            let mut ps = PAINTSTRUCT::default();
            // SAFETY: BeginPaint/EndPaint 必须配对。
            let hdc = unsafe { BeginPaint(hwnd, &mut ps) };
            renderer::with_renderer(|r| r.render(hdc));
            unsafe {
                let _ = EndPaint(hwnd, &ps);
            }
            LRESULT(0)
        }

        WM_TIMER => handle_timer(hwnd, wparam),

        WM_USER_NETWORK_DISCONNECTED => {
            let _ = sync_monitoring_timers(hwnd);
            LRESULT(0)
        }

        WM_USER_NETWORK_RECONNECTED => {
            reset_network_backoff();
            let _ = sync_monitoring_timers(hwnd);
            start_auto_check();
            LRESULT(0)
        }

        WM_SETTINGCHANGE => {
            // 只在启动后、嵌入任务栏之前的顶层窗口期可达；嵌入后由看门狗接收
            // 顶层广播再落到本窗口（见 watchdog_wnd_proc）。
            // SAFETY: OS 保证 lparam 指向 NUL 结尾宽字符串（或 null）。
            if unsafe { is_immersive_color_set(lparam) } {
                apply_theme_change(hwnd);
            }
            LRESULT(0)
        }

        WM_DPICHANGED => {
            let mut dpi_updated = false;
            renderer::with_renderer(|r| dpi_updated = r.update_dpi(hwnd));
            if dpi_updated {
                // 失败不弹框：DPI 变更本身就是重排，改由 reembed_if_lost 在下一 tick 补做，
                // 避免跨屏拖动时连环弹窗。
                let _ = embed_in_taskbar(hwnd);
            } else {
                // 位图/字体创建失败：须把窗口回滚到渲染器维持的旧尺寸；
                // 跨屏后的合身位置由下个成功周期自愈。
                rollback_window_to_bitmap(hwnd);
            }
            unsafe {
                let _ = InvalidateRect(Some(hwnd), None, false);
            }
            LRESULT(0)
        }

        // 电源设置通知是定向消息：RegisterPowerSettingNotification 绑定的目标是
        // 当前主窗口 HWND，嵌入为子窗口后仍直达，故处理器必须留在主窗口侧。
        WM_POWERBROADCAST => handle_power_broadcast(hwnd, wparam, lparam),

        // 同理，锁屏/解锁由 WTSRegisterSessionNotification 定向到主窗口，且其
        // 暂停/恢复要带当前 hwnd 重建定时器，搬到看门狗会让目标窗口错位。
        WM_WTSSESSION_CHANGE => handle_session_change(hwnd, wparam),

        WM_CLOSE => {
            begin_exit();
            LRESULT(0)
        }

        WM_APP_TRAY => {
            let event = (lparam.0 as u32) & LOWORD_MASK;
            if event == WM_CONTEXTMENU {
                tray::show_context_menu(hwnd);
            }
            LRESULT(0)
        }

        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

#[cfg(test)]
mod tests {
    //! 看门狗控制入口的语义测试：只覆盖纯判定与句柄校验，不创建真实窗口。

    use super::{CURRENT_MAIN_HWND, claim_exit_request, live_main_hwnd, parse_cli_args};

    #[test]
    fn quit_coexists_with_check_update_both_parsed() {
        // 该组合走退出分支是 main() 先检查 quit 的代码顺序保证的，
        // 本测试只钉死解析结果（两标记同为真），不证明分支优先级。
        let cli = parse_cli_args(
            ["--quit", "--check-update", "--manual"]
                .into_iter()
                .map(std::ffi::OsString::from),
        );
        assert!(cli.quit, "--quit 应被识别");
        assert!(cli.check_update, "--check-update 应被识别");
        assert!(cli.manual, "--manual 应被识别");
        assert!(!cli.relaunched_by_update);
    }

    #[test]
    fn cli_args_require_exact_match() {
        // 缀接形式与旧 `==` 语义一致判否：精确匹配才算数（父身份参数连同其值一并判否）。
        let cli = parse_cli_args(
            [
                "--quit=1",
                "--check-update=1",
                "--manual=1",
                "--relaunched-by-update=1",
                "--parent-pid=1",
                "--parent-start=2",
            ]
            .into_iter()
            .map(std::ffi::OsString::from),
        );
        assert_eq!(
            cli,
            super::CliArgs {
                quit: false,
                check_update: false,
                manual: false,
                relaunched_by_update: false,
                parent_pid: None,
                parent_start: None,
            }
        );
    }

    #[test]
    fn cli_args_parse_parent_identity_pair() {
        let cli = parse_cli_args(
            [
                "--check-update",
                "--parent-pid",
                "4242",
                "--parent-start",
                "133700000000000000",
            ]
            .into_iter()
            .map(std::ffi::OsString::from),
        );
        assert!(cli.check_update);
        assert_eq!(cli.parent_pid, Some(4242));
        assert_eq!(cli.parent_start, Some(133700000000000000));
    }

    #[test]
    fn cli_args_degrade_when_parent_identity_is_incomplete_or_invalid() {
        // 值缺失/非数字 ⇒ 保持 None（探针退化为「无父可查」），
        // 绝不能把残缺参数当成「父已退出」：那会误杀手工 `--check-update --manual`。
        let cli = parse_cli_args(
            ["--parent-pid", "--parent-start", "abc", "--manual"]
                .into_iter()
                .map(std::ffi::OsString::from),
        );
        assert_eq!(cli.parent_pid, None);
        assert_eq!(cli.parent_start, None);
        assert!(cli.manual);

        // 只有一个参数（另一半缺席）同样退化。
        let cli = parse_cli_args(
            ["--parent-start", "42"]
                .into_iter()
                .map(std::ffi::OsString::from),
        );
        assert_eq!(cli.parent_pid, None);
        assert_eq!(cli.parent_start, Some(42));
    }

    #[test]
    fn cli_args_do_not_swallow_a_following_flag_as_parent_value() {
        // 残缺命令行（手打）：值位置的参数本身是标志时不得被吞掉，
        // 否则 `--parent-pid --manual` 会静默把手动标记丢掉。
        let cli = parse_cli_args(
            ["--check-update", "--parent-pid", "--manual"]
                .into_iter()
                .map(std::ffi::OsString::from),
        );
        assert_eq!(cli.parent_pid, None, "标志形态的候选值必须按缺席处理");
        assert!(cli.manual, "--manual 必须仍被解析为标志");
        assert!(cli.check_update);
    }

    #[test]
    fn cli_args_parses_each_flag_and_ignores_unknown() {
        use std::os::windows::ffi::OsStringExt;
        // 逐个标记：单个参数应只点亮对应位。
        let cli = parse_cli_args([std::ffi::OsString::from("--quit")]);
        assert!(cli.quit && !cli.check_update && !cli.manual && !cli.relaunched_by_update);
        // 未知参数与非 Unicode 参数只被忽略，不 panic。
        let cli = parse_cli_args(
            ["--unknown", crate::config::RELAUNCHED_BY_UPDATE_ARG]
                .into_iter()
                .map(std::ffi::OsString::from)
                .chain(std::iter::once(std::ffi::OsString::from_wide(&[0xD800u16]))),
        );
        assert!(!cli.quit && !cli.check_update && !cli.manual && cli.relaunched_by_update);
    }

    use std::sync::atomic::{AtomicBool, Ordering};

    #[test]
    fn exit_request_gate_accepts_only_first_request() {
        // --quit 可能因轮询/重试重复投递：只有第一次请求执行退出序列，
        // 后续请求必须被幂等门吞掉（不重复清理托盘、不重复 PostQuitMessage）。
        let gate = AtomicBool::new(false);
        assert!(claim_exit_request(&gate), "首次请求应执行退出序列");
        assert!(!claim_exit_request(&gate), "重复请求应被吞掉");
        assert!(!claim_exit_request(&gate), "第三次仍应被吞掉");
        assert!(gate.load(Ordering::Acquire));
    }

    #[test]
    fn stale_main_hwnd_is_rejected() {
        // 模拟 Explorer 重建后残留的旧句柄：IsWindow 必须否决它，使需要向当前主窗口
        // 投递主题/重绘等动作的路径跳过失效目标，而不是把消息投给已销毁的窗口。
        CURRENT_MAIN_HWND.store_raw(0x0BAD_F00D);
        assert!(live_main_hwnd().is_none());

        // 重建间隙（CURRENT_MAIN_HWND 为空）同样没有可转发的目标。
        CURRENT_MAIN_HWND.clear();
        assert!(live_main_hwnd().is_none());
    }
}
