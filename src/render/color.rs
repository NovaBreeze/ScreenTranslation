use crate::ocr::Rect;
use image::{Rgba, RgbaImage};
use std::collections::HashMap;

pub fn sample_background_color(image: &RgbaImage, rect: Rect) -> (Rgba<u8>, f32) {
    let mut histogram = HashMap::<u16, usize>::new();
    let mut sum = HashMap::<u16, [u64; 3]>::new();
    let points = border_points(rect, 4, image.width(), image.height());
    for (x, y) in &points {
        let pixel = image.get_pixel(*x, *y);
        let key =
            ((pixel[0] as u16 >> 4) << 8) | ((pixel[1] as u16 >> 4) << 4) | (pixel[2] as u16 >> 4);
        *histogram.entry(key).or_default() += 1;
        let entry = sum.entry(key).or_default();
        entry[0] += pixel[0] as u64;
        entry[1] += pixel[1] as u64;
        entry[2] += pixel[2] as u64;
    }
    let Some((key, count)) = histogram.into_iter().max_by_key(|(_, count)| *count) else {
        return (Rgba([255, 255, 255, 255]), 0.0);
    };
    let rgb = sum[&key];
    (
        Rgba([
            (rgb[0] / count as u64) as u8,
            (rgb[1] / count as u64) as u8,
            (rgb[2] / count as u64) as u8,
            255,
        ]),
        count as f32 / points.len().max(1) as f32,
    )
}

pub fn contrasting_text(background: Rgba<u8>) -> Rgba<u8> {
    let luminance = 0.2126 * background[0] as f32
        + 0.7152 * background[1] as f32
        + 0.0722 * background[2] as f32;
    if luminance > 145.0 {
        Rgba([18, 24, 33, 255])
    } else {
        Rgba([250, 250, 250, 255])
    }
}

pub fn pick_text_color(image: &RgbaImage, rect: Rect, background: Rgba<u8>) -> Rgba<u8> {
    let x1 = rect.x.saturating_add(rect.width).min(image.width());
    let y1 = rect.y.saturating_add(rect.height).min(image.height());
    let mut red = Vec::new();
    let mut green = Vec::new();
    let mut blue = Vec::new();
    for y in rect.y.min(image.height())..y1 {
        for x in rect.x.min(image.width())..x1 {
            let pixel = image.get_pixel(x, y);
            let distance = (pixel[0] as i16 - background[0] as i16).unsigned_abs()
                + (pixel[1] as i16 - background[1] as i16).unsigned_abs()
                + (pixel[2] as i16 - background[2] as i16).unsigned_abs();
            if distance > 72 {
                red.push(pixel[0]);
                green.push(pixel[1]);
                blue.push(pixel[2]);
            }
        }
    }
    if red.len() < 4 {
        return contrasting_text(background);
    }
    red.sort_unstable();
    green.sort_unstable();
    blue.sort_unstable();
    let middle = red.len() / 2;
    let candidate = Rgba([red[middle], green[middle], blue[middle], 255]);
    if contrast_ratio(candidate, background) >= 3.0 {
        candidate
    } else {
        contrasting_text(background)
    }
}

fn contrast_ratio(left: Rgba<u8>, right: Rgba<u8>) -> f32 {
    fn luminance(color: Rgba<u8>) -> f32 {
        let convert = |channel: u8| {
            let value = channel as f32 / 255.0;
            if value <= 0.03928 {
                value / 12.92
            } else {
                ((value + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * convert(color[0]) + 0.7152 * convert(color[1]) + 0.0722 * convert(color[2])
    }
    let a = luminance(left);
    let b = luminance(right);
    (a.max(b) + 0.05) / (a.min(b) + 0.05)
}

fn border_points(rect: Rect, padding: u32, width: u32, height: u32) -> Vec<(u32, u32)> {
    if width == 0 || height == 0 {
        return Vec::new();
    }
    let x0 = rect.x.saturating_sub(padding).min(width - 1);
    let y0 = rect.y.saturating_sub(padding).min(height - 1);
    let x1 = rect
        .x
        .saturating_add(rect.width)
        .saturating_add(padding)
        .min(width - 1);
    let y1 = rect
        .y
        .saturating_add(rect.height)
        .saturating_add(padding)
        .min(height - 1);
    let mut points = Vec::new();
    for x in x0..=x1 {
        points.push((x, y0));
        points.push((x, y1));
    }
    for y in y0..=y1 {
        points.push((x0, y));
        points.push((x1, y));
    }
    points
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quantized_mode_ignores_small_noise() {
        let mut image = RgbaImage::from_pixel(20, 20, Rgba([245, 245, 245, 255]));
        image.put_pixel(0, 0, Rgba([240, 242, 244, 255]));
        let (color, purity) = sample_background_color(
            &image,
            Rect {
                x: 2,
                y: 2,
                width: 12,
                height: 12,
            },
        );
        assert!(color[0] > 235);
        assert!(purity > 0.9);
    }
}
