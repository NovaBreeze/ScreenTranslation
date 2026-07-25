//! Windows 前台窗口控制。
//!
//! 全局热键触发时本进程通常没有前台权限（winit 首次 show 用 SW_SHOWNOACTIVATE），
//! 遮罩会以“未激活”状态出现；用户第一次点击时 Windows 才激活这个全屏无边框窗口，
//! 产生可见的前台切换黑帧，关闭时再闪一次。解决办法：趁遮罩还在屏幕外时用
//! AttachThreadInput 绕过前台锁定强制激活，让激活过渡发生在屏幕外；
//! 关闭时先移出屏幕再归还前台，过渡同样不可见。

#[cfg(windows)]
use windows::Win32::Foundation::HWND;

/// 当前前台窗口句柄（0 表示无）。
#[cfg(windows)]
pub fn current() -> isize {
    use windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow;
    unsafe { GetForegroundWindow() }.0 as isize
}

/// 从 Slint 窗口取 Win32 HWND；窗口尚未映射或失败时返回 0。
#[cfg(windows)]
pub fn hwnd_of(window: &slint::Window) -> isize {
    use raw_window_handle_06::{HasWindowHandle, RawWindowHandle};
    let handle = window.window_handle();
    match handle.window_handle().map(|h| h.as_raw()) {
        Ok(RawWindowHandle::Win32(win32)) => win32.hwnd.get(),
        _ => 0,
    }
}

/// 绕过前台锁定，强制把窗口置为前台。
#[cfg(windows)]
pub fn force_foreground(hwnd: isize) {
    use windows::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
    use windows::Win32::UI::WindowsAndMessaging::{
        BringWindowToTop, GetForegroundWindow, GetWindowThreadProcessId, SetForegroundWindow,
    };
    use windows::Win32::UI::Input::KeyboardAndMouse::SetActiveWindow;
    if hwnd == 0 {
        return;
    }
    unsafe {
        let target = HWND(hwnd as _);
        let foreground = GetForegroundWindow();
        if foreground == target {
            return;
        }
        let foreground_thread = GetWindowThreadProcessId(foreground, None);
        let current_thread = GetCurrentThreadId();
        // 前台锁定（foreground lockout）：附着到前台线程的输入队列后，
        // SetForegroundWindow 才会被系统接受。
        let attached = foreground_thread != 0
            && foreground_thread != current_thread
            && AttachThreadInput(current_thread, foreground_thread, true).as_bool();
        let _ = BringWindowToTop(target);
        let _ = SetForegroundWindow(target);
        let _ = SetActiveWindow(target);
        if attached {
            let _ = AttachThreadInput(current_thread, foreground_thread, false);
        }
    }
}

/// 把窗口标记为工具窗口：不占任务栏按钮、不出现在 Alt+Tab。
/// 幂等；winit 在部分状态变更时会重写扩展样式，所以每次上场前都要重设。
#[cfg(windows)]
pub fn make_tool_window(hwnd: isize) {
    use windows::Win32::UI::WindowsAndMessaging::{
        GWL_EXSTYLE, GetWindowLongW, SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE,
        SWP_NOZORDER, SetWindowLongW, SetWindowPos, WS_EX_APPWINDOW, WS_EX_TOOLWINDOW,
    };
    if hwnd == 0 {
        return;
    }
    unsafe {
        let target = HWND(hwnd as _);
        let exstyle = GetWindowLongW(target, GWL_EXSTYLE);
        let updated = (exstyle | WS_EX_TOOLWINDOW.0 as i32) & !(WS_EX_APPWINDOW.0 as i32);
        if updated != exstyle {
            SetWindowLongW(target, GWL_EXSTYLE, updated);
            let _ = SetWindowPos(
                target,
                None,
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED,
            );
        }
    }
}

/// 把前台归还给指定窗口（关闭遮罩后恢复用户原来的焦点）。
#[cfg(windows)]
pub fn restore(hwnd: isize) {
    if hwnd == 0 {
        return;
    }
    use windows::Win32::UI::WindowsAndMessaging::SetForegroundWindow;
    unsafe {
        let _ = SetForegroundWindow(HWND(hwnd as _));
    }
}

#[cfg(not(windows))]
pub fn current() -> isize {
    0
}

#[cfg(not(windows))]
pub fn hwnd_of(_window: &slint::Window) -> isize {
    0
}

#[cfg(not(windows))]
pub fn force_foreground(_hwnd: isize) {}

#[cfg(not(windows))]
pub fn make_tool_window(_hwnd: isize) {}

#[cfg(not(windows))]
pub fn restore(_hwnd: isize) {}
