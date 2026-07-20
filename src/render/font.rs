use crate::ocr::Rect;
use ab_glyph::{Font, FontArc, PxScale, ScaleFont, point};
use anyhow::{Context, Result};
use image::{Rgba, RgbaImage};
use std::path::{Path, PathBuf};

pub fn load_font(preferred: Option<&str>) -> Result<FontArc> {
    let mut candidates = Vec::<PathBuf>::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(parent) = exe.parent() {
            candidates.push(parent.join("assets/fonts/SourceHanSansSC-subset.otf"));
        }
    }
    if let Ok(current) = std::env::current_dir() {
        candidates.push(current.join("assets/fonts/SourceHanSansSC-subset.otf"));
    }
    #[cfg(windows)]
    {
        if let Some(preferred) = preferred {
            let selected = match preferred {
                "Segoe UI" => Some(r"C:\Windows\Fonts\segoeui.ttf"),
                "等宽字体" => Some(r"C:\Windows\Fonts\consola.ttf"),
                "Microsoft YaHei UI" => Some(r"C:\Windows\Fonts\msyh.ttc"),
                _ => None,
            };
            if let Some(selected) = selected {
                candidates.push(PathBuf::from(selected));
            }
        }
        candidates.push(PathBuf::from(r"C:\Windows\Fonts\msyh.ttc"));
        candidates.push(PathBuf::from(r"C:\Windows\Fonts\simhei.ttf"));
        candidates.push(PathBuf::from(r"C:\Windows\Fonts\arial.ttf"));
    }

    for path in candidates {
        if let Ok(bytes) = std::fs::read(&path) {
            if let Ok(font) = FontArc::try_from_vec(bytes) {
                return Ok(font);
            }
        }
    }
    anyhow::bail!("未找到可用字体，请放置 assets/fonts/SourceHanSansSC-subset.otf")
}

pub fn fit_font_size(font: &FontArc, text: &str, rect: Rect) -> f32 {
    let mut size = (rect.height as f32 * 0.75).clamp(10.0, 64.0);
    while size > 10.0 {
        let lines = wrap_lines(font, text, size, rect.width.max(1) as f32);
        if lines.len() as f32 * size * 1.25 <= rect.height.max(1) as f32 * 1.8 {
            break;
        }
        size -= 1.0;
    }
    size
}

pub fn wrapped_text_height(font: &FontArc, text: &str, size: f32, width: u32) -> f32 {
    wrap_lines(font, text, size, width.max(1) as f32).len() as f32 * size * 1.2
}

pub fn draw_text_wrapped(
    image: &mut RgbaImage,
    font: &FontArc,
    text: &str,
    rect: Rect,
    size: f32,
    color: Rgba<u8>,
) {
    let lines = wrap_lines(font, text, size, rect.width.max(1) as f32);
    let scale = PxScale::from(size);
    let mut baseline = rect.y as f32 + size;
    for line in lines {
        let mut x = rect.x as f32 + 2.0;
        for ch in line.chars() {
            let glyph_id = font.glyph_id(ch);
            let advance = font.as_scaled(scale).h_advance(glyph_id);
            let glyph = glyph_id.with_scale_and_position(scale, point(x, baseline));
            if let Some(outlined) = font.outline_glyph(glyph) {
                let bounds = outlined.px_bounds();
                outlined.draw(|gx, gy, coverage| {
                    let px = bounds.min.x.floor() as i32 + gx as i32;
                    let py = bounds.min.y.floor() as i32 + gy as i32;
                    if px >= 0 && py >= 0 && px < image.width() as i32 && py < image.height() as i32
                    {
                        blend_pixel(image.get_pixel_mut(px as u32, py as u32), color, coverage);
                    }
                });
            }
            x += advance;
        }
        baseline += size * 1.2;
    }
}

fn wrap_lines(font: &FontArc, text: &str, size: f32, max_width: f32) -> Vec<String> {
    let scaled = font.as_scaled(PxScale::from(size));
    let mut lines = vec![String::new()];
    let mut width = 0.0;
    for ch in text.chars() {
        if ch == '\n' {
            lines.push(String::new());
            width = 0.0;
            continue;
        }
        let advance = scaled.h_advance(font.glyph_id(ch));
        if width + advance > max_width && !lines.last().is_some_and(String::is_empty) {
            lines.push(String::new());
            width = 0.0;
        }
        lines.last_mut().expect("at least one line").push(ch);
        width += advance;
    }
    lines
}

fn blend_pixel(destination: &mut Rgba<u8>, source: Rgba<u8>, coverage: f32) {
    let alpha = coverage.clamp(0.0, 1.0) * source[3] as f32 / 255.0;
    for channel in 0..3 {
        destination[channel] =
            (source[channel] as f32 * alpha + destination[channel] as f32 * (1.0 - alpha)) as u8;
    }
    destination[3] = 255;
}

#[allow(dead_code)]
fn _path_context(path: &Path) -> Result<Vec<u8>> {
    std::fs::read(path).with_context(|| format!("读取字体失败：{}", path.display()))
}
