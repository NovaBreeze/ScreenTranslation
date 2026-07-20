# 故障排查

## 提示缺少 OCR 模型

确认 `assets/ocr/` 中存在 det、rec、`pp-lcnet_x0_25_textline_ori.onnx` 和 `ppocr_keys_v4.txt`。
开发运行时从项目目录读取，分发时应将 `assets/` 放在 exe 同级目录。

## OCR 模型加载失败

检查模型是否为 ONNX 格式且文件完整。应用使用 CPU 版 ONNX Runtime；发布包必须包含
与程序匹配的 `onnxruntime.dll`。

## 翻译接口返回 401/403

检查 API Key、API 地址和模型权限。API 地址可以填写 `https://host/v1`，应用会自动补全
`/chat/completions`。

## 代理连接失败

代理需填写完整 URL，例如 `http://127.0.0.1:7890` 或 `socks5://127.0.0.1:1080`。

## 识别效果差

竖排文字、艺术字、描边字和低分辨率文字是本地模型的已知弱项。尽量缩小选区并让文字保持清晰。

## Ctrl + Alt + T 没有反应

确认托盘中存在“截屏翻译”图标。若设置页提示热键注册失败，说明该组合已被其他程序占用；关闭冲突程序后重新启动。本应用不需要管理员权限。

## 托盘图标未显示

Windows 可能将新图标折叠到任务栏的隐藏图标区域。也可再次运行程序；第二实例会唤醒首实例的设置窗口。

## 检查更新返回 404

自动更新读取 `NovaBreeze/ScreenTranslation` 的最新 GitHub Release。仓库尚未发布 Release 时 GitHub 会返回 404；创建带 Windows x64 ZIP 资源的首个 Release 后即可正常使用。开发目录中的构建不会自动覆盖源码工作区。
