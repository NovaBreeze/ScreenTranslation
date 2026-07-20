use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

pub struct ModelPaths {
    pub det: PathBuf,
    pub cls: PathBuf,
    pub rec: PathBuf,
    pub keys: PathBuf,
}

impl ModelPaths {
    pub fn discover() -> Result<Self> {
        let candidates = [
            std::env::current_exe()
                .ok()
                .and_then(|path| path.parent().map(|p| p.join("assets").join("ocr"))),
            std::env::current_dir()
                .ok()
                .map(|path| path.join("assets").join("ocr")),
        ];
        let root = candidates
            .into_iter()
            .flatten()
            .find(|path| path.join("ch_PP-OCRv4_det_infer.onnx").is_file())
            .context("未找到 OCR 模型。请将 PP-OCRv4 det/rec 和字典放入 assets/ocr/")?;

        let paths = Self {
            det: root.join("ch_PP-OCRv4_det_infer.onnx"),
            cls: root.join("pp-lcnet_x0_25_textline_ori.onnx"),
            rec: root.join("ch_PP-OCRv4_rec_infer.onnx"),
            keys: root.join("ppocr_keys_v4.txt"),
        };
        paths.validate()?;
        Ok(paths)
    }

    fn validate(&self) -> Result<()> {
        for path in [&self.det, &self.rec, &self.keys] {
            anyhow::ensure!(path.is_file(), "缺少 OCR 资源：{}", path.display());
        }
        Ok(())
    }
}

pub fn display_path(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}
