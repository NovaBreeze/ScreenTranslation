# 翻译服务配置

Screen Translator 支持 OpenAI Chat Completions 兼容接口和 Ollama。

## 多供应商与失败回退

设置页可添加多个翻译供应商，每个供应商可填多个模型（逗号分隔）。
翻译时按「供应商列表顺序 × 模型顺序」组成回退链：当前节点请求失败时自动
切换到下一个节点；流式输出中途失败时，只对尚未译出的行切换，已译出的行
保留。列表中可用 ↑ ↓ 调整优先级。

## OpenCode Go / Zen

OpenCode 的 Go 订阅与 Zen 按量/免费模型共用同一把 API Key（在
[opencode.ai](https://opencode.ai/auth) 控制台创建），预设已内置：

- OpenCode Go：`https://opencode.ai/zen/go/v1`，如 `deepseek-v4-flash`、`glm-5.1`
- OpenCode Zen 免费：`https://opencode.ai/zen/v1`，如 `big-pickle`、
  `mimo-v2.5-free`、`deepseek-v4-flash-free`

注意：免费模型的用量数据可能被 OpenCode 用于改进模型，请勿截取敏感内容。

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
