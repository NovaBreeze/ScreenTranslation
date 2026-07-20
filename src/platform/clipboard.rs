use anyhow::{Context, Result};
use arboard::{Clipboard, ImageData};
use image::DynamicImage;
use std::borrow::Cow;

pub fn copy_text(text: &str) -> Result<()> {
    Clipboard::new()
        .context("无法打开剪贴板")?
        .set_text(text.to_owned())
        .context("复制文本失败")
}

pub fn copy_image(image: &DynamicImage) -> Result<()> {
    let rgba = image.to_rgba8();
    Clipboard::new()
        .context("无法打开剪贴板")?
        .set_image(ImageData {
            width: rgba.width() as usize,
            height: rgba.height() as usize,
            bytes: Cow::Owned(rgba.into_raw()),
        })
        .context("复制图片失败")
}
