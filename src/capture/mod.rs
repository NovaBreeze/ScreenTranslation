mod dxgi;
mod gdi;

use anyhow::Result;
use image::{DynamicImage, GenericImageView};

pub use gdi::DisplayInfo;

#[derive(Debug, Clone, Copy)]
pub struct Selection {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

pub fn display_under_cursor() -> DisplayInfo {
    gdi::display_under_cursor()
}

pub fn capture_selection_on_display(
    selection: Selection,
    display: DisplayInfo,
) -> Result<DynamicImage> {
    let scale = display.scale_factor;
    let frame = match dxgi::capture_monitor(display.index) {
        Ok(frame) if frame.dimensions() == (display.width, display.height) => frame,
        _ => gdi::capture_display(display)?,
    };
    let (frame_w, frame_h) = frame.dimensions();

    let x = ((selection.x * scale).round().max(0.0) as u32).min(frame_w);
    let y = ((selection.y * scale).round().max(0.0) as u32).min(frame_h);
    let width = ((selection.width * scale).round().max(1.0) as u32).min(frame_w.saturating_sub(x));
    let height =
        ((selection.height * scale).round().max(1.0) as u32).min(frame_h.saturating_sub(y));

    anyhow::ensure!(width > 0 && height > 0, "选择区域超出屏幕范围");
    Ok(frame.crop_imm(x, y, width, height))
}
