use super::models::{ModelPaths, display_path};
use anyhow::{Context, Result};
use image::DynamicImage;
use oar_ocr::oarocr::{OAROCR, OAROCRBuilder};

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

        blocks.sort_by(|a, b| {
            let aa = a.bounding_box();
            let bb = b.bounding_box();
            let row_tolerance = aa.height.max(bb.height) as f32 * 0.55;
            if (aa.y as f32 - bb.y as f32).abs() <= row_tolerance {
                aa.x.cmp(&bb.x)
            } else {
                aa.y.cmp(&bb.y)
            }
        });
        Ok(blocks)
    }

    fn load() -> Result<OAROCR> {
        initialize_onnx_runtime()?;
        let paths = ModelPaths::discover()?;
        let mut builder = OAROCRBuilder::new(
            display_path(&paths.det),
            display_path(&paths.rec),
            display_path(&paths.keys),
        )
        .image_batch_size(1)
        .region_batch_size(8)
        .return_word_box(true);
        if paths.cls.is_file() {
            builder = builder.with_text_line_orientation_classification(display_path(&paths.cls));
        }
        builder.build().context("加载 PP-OCRv4 模型失败")
    }
}

fn initialize_onnx_runtime() -> Result<()> {
    let candidates = [
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
}
