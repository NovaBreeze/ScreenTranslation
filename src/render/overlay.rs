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

    // 第一遍 A：收集有效块及其检测框。
    let mut candidates: Vec<(usize, Rect)> = Vec::new();
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
        candidates.push((index, rect));
    }
    // 第一遍 B：行距预算 = 到下一个水平相交块顶边的距离；没有后续块则放开。
    // OCR 的 unclip 会把框撑大，框高不能代表真实行间距。
    let budgets: Vec<f32> = candidates
        .iter()
        .map(|(_, rect)| {
            let mut budget = rect.height as f32 * 2.0;
            for (_, other) in &candidates {
                let dy = other.y as f32 - rect.y as f32;
                let overlap_x = rect.x < other.x.saturating_add(other.width)
                    && other.x < rect.x.saturating_add(rect.width);
                if overlap_x && dy > 0.5 {
                    budget = budget.min(dy);
                }
            }
            budget
        })
        .collect();
    // 第一遍 C：逐块计算绘制矩形与独立适配字号。
    // 独立适配会让短译文占满行高、长译文被压缩——同一屏字号乱跳。
    let mut items: Vec<(usize, Rect, Rect, f32)> = Vec::new();
    for (i, (index, rect)) in candidates.iter().enumerate() {
        let translation = &translations[*index];
        let mut draw_rect = *rect;
        let mut size = fit_font_size(&font, translation, draw_rect, budgets[i]);
        if wrapped_text_height(&font, translation, size, draw_rect.width) > draw_rect.height as f32
        {
            draw_rect.height = draw_rect
                .height
                .saturating_add(rect.height)
                .min(output.height().saturating_sub(draw_rect.y));
            size = fit_font_size(&font, translation, draw_rect, budgets[i]);
        }
        items.push((*index, *rect, draw_rect, size));
    }
    let fitted: Vec<f32> = items.iter().map(|(_, _, _, size)| *size).collect();
    let unified = unify_sizes(&fitted);

    // 第二遍：先完成全部背景填充/修复，再统一绘制文字。
    // 否则下一行的背景会硬切掉上一行越出框底的字形。
    let mut draws = Vec::with_capacity(items.len());
    for (item_index, (block_index, rect, draw_rect, _)) in items.iter().enumerate() {
        let block = &blocks[*block_index];
        let rect = *rect;
        let draw_rect = *draw_rect;
        let size = unified[item_index];
        let (sampled_background, purity) = sample_background_color(&source, rect);
        let background = configured_background.unwrap_or(sampled_background);
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
        draws.push((*block_index, draw_rect, size, text_color));
    }
    for (block_index, draw_rect, size, text_color) in draws {
        draw_text_wrapped(
            &mut output,
            &font,
            &translations[block_index],
            draw_rect,
            size,
            text_color,
        );
    }
    Ok(DynamicImage::ImageRgba8(output))
}

/// 用各块独立适配字号的中位数做统一上限：短译文不再撑满行高，
/// 长译文保留自己的适配字号（不会溢出）。
fn unify_sizes(fitted: &[f32]) -> Vec<f32> {
    if fitted.is_empty() {
        return Vec::new();
    }
    let mut sorted = fitted.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let median = if sorted.len() % 2 == 1 {
        sorted[sorted.len() / 2]
    } else {
        (sorted[sorted.len() / 2 - 1] + sorted[sorted.len() / 2]) / 2.0
    };
    fitted.iter().map(|size| size.min(median)).collect()
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

#[cfg(test)]
mod unify_tests {
    use super::unify_sizes;

    #[test]
    fn short_translation_is_capped_to_median() {
        // 两条长译文适配出小字号，一条短译文占满行高：统一后全部对齐中位数。
        let unified = unify_sizes(&[14.0, 36.0, 14.0]);
        assert_eq!(unified, vec![14.0, 14.0, 14.0]);
    }

    #[test]
    fn long_translation_keeps_own_fitted_size() {
        // 中位数高于某些块的适配字号时，这些块保持较小字号不溢出。
        let unified = unify_sizes(&[10.0, 30.0, 32.0]);
        assert_eq!(unified, vec![10.0, 30.0, 30.0]);
    }

    #[test]
    fn single_block_is_unchanged() {
        assert_eq!(unify_sizes(&[22.0]), vec![22.0]);
        assert!(unify_sizes(&[]).is_empty());
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

    /// 复现用户反馈的场景：原文是三行等高终端文字，译文长短不一，
    /// 渲染后各行字号应基本一致（人工查看 target/render-uniform.png）。
    #[test]
    #[ignore = "生成视觉检查图片"]
    fn renders_uniform_font_size_for_mixed_lengths() {
        let make_block = |y: f32, text: &str| TextBlock {
            text: text.into(),
            polygon: vec![
                Point { x: 40.0, y },
                Point { x: 900.0, y },
                Point {
                    x: 900.0,
                    y: y + 28.0,
                },
                Point {
                    x: 40.0,
                    y: y + 28.0,
                },
            ],
            word_boxes: Vec::new(),
            confidence: 0.99,
        };
        let blocks = vec![
            make_block(20.0, "Welcome to Nushell,"),
            make_block(70.0, "based on the nu language,"),
            make_block(120.0, "where all data is structured!"),
        ];
        // 紧间距组：模拟 unclip 后框互相重叠的 OCR 输出（pitch < 框高），
        // 行距预算应压住字号，渲染行不得重叠。
        let tight = vec![make_block(170.0, "line one"), make_block(194.0, "line two")];
        let translations = vec![
            "欢迎来到 Nushell，这里是一个全新的 Shell".to_string(),
            "基于 nu 语言，".to_string(),
            "这里所有数据都是结构化的！支持管道操作".to_string(),
            "紧间距第一行文字渲染".to_string(),
            "紧间距第二行文字渲染".to_string(),
        ];
        let all: Vec<TextBlock> = blocks.into_iter().chain(tight).collect();
        let source =
            DynamicImage::ImageRgba8(RgbaImage::from_pixel(960, 260, Rgba([40, 44, 52, 255])));
        let rendered = render_overlay(&source, &all, &translations).expect("render");
        rendered.save("target/render-uniform.png").expect("save");
    }

    #[test]
    fn font_size_respects_vertical_budget() {
        let font = load_font(None).expect("font");
        let rect = Rect {
            x: 0,
            y: 0,
            width: 800,
            height: 28,
        };
        let capped = fit_font_size(&font, "短句", rect, 20.0);
        assert!(
            capped * 1.25 <= 20.0 + f32::EPSILON,
            "size {capped} 超出行距预算"
        );
        let free = fit_font_size(&font, "短句", rect, 200.0);
        assert!(free > capped, "预算充足时不应被压低：{free} vs {capped}");
    }
}
