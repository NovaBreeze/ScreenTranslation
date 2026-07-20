# 翻译服务配置

Screen Translator 支持 OpenAI Chat Completions 兼容接口和 Ollama。

## OpenAI 兼容接口

填写 API 根地址即可，应用会自动补全 `/chat/completions`：

- DeepSeek：`https://api.deepseek.com/v1`
- OpenAI：`https://api.openai.com/v1`
- 通义兼容模式：对应服务商提供的 `/compatible-mode/v1`
- 智谱：对应服务商提供的 OpenAI 兼容根地址

请求使用带编号的文本：

```text
<1>第一行
<2>第二行
```

响应必须保留相同编号。应用会校验、整体重试一次，再对缺失行逐条补翻。

## Ollama

默认地址为 `http://127.0.0.1:11434`。请先在本机执行：

```powershell
ollama pull qwen3
```

设置中选择 Ollama 并填写已安装的模型名。该路径不会把 OCR 文本发送到外网。

## 代理

支持 HTTP、HTTPS 与 SOCKS5，例如：

```text
http://127.0.0.1:7890
socks5://127.0.0.1:1080
```

留空时使用系统代理设置。
