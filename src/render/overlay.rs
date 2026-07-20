use super::{
    color::{contrasting_text, pick_text_color, sample_background_color},
    font::{draw_text_wrapped, fit_font_size, load_font, wrapped_text_height},
};
use crate::ocr::{Rect, TextBlock};
use anyhow::Result;
use image::{DynamicImage, Rgba, RgbaImage};

#[cfg(test)]
pub fn render_overlay(
    original: &DynamicImage,
    blocks: &[TextBlock],
    translations: &[String],
) -> Result<DynamicImage> {
    render_overlay_with_style(original, blocks, translations, None, None, None)
}

pub fn render_overlay_with_style(
    original: &DynamicImage,
    blocks: &[TextBlock],
    translations: &[String],
    preferred_font: Option<&str>,
    configured_text: Option<Rgba<u8>>,
    configured_background: Option<Rgba<u8>>,
) -> Result<DynamicImage> {
    let source = original.to_rgba8();
    let mut output = source.clone();
    let font = load_font(preferred_font)?;

    for (index, block) in blocks.iter().enumerate() {
        let Some(translation) = translations.get(index) else {
            continue;
        };
        if translation.trim().is_empty() {
            continue;
        }
        let rect = clamp_rect(block.bounding_box(), output.width(), output.height());
        if rect.width == 0 || rect.height == 0 {
            continue;
        }
        let (sampled_background, purity) = sample_background_color(&source, rect);
        let background = configured_background.unwrap_or(sampled_background);
        let mut draw_rect = rect;
        let mut size = fit_font_size(&font, translation, draw_rect);
        if wrapped_text_height(&font, translation, size, draw_rect.width) > draw_rect.height as f32
        {
            draw_rect.height = draw_rect
                .height
                .saturating_add(rect.height)
                .min(output.height().saturating_sub(draw_rect.y));
            size = fit_font_size(&font, translation, draw_rect);
        }
        let text_color = if let Some(text) = configured_text {
            fill_rect(&mut output, draw_rect, background);
            text
        } else if purity >= 0.6 {
            let color_sample_rect = union_rects(&block.word_boxes).unwrap_or(rect);
            let color = pick_text_color(&source, color_sample_rect, background);
            fill_rect(&mut output, draw_rect, background);
            color
        } else {
            inpaint_rect(&mut output, &source, draw_rect);
            contrasting_text(background)
        };

        draw_text_wrapped(&mut output, &font, translation, draw_rect, size, text_color);
    }
    Ok(DynamicImage::ImageRgba8(output))
}

fn union_rects(rects: &[Rect]) -> Option<Rect> {
    let first = *rects.first()?;
    let (mut left, mut top) = (first.x, first.y);
    let (mut right, mut bottom) = (
        first.x.saturating_add(first.width),
        first.y.saturating_add(first.height),
    );
    for rect in &rects[1..] {
        left = left.min(rect.x);
        top = top.min(rect.y);
        right = right.max(rect.x.saturating_add(rect.width));
        bottom = bottom.max(rect.y.saturating_add(rect.height));
    }
    Some(Rect {
        x: left,
        y: top,
        width: right.saturating_sub(left),
        height: bottom.saturating_sub(top),
    })
}

fn clamp_rect(rect: Rect, width: u32, height: u32) -> Rect {
    let x = rect.x.min(width);
    let y = rect.y.min(height);
    Rect {
        x,
        y,
        width: rect.width.min(width.saturating_sub(x)),
        height: rect.height.min(height.saturating_sub(y)),
    }
}

fn fill_rect(image: &mut RgbaImage, rect: Rect, color: Rgba<u8>) {
    for y in rect.y..rect.y.saturating_add(rect.height).min(image.height()) {
        for x in rect.x..rect.x.saturating_add(rect.width).min(image.width()) {
            image.put_pixel(x, y, color);
        }
    }
}

fn inpaint_rect(image: &mut RgbaImage, source: &RgbaImage, rect: Rect) {
    if rect.width == 0 || rect.height == 0 || source.width() == 0 || source.height() == 0 {
        return;
    }
    let max_x = rect.x.saturating_add(rect.width).min(source.width());
    let max_y = rect.y.saturating_add(rect.height).min(source.height());
    let top_y = rect.y.saturating_sub(1).min(source.height() - 1);
    let bottom_y = max_y.min(source.height() - 1);
    let left_x = rect.x.saturating_sub(1).min(source.width() - 1);
    let right_x = max_x.min(source.width() - 1);
    for y in rect.y..rect.y.saturating_add(rect.height).min(image.height()) {
        for x in rect.x..rect.x.saturating_add(rect.width).min(image.width()) {
            let horizontal = (x - rect.x) as f32 / rect.width.saturating_sub(1).max(1) as f32;
            let vertical = (y - rect.y) as f32 / rect.height.saturating_sub(1).max(1) as f32;
            let top = source.get_pixel(x.min(source.width() - 1), top_y);
            let bottom = source.get_pixel(x.min(source.width() - 1), bottom_y);
            let left = source.get_pixel(left_x, y.min(source.height() - 1));
            let right = source.get_pixel(right_x, y.min(source.height() - 1));
            let pixel = image.get_pixel_mut(x, y);
            for channel in 0..3 {
                let vertical_color =
                    top[channel] as f32 * (1.0 - vertical) + bottom[channel] as f32 * vertical;
                let horizontal_color =
                    left[channel] as f32 * (1.0 - horizontal) + right[channel] as f32 * horizontal;
                pixel[channel] = ((vertical_color + horizontal_color) * 0.5).round() as u8;
            }
            pixel[3] = 255;
        }
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use crate::ocr::Point;

    #[test]
    fn renders_translation_inside_detected_line() {
        let source =
            DynamicImage::ImageRgba8(RgbaImage::from_pixel(320, 100, Rgba([245, 245, 245, 255])));
        let block = TextBlock {
            text: "hello".into(),
            polygon: vec![
                Point { x: 20.0, y: 20.0 },
                Point { x: 300.0, y: 20.0 },
                Point { x: 300.0, y: 70.0 },
                Point { x: 20.0, y: 70.0 },
            ],
            word_boxes: Vec::new(),
            confidence: 0.99,
        };
        let rendered = render_overlay(&source, &[block], &["你好，世界".into()])
            .expect("render should succeed");
        assert_ne!(source.to_rgba8(), rendered.to_rgba8());
    }
}
