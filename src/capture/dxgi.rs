use anyhow::{Context, Result};
use image::{DynamicImage, RgbaImage};

#[cfg(windows)]
pub fn capture_monitor(index: usize) -> Result<DynamicImage> {
    use dxgi_capture_rs::{CaptureError, DXGIManager};

    let mut manager = DXGIManager::new(250).map_err(|error| anyhow::anyhow!(error))?;
    manager.set_capture_source_index(index);

    let mut last_error = None;
    for _ in 0..4 {
        match manager.capture_frame() {
            Ok((pixels, (width, height))) => {
                let mut rgba = Vec::with_capacity(pixels.len() * 4);
                for pixel in pixels {
                    rgba.extend_from_slice(&[pixel.r, pixel.g, pixel.b, 255]);
                }
                let image = RgbaImage::from_raw(width as u32, height as u32, rgba)
                    .context("DXGI 返回的帧尺寸无效")?;
                return Ok(DynamicImage::ImageRgba8(image));
            }
            Err(CaptureError::Timeout) => {
                last_error = Some(anyhow::anyhow!("等待 DXGI 新帧超时"));
            }
            Err(error) => {
                last_error = Some(anyhow::anyhow!("DXGI 捕获失败：{error}"));
                break;
            }
        }
    }
    Err(last_error.unwrap_or_else(|| anyhow::anyhow!("DXGI 未返回桌面帧")))
}

#[cfg(not(windows))]
#[allow(dead_code)]
pub fn capture_monitor(_index: usize) -> Result<DynamicImage> {
    anyhow::bail!("DXGI 截屏仅支持 Windows")
}
