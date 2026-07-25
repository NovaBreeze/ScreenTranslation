use super::models::{ModelPaths, display_path};
use anyhow::{Context, Result};
use image::DynamicImage;
use oar_ocr::oarocr::{OAROCR, OAROCRBuilder};
use oar_ocr::{domain::TextDetectionConfig, processors::LimitType};

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Point {
    pub x: f32,
    pub y: f32,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone)]
pub struct TextBlock {
    pub text: String,
    pub polygon: Vec<Point>,
    pub word_boxes: Vec<Rect>,
    pub confidence: f32,
}

impl TextBlock {
    pub fn bounding_box(&self) -> Rect {
        let min_x = self
            .polygon
            .iter()
            .map(|p| p.x)
            .fold(f32::INFINITY, f32::min)
            .max(0.0);
        let min_y = self
            .polygon
            .iter()
            .map(|p| p.y)
            .fold(f32::INFINITY, f32::min)
            .max(0.0);
        let max_x = self
            .polygon
            .iter()
            .map(|p| p.x)
            .fold(0.0, f32::max)
            .max(min_x + 1.0);
        let max_y = self
            .polygon
            .iter()
            .map(|p| p.y)
            .fold(0.0, f32::max)
            .max(min_y + 1.0);
        Rect {
            x: min_x.floor() as u32,
            y: min_y.floor() as u32,
            width: (max_x - min_x).ceil() as u32,
            height: (max_y - min_y).ceil() as u32,
        }
    }
}

pub struct OcrEngine {
    pipeline: Option<OAROCR>,
}

impl Default for OcrEngine {
    fn default() -> Self {
        Self { pipeline: None }
    }
}

impl OcrEngine {
    pub fn prewarm(&mut self) -> Result<()> {
        if self.pipeline.is_none() {
            self.pipeline = Some(Self::load()?);
        }
        Ok(())
    }

    pub fn recognize(&mut self, image: &DynamicImage) -> Result<Vec<TextBlock>> {
        self.prewarm()?;
        let pipeline = self.pipeline.as_ref().expect("OCR pipeline initialized");
        let result = pipeline
            .predict(vec![image.to_rgb8()])
            .context("OCR 推理失败")?
            .into_iter()
            .next()
            .context("OCR 未返回结果")?;

        let mut blocks: Vec<TextBlock> = result
            .text_regions
            .into_iter()
            .filter_map(|region| {
                let word_boxes = region
                    .word_boxes
                    .as_ref()
                    .map(|boxes| {
                        boxes
                            .iter()
                            .map(|bbox| Rect {
                                x: bbox.x_min().max(0.0).floor() as u32,
                                y: bbox.y_min().max(0.0).floor() as u32,
                                width: (bbox.x_max() - bbox.x_min()).max(1.0).ceil() as u32,
                                height: (bbox.y_max() - bbox.y_min()).max(1.0).ceil() as u32,
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                let text = region.text?.trim().to_owned();
                if text.is_empty() {
                    return None;
                }
                Some(TextBlock {
                    text,
                    polygon: region
                        .bounding_box
                        .points
                        .iter()
                        .map(|point| Point {
                            x: point.x,
                            y: point.y,
                        })
                        .collect(),
                    word_boxes,
                    confidence: region.confidence.unwrap_or(0.0),
                })
            })
            .collect();

        // 阅读顺序：先按 y 排序后按行带聚类出行号，再按 (行号, x) 排序。
        // 两两容差比较不满足传递性（a≈b、b≈c 但 a≉c），release 下 panic=abort
        // 会直接闪退，这里必须用全序键。
        blocks.sort_by_key(|block| block.bounding_box().y);
        let mut tagged: Vec<(usize, TextBlock)> = Vec::with_capacity(blocks.len());
        let mut row = 0usize;
        let mut band_bottom = 0u32;
        for block in blocks.drain(..) {
            let rect = block.bounding_box();
            if rect.y > band_bottom {
                row += 1;
                band_bottom = rect.y + rect.height;
            } else {
                band_bottom = band_bottom.max(rect.y + rect.height);
            }
            tagged.push((row, block));
        }
        tagged.sort_by_key(|(row, block)| (*row, block.bounding_box().x));
        blocks.extend(tagged.into_iter().map(|(_, block)| block));

        // 几何空格重建：rec 模型对词间空格预测不稳定（同一行时有时无），
        // 按行图列投影把明显的墨水中断补回为空格。
        let gray = image.to_luma8();
        for block in &mut blocks {
            let rect = block.bounding_box();
            let x = rect.x.min(gray.width().saturating_sub(1));
            let y = rect.y.min(gray.height().saturating_sub(1));
            let width = rect.width.min(gray.width() - x);
            let height = rect.height.min(gray.height() - y);
            if width < 8 || height < 6 {
                continue;
            }
            let line = image::imageops::crop_imm(&gray, x, y, width, height).to_image();
            block.text = restore_spaces(&block.text, &line).trim().to_owned();
        }
        Ok(blocks)
    }

    fn load() -> Result<OAROCR> {
        initialize_onnx_runtime()?;
        let paths = ModelPaths::discover()?;
        // det 默认把长边压到 960px，屏幕文字（常见 12–16px 高）会缩到检测下限以下。
        // 放宽到 1920 覆盖全高清选区不降采样；更长边仍受限以控制内存/耗时。
        let detection = TextDetectionConfig {
            limit_side_len: Some(1920),
            limit_type: Some(LimitType::Max),
            max_side_len: Some(4000),
            ..Default::default()
        };
        let mut builder = OAROCRBuilder::new(
            display_path(&paths.det),
            display_path(&paths.rec),
            display_path(&paths.keys),
        )
        .text_detection_config(detection)
        .image_batch_size(1)
        .region_batch_size(8)
        .return_word_box(true);
        if paths.cls.is_file() {
            builder = builder.with_text_line_orientation_classification(display_path(&paths.cls));
        }
        builder.build().context("加载 PP-OCRv4 模型失败")
    }
}

/// 按块中心点过滤：只保留中心落在 region 内的块。配合外扩裁剪使用——
/// 裁片扩出选区给检测器上下文，识别后丢弃扩边带进来的相邻行。
pub fn filter_to_region(blocks: &[TextBlock], region: &Rect) -> Vec<TextBlock> {
    blocks
        .iter()
        .filter(|block| {
            let rect = block.bounding_box();
            let cx = rect.x + rect.width / 2;
            let cy = rect.y + rect.height / 2;
            cx >= region.x
                && cx < region.x + region.width
                && cy >= region.y
                && cy < region.y + region.height
        })
        .cloned()
        .collect()
}

/// 把同一视觉行上被切碎的块并回一行：紧贴单行的小裁片会让检测器在图标/
/// 大间距处断行，碎块独立翻译会丢失上下文且小框渲染压缩字号。
/// 只并水平间距小的同高度行；行与行（分点/段落）绝不合并。
pub fn merge_line_fragments(blocks: &[TextBlock]) -> Vec<TextBlock> {
    let mut lines: Vec<TextBlock> = Vec::new();
    for block in blocks {
        let rect = block.bounding_box();
        let merge = lines
            .last()
            .is_some_and(|tail| same_line(&tail.bounding_box(), &rect));
        if merge {
            let tail = lines.last_mut().expect("tail exists");
            tail.text = join_fragments(&tail.text, &block.text);
            let union = union_rect(tail.bounding_box(), rect);
            tail.polygon = vec![
                Point {
                    x: union.x as f32,
                    y: union.y as f32,
                },
                Point {
                    x: (union.x + union.width) as f32,
                    y: union.y as f32,
                },
                Point {
                    x: (union.x + union.width) as f32,
                    y: (union.y + union.height) as f32,
                },
                Point {
                    x: union.x as f32,
                    y: (union.y + union.height) as f32,
                },
            ];
            tail.word_boxes.extend(block.word_boxes.iter().copied());
            tail.confidence = tail.confidence.min(block.confidence);
        } else {
            lines.push(block.clone());
        }
    }
    lines
}

/// 同一视觉行判定：竖直重叠超过较矮块的一半，且水平间距不超过 2 倍行高
///（覆盖空格/分隔符间隙；更大间距视为分栏）。负间距（unclip 重叠）允许。
fn same_line(tail: &Rect, next: &Rect) -> bool {
    let line = tail.height.min(next.height) as i64;
    let overlap_y =
        (tail.y + tail.height).min(next.y + next.height) as i64 - tail.y.max(next.y) as i64;
    if overlap_y * 2 < line {
        return false;
    }
    let gap = next.x as i64 - (tail.x + tail.width) as i64;
    gap >= -line && gap <= line * 2
}

/// 拼接同行碎块：任一侧是 CJK（含全角标点）时不加空格，其余补一个空格。
fn join_fragments(tail: &str, next: &str) -> String {
    let tail = tail.trim_end();
    let next = next.trim_start();
    let cjk = |c: char| {
        matches!(
            c,
            '\u{4e00}'..='\u{9fff}' | '\u{3000}'..='\u{303f}' | '\u{ff00}'..='\u{ffef}'
        )
    };
    let needs_space = !tail.chars().last().is_some_and(cjk) && !next.chars().next().is_some_and(cjk);
    if needs_space {
        format!("{tail} {next}")
    } else {
        format!("{tail}{next}")
    }
}

fn union_rect(a: Rect, b: Rect) -> Rect {
    let x = a.x.min(b.x);
    let y = a.y.min(b.y);
    let right = (a.x + a.width).max(b.x + b.width);
    let bottom = (a.y + a.height).max(b.y + b.height);
    Rect {
        x,
        y,
        width: right - x,
        height: bottom - y,
    }
}

/// PP-OCRv4 中文识别模型对词间空格预测不稳定（同一行时有时无，word_boxes
/// 是均分合成的、无几何信息）。用行图列投影做几何重建：墨迹边界内足够宽的
/// 无墨空隙按比例映射回字符边界补空格。模型已识别的空格不重复添加；两侧
/// 均非 ASCII 字母数字（多为纯中文行的宽字形间隙）时不插入。
fn restore_spaces(text: &str, line: &image::GrayImage) -> String {
    let char_count = text.chars().count();
    if char_count < 3 || line.width() < 8 || line.height() < 6 {
        return text.to_owned();
    }
    let width = line.width() as usize;
    let height = line.height() as usize;
    let raw = line.as_raw();

    // Otsu 阈值二值化；墨迹取像素较少的一侧（文字总是少数）。
    let mut hist = [0u32; 256];
    for &v in raw {
        hist[v as usize] += 1;
    }
    let total = (width * height) as f32;
    let sum: f32 = hist
        .iter()
        .enumerate()
        .map(|(i, &c)| i as f32 * c as f32)
        .sum();
    let mut sum_background = 0.0f32;
    let mut weight_background = 0u32;
    let mut threshold = 128u8;
    let mut best = -1.0f32;
    for (t, &count) in hist.iter().enumerate() {
        weight_background += count;
        if weight_background == 0 {
            continue;
        }
        let weight_foreground = (width * height) as u32 - weight_background;
        if weight_foreground == 0 {
            break;
        }
        sum_background += t as f32 * count as f32;
        let mean_background = sum_background / weight_background as f32;
        let mean_foreground = (sum - sum_background) / weight_foreground as f32;
        let between = weight_background as f32
            * weight_foreground as f32
            * (mean_background - mean_foreground)
            * (mean_background - mean_foreground)
            / (total * total);
        if between > best {
            best = between;
            threshold = t as u8;
        }
    }
    let dark: u32 = hist[..=threshold as usize].iter().sum();
    let ink_is_dark = (dark as f32) <= total / 2.0;

    // 列墨迹分布：任一像素为墨即整列有墨。
    let mut col_ink = vec![false; width];
    for (x, column) in col_ink.iter_mut().enumerate() {
        for y in 0..height {
            let v = raw[y * width + x];
            if (v <= threshold) == ink_is_dark {
                *column = true;
                break;
            }
        }
    }
    let Some(ink_left) = col_ink.iter().position(|&v| v) else {
        return text.to_owned();
    };
    let ink_right = col_ink.iter().rposition(|&v| v).expect("ink exists");
    if ink_right <= ink_left {
        return text.to_owned();
    }

    // 候选空隙：墨迹边界内 ≥3px 的无墨列。
    let mut gaps: Vec<(usize, usize)> = Vec::new();
    let mut gap_start = None;
    for x in ink_left..=ink_right {
        match (col_ink[x], gap_start) {
            (false, None) => gap_start = Some(x),
            (true, Some(start)) => {
                if x - start >= 3 {
                    gaps.push((start, x));
                }
                gap_start = None;
            }
            _ => {}
        }
    }
    if gaps.is_empty() {
        return text.to_owned();
    }

    // 空格 = 整字符单元的空隙（等宽字体终端是主场景）：先按 字符数 估计单元宽，
    // ≥0.7 单元的空隙才算空格并回补缺失单元数，迭代到稳定。
    // 比例字体的窄字母间隙（F:、l( 等 3–6px）低于 0.7 单元，不误插。
    let chars: Vec<char> = text.chars().collect();
    let char_count = chars.len();
    let span = (ink_right - ink_left) as f32;
    let mut extra = 0usize;
    let mut cell;
    for _ in 0..3 {
        cell = span / (char_count + extra) as f32;
        let missing: usize = gaps
            .iter()
            .filter(|&&(start, end)| end - start >= (cell * 0.7) as usize)
            .map(|&(start, end)| (((end - start) as f32 / cell).round().max(1.0)) as usize)
            .sum();
        if missing == extra {
            break;
        }
        extra = missing;
    }
    cell = span / (char_count + extra) as f32;
    let min_gap = 3.max((cell * 0.7) as usize);

    let mut insertions: Vec<(usize, usize)> = Vec::new(); // (字符边界, 空格数)
    let mut inserted_cells = 0usize;
    for &(start, end) in &gaps {
        if end - start < min_gap {
            continue;
        }
        let count = (((end - start) as f32 / cell).round().max(1.0)) as usize;
        let boundary = (((start - ink_left) as f32 / cell).round().max(1.0) as usize)
            .saturating_sub(inserted_cells)
            .clamp(1, char_count - 1);
        inserted_cells += count;
        // 只补“恰好一个单元”的空隙：≥2 单元的宽空隙在文本行里多半是
        // 图标/栏间距而非双空格，插入会比缺失更难看。
        if count != 1 {
            continue;
        }
        // 模型已识别该处空格（±1 字符容差）：消耗单元但不重复插入。
        let has_model_space = chars[boundary - 1] == ' '
            || chars[boundary] == ' '
            || (boundary >= 2 && chars[boundary - 2] == ' ')
            || (boundary + 1 < char_count && chars[boundary + 1] == ' ');
        if has_model_space {
            continue;
        }
        // 两侧均非 ASCII 字母数字（多为纯中文行的宽字形间隙）时不插入。
        if !chars[boundary - 1].is_ascii_alphanumeric()
            && !chars[boundary].is_ascii_alphanumeric()
        {
            continue;
        }
        if let Some(last) = insertions.last_mut()
            && last.0 == boundary
        {
            last.1 = last.1.max(count);
        } else {
            insertions.push((boundary, count));
        }
    }
    if insertions.is_empty() {
        return text.to_owned();
    }
    let mut restored = String::with_capacity(text.len() + insertions.len());
    for (i, ch) in chars.iter().enumerate() {
        if let Some(&(_, count)) = insertions.iter().find(|&&(boundary, _)| boundary == i) {
            for _ in 0..count {
                restored.push(' ');
            }
        }
        restored.push(*ch);
    }
    restored
}

fn initialize_onnx_runtime() -> Result<()> {    let candidates = [
        std::env::current_exe()
            .ok()
            .and_then(|path| path.parent().map(|parent| parent.join("onnxruntime.dll"))),
        std::env::current_dir()
            .ok()
            .map(|path| path.join("onnxruntime.dll")),
    ];
    let path = candidates
        .into_iter()
        .flatten()
        .find(|path| path.is_file())
        .context("未找到 onnxruntime.dll，请将它放在程序同目录")?;
    let _ = ort::init_from(path)
        .context("加载 onnxruntime.dll 失败")?
        .commit();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 构造二值行图：`runs` 为有墨列区间（rows 4..16），其余为背景。
    /// 墨占比保持 <50%，使 Otsu 墨迹侧判定稳定。
    fn paint(width: u32, height: u32, runs: &[(usize, usize)], ink_on_dark: bool) -> image::GrayImage {
        let (bg, ink) = if ink_on_dark { (255u8, 0u8) } else { (0u8, 255u8) };
        let mut img = image::GrayImage::from_pixel(width, height, image::Luma([bg]));
        for &(start, end) in runs {
            for x in start..end {
                for y in 4..16 {
                    img.put_pixel(x as u32, y, image::Luma([ink]));
                }
            }
        }
        img
    }

    // 8 个字符单元（10px 一个），第 4/5 字符之间隔了一个空格单元。
    const GAPPED_RUNS: &[(usize, usize)] = &[
        (0, 8),
        (10, 18),
        (20, 28),
        (30, 38),
        (50, 58),
        (60, 68),
        (70, 78),
        (80, 88),
    ];
    const TIGHT_RUNS: &[(usize, usize)] = &[
        (0, 8),
        (10, 18),
        (20, 28),
        (30, 38),
        (40, 48),
        (50, 58),
        (60, 68),
        (70, 78),
    ];

    fn block(text: &str, x: u32, y: u32, width: u32, height: u32) -> TextBlock {
        TextBlock {
            text: text.to_owned(),
            polygon: vec![
                Point {
                    x: x as f32,
                    y: y as f32,
                },
                Point {
                    x: (x + width) as f32,
                    y: y as f32,
                },
                Point {
                    x: (x + width) as f32,
                    y: (y + height) as f32,
                },
                Point {
                    x: x as f32,
                    y: (y + height) as f32,
                },
            ],
            word_boxes: vec![Rect {
                x,
                y,
                width,
                height,
            }],
            confidence: 0.9,
        }
    }

    #[test]
    fn same_line_fragments_merge_into_one_line() {
        // 终端状态行被切成三截（图标后、大间距处）：应并回一行。
        let blocks = vec![
            block("Launch start: app", 10, 100, 200, 30),
            block("F:/Workspace/ScreenTranslator/target/re…", 230, 102, 500, 28),
            block("· running · pid 16912", 760, 101, 300, 29),
        ];
        let lines = merge_line_fragments(&blocks);
        assert_eq!(lines.len(), 1);
        assert_eq!(
            lines[0].text,
            "Launch start: app F:/Workspace/ScreenTranslator/target/re… · running · pid 16912"
        );
        let rect = lines[0].bounding_box();
        assert_eq!(rect.x, 10);
        assert_eq!(rect.width, 1060 - 10);
    }

    #[test]
    fn stacked_lines_do_not_merge() {
        // 分点/多行：竖直无重叠，绝不能并。
        let blocks = vec![
            block("first bullet", 10, 100, 300, 30),
            block("second bullet", 10, 140, 300, 30),
        ];
        assert_eq!(merge_line_fragments(&blocks).len(), 2);
    }

    #[test]
    fn wide_column_gap_does_not_merge() {
        // 同一基线上的分栏：间距 3 倍行高以上。
        let blocks = vec![
            block("left cell", 10, 100, 200, 30),
            block("right cell", 400, 100, 200, 30),
        ];
        assert_eq!(merge_line_fragments(&blocks).len(), 2);
    }

    #[test]
    fn overlapping_fragments_merge() {
        // unclip 撑大后框互相重叠（负间距）。
        let blocks = vec![
            block("part one", 10, 100, 200, 30),
            block("part two", 190, 100, 200, 30),
        ];
        let lines = merge_line_fragments(&blocks);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].text, "part one part two");
    }

    #[test]
    fn cjk_fragments_join_without_space() {
        let blocks = vec![
            block("状态：", 10, 100, 100, 30),
            block("运行中", 120, 100, 100, 30),
        ];
        let lines = merge_line_fragments(&blocks);
        assert_eq!(lines[0].text, "状态：运行中");
    }

    #[test]
    fn filter_to_region_keeps_only_centered_blocks() {
        let region = Rect {
            x: 50,
            y: 50,
            width: 300,
            height: 60,
        };
        let inside = block("inside", 60, 60, 200, 30); // 中心 (160,75) 在内
        let above = block("above", 60, 10, 200, 30); // 中心 (160,25) 在外
        let below = block("below", 60, 120, 200, 30); // 中心 (160,135) 在外
        let straddle_inside = block("straddle", 60, 80, 200, 50); // 中心 (160,105) 在内
        let kept = filter_to_region(&[inside, above, below, straddle_inside], &region);
        let texts: Vec<&str> = kept.iter().map(|block| block.text.as_str()).collect();
        assert_eq!(texts, ["inside", "straddle"]);
    }

    #[test]
    fn restores_space_at_wide_ink_gap() {
        let line = paint(100, 20, GAPPED_RUNS, true);
        assert_eq!(restore_spaces("aaaaaaaa", &line), "aaaa aaaa");
    }

    #[test]
    fn restores_space_for_light_on_dark_terminal_text() {
        let line = paint(100, 20, GAPPED_RUNS, false);
        assert_eq!(restore_spaces("aaaaaaaa", &line), "aaaa aaaa");
    }

    #[test]
    fn tight_char_gaps_do_not_gain_spaces() {
        let line = paint(100, 20, TIGHT_RUNS, true);
        assert_eq!(restore_spaces("aaaaaaaa", &line), "aaaaaaaa");
    }

    #[test]
    fn cjk_neighbors_do_not_gain_spaces() {
        let line = paint(100, 20, GAPPED_RUNS, true);
        assert_eq!(restore_spaces("你好世界你好世界", &line), "你好世界你好世界");
    }

    #[test]
    fn existing_model_space_is_not_duplicated() {
        let line = paint(100, 20, GAPPED_RUNS, true);
        assert_eq!(restore_spaces("aaaa aaaa", &line), "aaaa aaaa");
    }

    #[test]
    fn bundled_models_load() {
        OcrEngine::load().expect("bundled OCR models should load");
    }

    #[test]
    fn fixture_runs_end_to_end_ocr() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/chinese-text.png");
        let image = image::open(path).expect("fixture image should load");
        let mut engine = OcrEngine::default();
        let blocks = engine.recognize(&image).expect("fixture OCR should run");
        assert!(!blocks.is_empty());
        assert!(blocks.iter().all(|block| block.confidence > 0.0));
    }

    /// 诊断：真实屏幕文本的空格是否被识别保留（cargo test -- --ignored --nocapture）。
    /// 输入是应用自己转储的冻结帧（含英文终端文本）。
    #[test]
    #[ignore = "诊断用，打印识别文本与 word_boxes"]
    fn english_spaces_are_recognized() {
        let path = crate::logging::log_dir().join("last-frame.png");
        let image = image::open(&path).expect("last-frame.png should exist");
        let mut engine = OcrEngine::default();
        let blocks = engine.recognize(&image).expect("ocr");
        for (i, block) in blocks.iter().enumerate() {
            let ascii = block.text.chars().filter(|c| c.is_ascii()).count();
            if ascii < 8 {
                continue;
            }
            let chars = block.text.chars().count();
            let gaps: Vec<i32> = block
                .word_boxes
                .windows(2)
                .map(|pair| pair[1].x as i32 - (pair[0].x + pair[0].width) as i32)
                .collect();
            println!(
                "block {i}: chars={} boxes={} gaps={:?} text={:?}",
                chars,
                block.word_boxes.len(),
                gaps,
                block.text
            );
        }
    }
}
