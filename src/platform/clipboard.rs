use anyhow::{Context, Result};
use arboard::Clipboard;

pub fn copy_text(text: &str) -> Result<()> {
    Clipboard::new()
        .context("无法打开剪贴板")?
        .set_text(text.to_owned())
        .context("复制文本失败")
}
