use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

/// 管理当前用户的开机启动项。
#[derive(Clone, Debug)]
pub struct Autostart {
    app_name: String,
    executable: PathBuf,
}

impl Autostart {
    pub fn new(app_name: impl Into<String>, executable: impl Into<PathBuf>) -> Self {
        Self {
            app_name: app_name.into(),
            executable: executable.into(),
        }
    }

    pub fn for_current_exe(app_name: impl Into<String>) -> Result<Self> {
        let executable = std::env::current_exe().context("无法确定当前程序路径")?;
        Ok(Self::new(app_name, executable))
    }

    /// 返回 HKCU Run 中是否存在该应用的有效启动项。
    #[cfg(windows)]
    pub fn is_enabled(&self) -> Result<bool> {
        use winreg::{RegKey, enums::HKEY_CURRENT_USER};

        let current_user = RegKey::predef(HKEY_CURRENT_USER);
        let run = match current_user.open_subkey(RUN_KEY) {
            Ok(key) => key,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error).context("无法打开当前用户启动项注册表"),
        };

        match run.get_value::<String, _>(&self.app_name) {
            Ok(command) => Ok(!command.trim().is_empty()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error).context("无法读取当前用户启动项"),
        }
    }

    /// 在 HKCU Run 中启用或禁用该应用。
    #[cfg(windows)]
    pub fn set_enabled(&self, enabled: bool) -> Result<()> {
        use winreg::{RegKey, enums::HKEY_CURRENT_USER};

        let current_user = RegKey::predef(HKEY_CURRENT_USER);
        let (run, _) = current_user
            .create_subkey(RUN_KEY)
            .context("无法打开当前用户启动项注册表")?;

        if enabled {
            let command = quote_executable(&self.executable);
            run.set_value(&self.app_name, &command)
                .context("无法写入当前用户启动项")
        } else {
            match run.delete_value(&self.app_name) {
                Ok(()) => Ok(()),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(error) => Err(error).context("无法删除当前用户启动项"),
            }
        }
    }

    #[cfg(not(windows))]
    pub fn is_enabled(&self) -> Result<bool> {
        Ok(false)
    }

    #[cfg(not(windows))]
    pub fn set_enabled(&self, _enabled: bool) -> Result<()> {
        Ok(())
    }
}

#[cfg(windows)]
const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";

#[cfg(any(windows, test))]
fn quote_executable(path: &Path) -> String {
    format!("\"{}\"", path.display())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn executable_is_quoted_for_run_key() {
        let command = quote_executable(Path::new(r"C:\Program Files\Translator\app.exe"));
        assert_eq!(command, r#""C:\Program Files\Translator\app.exe""#);
    }
}
