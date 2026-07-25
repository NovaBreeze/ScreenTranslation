#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod capture;
mod config;
mod history;
mod logging;
mod ocr;
mod platform;
mod render;
mod security;
mod translate;

slint::include_modules!();

fn main() -> anyhow::Result<()> {
    logging::init();

    #[cfg(windows)]
    unsafe {
        use windows::Win32::UI::HiDpi::{
            DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext,
        };
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }

    let instance = platform::single::SingleInstance::new("ScreenTranslator.Desktop.Singleton")?;
    if !instance.is_primary() {
        return Ok(());
    }
    app::run(instance)
}
