//! 文件日志：写入 `%APPDATA%\ScreenTranslator\logs\screen-translator.log`。
//!
//! 终端不可见的托盘程序必须有落盘日志，否则用户环境里的问题无法诊断。
//! 可用 `RUST_LOG` 环境变量覆盖级别（默认 `screen_translator=info`）。

use std::{
    fs,
    io::Write,
    path::PathBuf,
    sync::{Arc, Mutex},
};

/// 日志文件最大尺寸；超过后截断重写，避免无限增长。
const MAX_LOG_BYTES: u64 = 4 * 1024 * 1024;

#[derive(Clone)]
struct FileWriter(Arc<Mutex<fs::File>>);

impl Write for FileWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().expect("log file lock").write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.0.lock().expect("log file lock").flush()
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for FileWriter {
    type Writer = FileWriter;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

pub fn log_dir() -> PathBuf {
    crate::config::config_dir()
        .unwrap_or_else(|_| std::env::temp_dir().join("ScreenTranslator"))
        .join("logs")
}

pub fn init() {
    let dir = log_dir();
    let _ = fs::create_dir_all(&dir);
    let path = dir.join("screen-translator.log");

    // panic 落盘：release 为 panic="abort"，托盘程序无控制台，
    // 不抓 hook 的话崩溃只剩一个 0xc0000409 退出码。
    let panic_path = dir.join("panics.log");
    std::panic::set_hook(Box::new(move |info| {
        let message = format!(
            "{info}\nbacktrace:\n{}\n",
            std::backtrace::Backtrace::force_capture()
        );
        eprintln!("panic: {message}");
        if let Ok(mut file) = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&panic_path)
        {
            let _ = writeln!(file, "{message}");
        }
    }));
    if let Ok(meta) = fs::metadata(&path)
        && meta.len() > MAX_LOG_BYTES
    {
        let _ = fs::remove_file(&path);
    }

    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| "screen_translator=info".into());

    match fs::OpenOptions::new().create(true).append(true).open(&path) {
        Ok(file) => {
            let writer = FileWriter(Arc::new(Mutex::new(file)));
            tracing_subscriber::fmt()
                .with_env_filter(filter)
                .with_writer(writer)
                .with_ansi(false)
                .init();
        }
        Err(_) => {
            // 日志文件不可写时退回 stderr，不阻塞启动。
            tracing_subscriber::fmt().with_env_filter(filter).init();
        }
    }

    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        exe = ?std::env::current_exe().ok(),
        log = ?path,
        "screen-translator starting"
    );
}
