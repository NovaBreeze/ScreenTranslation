use anyhow::Result;
use std::sync::mpsc::Sender;

/// Windows 全局热键的修饰键。
pub const MOD_ALT: u32 = 0x0001;
pub const MOD_CONTROL: u32 = 0x0002;
pub const MOD_SHIFT: u32 = 0x0004;
pub const MOD_WIN: u32 = 0x0008;
pub const MOD_NOREPEAT: u32 = 0x4000;

/// 一个由 Windows 修饰键位图和虚拟键码组成的全局热键。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Hotkey {
    pub modifiers: u32,
    pub virtual_key: u32,
}

impl Hotkey {
    pub const fn new(modifiers: u32, virtual_key: u32) -> Self {
        Self {
            modifiers,
            virtual_key,
        }
    }
}

impl Default for Hotkey {
    fn default() -> Self {
        // VK_T
        Self::new(MOD_CONTROL | MOD_ALT | MOD_NOREPEAT, b'T' as u32)
    }
}

/// 在独立 Windows 消息线程上持有一个全局热键。
///
/// 管理器被丢弃时会停止消息循环并注销热键。
pub struct HotkeyManager {
    #[allow(dead_code)]
    hotkey: Hotkey,
    #[cfg(windows)]
    thread_id: u32,
    #[cfg(windows)]
    thread: Option<std::thread::JoinHandle<()>>,
}

impl HotkeyManager {
    /// 注册默认热键 Ctrl+Alt+T。
    #[allow(dead_code)]
    pub fn new(callback: impl Fn() + Send + Sync + 'static) -> Result<Self> {
        Self::with_hotkey(Hotkey::default(), callback)
    }

    /// 注册指定热键，并在每次触发时调用线程安全回调。
    pub fn with_hotkey(
        hotkey: Hotkey,
        callback: impl Fn() + Send + Sync + 'static,
    ) -> Result<Self> {
        Self::with_boxed_callback(hotkey, Box::new(callback))
    }

    /// 注册默认热键，并通过通道发送通知。
    #[allow(dead_code)]
    pub fn with_sender(sender: Sender<()>) -> Result<Self> {
        Self::new(move || {
            let _ = sender.send(());
        })
    }

    #[allow(dead_code)]
    pub fn hotkey(&self) -> Hotkey {
        self.hotkey
    }

    #[cfg(windows)]
    fn with_boxed_callback(hotkey: Hotkey, callback: Box<dyn Fn() + Send + Sync>) -> Result<Self> {
        use anyhow::{Context, anyhow};
        use std::sync::mpsc;

        let (started_tx, started_rx) = mpsc::sync_channel(1);
        let thread = std::thread::Builder::new()
            .name("global-hotkey".to_owned())
            .spawn(move || {
                const HOTKEY_ID: i32 = 1;
                const WM_HOTKEY: u32 = 0x0312;

                let thread_id = unsafe { GetCurrentThreadId() };
                let registered =
                    unsafe { RegisterHotKey(0, HOTKEY_ID, hotkey.modifiers, hotkey.virtual_key) };
                if registered == 0 {
                    let error = std::io::Error::last_os_error();
                    let _ = started_tx.send(Err(error));
                    return;
                }

                if started_tx.send(Ok(thread_id)).is_err() {
                    unsafe {
                        UnregisterHotKey(0, HOTKEY_ID);
                    }
                    return;
                }

                let mut message = Msg::default();
                loop {
                    let result = unsafe { GetMessageW(&mut message, 0, 0, 0) };
                    if result <= 0 {
                        break;
                    }
                    if message.message == WM_HOTKEY && message.w_param == HOTKEY_ID as usize {
                        callback();
                    }
                }

                unsafe {
                    UnregisterHotKey(0, HOTKEY_ID);
                }
            })
            .context("无法创建全局热键消息线程")?;

        match started_rx.recv().context("全局热键消息线程意外退出")? {
            Ok(thread_id) => Ok(Self {
                hotkey,
                thread_id,
                thread: Some(thread),
            }),
            Err(error) => {
                let _ = thread.join();
                Err(anyhow!("注册全局热键失败: {error}"))
            }
        }
    }

    #[cfg(not(windows))]
    fn with_boxed_callback(hotkey: Hotkey, _callback: Box<dyn Fn() + Send + Sync>) -> Result<Self> {
        Ok(Self { hotkey })
    }
}

#[cfg(windows)]
impl Drop for HotkeyManager {
    fn drop(&mut self) {
        const WM_QUIT: u32 = 0x0012;
        unsafe {
            PostThreadMessageW(self.thread_id, WM_QUIT, 0, 0);
        }
        if let Some(thread) = self.thread.take() {
            if unsafe { GetCurrentThreadId() } != self.thread_id {
                let _ = thread.join();
            }
        }
    }
}

#[cfg(windows)]
#[repr(C)]
#[derive(Default)]
struct Point {
    x: i32,
    y: i32,
}

#[cfg(windows)]
#[repr(C)]
#[derive(Default)]
struct Msg {
    hwnd: isize,
    message: u32,
    w_param: usize,
    l_param: isize,
    time: u32,
    point: Point,
    private: u32,
}

#[cfg(windows)]
#[link(name = "user32")]
unsafe extern "system" {
    fn RegisterHotKey(hwnd: isize, id: i32, modifiers: u32, virtual_key: u32) -> i32;
    fn UnregisterHotKey(hwnd: isize, id: i32) -> i32;
    fn GetMessageW(message: *mut Msg, hwnd: isize, min: u32, max: u32) -> i32;
    fn PostThreadMessageW(thread_id: u32, message: u32, w_param: usize, l_param: isize) -> i32;
}

#[cfg(windows)]
#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetCurrentThreadId() -> u32;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_hotkey_is_ctrl_alt_t() {
        let hotkey = Hotkey::default();
        assert_eq!(hotkey.virtual_key, b'T' as u32);
        assert_ne!(hotkey.modifiers & MOD_CONTROL, 0);
        assert_ne!(hotkey.modifiers & MOD_ALT, 0);
    }
}
