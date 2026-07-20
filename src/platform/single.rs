use anyhow::{Context, Result};
use std::sync::mpsc::Sender;

/// 同一程序进程间传递的命令。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InstanceCommand {
    ShowSettings,
}

/// 保留 `single-instance` 互斥，并提供第二实例到首实例的本机通知。
pub struct SingleInstance {
    instance: single_instance::SingleInstance,
    #[cfg(windows)]
    event_name: Vec<u16>,
    #[cfg(windows)]
    event: Handle,
}

impl SingleInstance {
    /// 创建单实例守卫。
    ///
    /// 如果当前进程是第二实例，构造时会自动向首实例发送
    /// [`InstanceCommand::ShowSettings`]。
    pub fn new(name: &str) -> Result<Self> {
        #[cfg(windows)]
        {
            let event_name = event_name(name);
            let event = create_event(Some(&event_name)).context("无法创建单实例通知事件")?;
            let instance = match single_instance::SingleInstance::new(name) {
                Ok(instance) => instance,
                Err(error) => {
                    drop(event);
                    return Err(error).context("无法创建单实例互斥");
                }
            };

            let result = Self {
                instance,
                event_name,
                event,
            };
            if !result.is_primary() {
                result.notify_primary(InstanceCommand::ShowSettings)?;
            }
            Ok(result)
        }

        #[cfg(not(windows))]
        {
            let instance =
                single_instance::SingleInstance::new(name).context("无法创建单实例互斥")?;
            Ok(Self { instance })
        }
    }

    pub fn is_primary(&self) -> bool {
        self.instance.is_single()
    }

    /// 向首实例发送命令。当前仅支持 `show-settings`。
    #[cfg(windows)]
    pub fn notify_primary(&self, command: InstanceCommand) -> Result<()> {
        match command {
            InstanceCommand::ShowSettings => self.event.set().context("无法通知首实例显示设置"),
        }
    }

    #[cfg(not(windows))]
    pub fn notify_primary(&self, _command: InstanceCommand) -> Result<()> {
        Ok(())
    }

    /// 在后台线程监听第二实例发来的 `show-settings` 通知。
    pub fn listen(
        &self,
        callback: impl Fn(InstanceCommand) + Send + Sync + 'static,
    ) -> Result<InstanceListener> {
        anyhow::ensure!(self.is_primary(), "只有首实例可以监听实例通知");

        #[cfg(windows)]
        {
            let notification =
                create_event(Some(&self.event_name)).context("无法打开单实例通知事件")?;
            let stop = create_event(None).context("无法创建单实例监听停止事件")?;
            let notification_raw = notification.0;
            let stop_raw = stop.0;

            let thread = match std::thread::Builder::new()
                .name("single-instance-ipc".to_owned())
                .spawn(move || {
                    const WAIT_OBJECT_0: u32 = 0;
                    const INFINITE: u32 = u32::MAX;

                    let handles = [notification_raw, stop_raw];
                    loop {
                        let result =
                            unsafe { WaitForMultipleObjects(2, handles.as_ptr(), 0, INFINITE) };
                        match result {
                            WAIT_OBJECT_0 => callback(InstanceCommand::ShowSettings),
                            value if value == WAIT_OBJECT_0 + 1 => break,
                            _ => break,
                        }
                    }

                    drop(notification);
                    drop(stop);
                }) {
                Ok(thread) => thread,
                Err(error) => return Err(error).context("无法创建单实例监听线程"),
            };

            Ok(InstanceListener {
                stop: stop_raw,
                thread: Some(thread),
            })
        }

        #[cfg(not(windows))]
        {
            let _ = callback;
            Ok(InstanceListener {})
        }
    }

    /// 使用标准通道接收实例命令。
    #[allow(dead_code)]
    pub fn listen_with_sender(&self, sender: Sender<InstanceCommand>) -> Result<InstanceListener> {
        self.listen(move |command| {
            let _ = sender.send(command);
        })
    }
}

/// 单实例 IPC 后台监听的生命周期守卫。
pub struct InstanceListener {
    #[cfg(windows)]
    stop: isize,
    #[cfg(windows)]
    thread: Option<std::thread::JoinHandle<()>>,
}

#[cfg(windows)]
impl Drop for InstanceListener {
    fn drop(&mut self) {
        unsafe {
            SetEvent(self.stop);
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(windows)]
struct Handle(isize);

#[cfg(windows)]
impl Handle {
    fn set(&self) -> std::io::Result<()> {
        if unsafe { SetEvent(self.0) } == 0 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
}

#[cfg(windows)]
impl Drop for Handle {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0);
        }
    }
}

#[cfg(windows)]
fn create_event(name: Option<&[u16]>) -> std::io::Result<Handle> {
    let name = name.map_or(std::ptr::null(), |value| value.as_ptr());
    let handle = unsafe { CreateEventW(std::ptr::null(), 0, 0, name) };
    if handle == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(Handle(handle))
    }
}

#[cfg(any(windows, test))]
fn event_name(instance_name: &str) -> Vec<u16> {
    let readable: String = instance_name
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '.' | '-' | '_') {
                character
            } else {
                '_'
            }
        })
        .collect();
    let hash = instance_name
        .as_bytes()
        .iter()
        .fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
            (hash ^ u64::from(*byte)).wrapping_mul(0x0000_0100_0000_01b3)
        });
    format!(r"Local\ScreenTranslator.{readable}.{hash:016x}.ShowSettings")
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect()
}

#[cfg(windows)]
#[link(name = "kernel32")]
unsafe extern "system" {
    fn CreateEventW(
        event_attributes: *const std::ffi::c_void,
        manual_reset: i32,
        initial_state: i32,
        name: *const u16,
    ) -> isize;
    fn SetEvent(event: isize) -> i32;
    fn CloseHandle(object: isize) -> i32;
    fn WaitForMultipleObjects(
        count: u32,
        handles: *const isize,
        wait_all: i32,
        milliseconds: u32,
    ) -> u32;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_name_is_local_stable_and_terminated() {
        let first = event_name("Screen Translator/桌面");
        let second = event_name("Screen Translator/桌面");
        assert_eq!(first, second);
        assert_eq!(first.last(), Some(&0));
        assert!(String::from_utf16_lossy(&first).starts_with(r"Local\ScreenTranslator."));
    }
}
