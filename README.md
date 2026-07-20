# Screen Translator

Windows 10/11 截屏翻译工具。支持本地 OCR、DeepSeek/OpenAI 兼容接口、Ollama、流式覆盖翻译、多屏高 DPI、全局热键和系统托盘。

## 开发运行

1. 安装 Rust stable 与 Visual Studio C++ Build Tools。
2. 将下列 RapidOCR PP-OCRv4 文件放入 `assets/ocr/`：
   - `ch_PP-OCRv4_det_infer.onnx`
   - `ch_PP-OCRv4_rec_infer.onnx`
   - `ppocr_keys_v4.txt`
   - 方向分类：`pp-lcnet_x0_25_textline_ori.onnx`
3. 确保 `onnxruntime.dll` 位于项目根目录。
4. 运行 `cargo run --release`。

首次 OCR 时才加载模型。API Key 通过 Windows DPAPI 加密，配置和历史位于
`%APPDATA%\ScreenTranslator\`。

## 使用

首次启动会打开设置；之后应用常驻系统托盘。填写 API 地址、Key、模型与目标语言并保存：

- 按 `Ctrl + Alt + T`，或点击托盘图标进入截屏。
- 在鼠标所在显示器拖动框选；`Esc` 或右键取消。
- 翻译期间再次按热键可取消；结果会按已完成的编号行流式刷新。
- 结果窗口支持钉住、缩放、复制译文/图片、保存 PNG 和重试。
- 托盘菜单可打开设置、历史记录或退出。

设置支持 DeepSeek、OpenAI、Ollama 和自定义 OpenAI 兼容服务，可配置代理、开机自启、OCR 预热和低置信度多模态 OCR 兜底。
应用会从 `NovaBreeze/ScreenTranslation` 的 GitHub Releases 检查更新；设置页可手动检查并安装 Windows x64 ZIP 发布包。

## 构建与分发

```powershell
cargo test
cargo build --release
./scripts/package.ps1
```

NSIS 安装脚本位于 `installer/installer.nsi`。

## 隐私

默认不会向云端发送截图，详见 [docs/privacy.md](docs/privacy.md)。

Made with Slint.
