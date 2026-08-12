//! Client for synchronous and streaming OpenAI-compatible chat completion APIs.

use anyhow::{Context, Result, anyhow, bail};
use eventsource_stream::Eventsource;
use futures::StreamExt;
use reqwest::{Client, Proxy, Url};
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::config::ProviderConfig;

use super::parser::{NumberedStreamParser, format_numbered, parse_numbered};

#[derive(Clone)]
pub struct OpenAiTranslator {
    client: Client,
    endpoint: Url,
    api_key: String,
    model: String,
    target_lang: String,
}

impl OpenAiTranslator {
    pub fn new(
        api_base: impl AsRef<str>,
        api_key: impl Into<String>,
        model: impl Into<String>,
        target_lang: impl Into<String>,
        proxy: Option<&str>,
    ) -> Result<Self> {
        let mut builder = Client::builder().timeout(std::time::Duration::from_secs(30));
        if let Some(proxy) = proxy.filter(|value| !value.trim().is_empty()) {
            builder = builder
                .proxy(Proxy::all(proxy).with_context(|| format!("invalid proxy URL: {proxy}"))?);
        }

        let api_key = api_key.into();
        if api_key.trim().is_empty() {
            bail!("API key is empty");
        }
        let model = model.into();
        if model.trim().is_empty() {
            bail!("model is empty");
        }

        Ok(Self {
            client: builder.build().context("failed to build HTTP client")?,
            endpoint: normalize_chat_completions_url(api_base.as_ref())?,
            api_key,
            model,
            target_lang: target_lang.into(),
        })
    }

    /// Construct a translator for one provider/model pair, decrypting the stored key.
    pub fn from_provider(
        provider: &ProviderConfig,
        model: &str,
        target_lang: &str,
        proxy: Option<&str>,
    ) -> Result<Self> {
        let api_key = provider
            .api_key()?
            .ok_or_else(|| anyhow!("供应商「{}」未配置 API key", provider.display_name()))?;
        Self::new(&provider.api_base, api_key, model, target_lang, proxy)
    }

    /// Translate lines while preserving their order.
    ///
    /// An incomplete batch is retried once as a whole. If that second response
    /// is still incomplete, each missing source line is requested separately.
    pub async fn translate(&self, lines: &[String]) -> Result<Vec<String>> {
        if lines.is_empty() {
            return Ok(Vec::new());
        }

        let first = self.request_batch(lines).await?;
        let mut aligned = parse_numbered(&first, lines.len());

        if aligned.iter().any(Option::is_none) {
            let retried = self.request_batch(lines).await?;
            aligned = parse_numbered(&retried, lines.len());
        }

        for index in 0..aligned.len() {
            if aligned[index].is_some() {
                continue;
            }
            let response = self
                .request_batch(std::slice::from_ref(&lines[index]))
                .await
                .with_context(|| format!("failed to translate missing line {}", index + 1))?;
            aligned[index] = parse_numbered(&response, 1).into_iter().next().flatten();
            if aligned[index].is_none() {
                bail!(
                    "translation response did not contain required marker <1> for line {}",
                    index + 1
                );
            }
        }

        aligned
            .into_iter()
            .enumerate()
            .map(|(index, value)| {
                value.ok_or_else(|| anyhow!("translation missing for line {}", index + 1))
            })
            .collect()
    }

    /// Stream translations. The callback receives the zero-based source line
    /// index and is called only when a complete `<N>` section is available.
    pub async fn translate_stream<F>(
        &self,
        lines: &[String],
        cancellation: &CancellationToken,
        mut on_line: F,
    ) -> Result<Vec<String>>
    where
        F: FnMut(usize, String),
    {
        if lines.is_empty() {
            return Ok(Vec::new());
        }

        let mut aligned = vec![None; lines.len()];
        self.stream_into(lines, cancellation, &mut aligned, &mut on_line)
            .await?;

        if aligned.iter().any(Option::is_none) {
            self.stream_into(lines, cancellation, &mut aligned, &mut on_line)
                .await?;
        }

        for index in 0..aligned.len() {
            if aligned[index].is_some() {
                continue;
            }
            self.request_stream(
                std::slice::from_ref(&lines[index]),
                cancellation,
                |_, text| {
                    if aligned[index].is_none() {
                        aligned[index] = Some(text.clone());
                        on_line(index, text);
                    }
                },
            )
            .await
            .with_context(|| format!("failed to translate missing line {}", index + 1))?;
            if aligned[index].is_none() {
                bail!(
                    "translation response did not contain required marker <1> for line {}",
                    index + 1
                );
            }
        }

        aligned
            .into_iter()
            .enumerate()
            .map(|(index, value)| {
                value.ok_or_else(|| anyhow!("translation missing for line {}", index + 1))
            })
            .collect()
    }

    async fn stream_into<F>(
        &self,
        lines: &[String],
        cancellation: &CancellationToken,
        aligned: &mut [Option<String>],
        on_line: &mut F,
    ) -> Result<()>
    where
        F: FnMut(usize, String),
    {
        self.request_stream(lines, cancellation, |index, value| {
            if aligned[index].is_none() {
                aligned[index] = Some(value.clone());
                on_line(index, value);
            }
        })
        .await
    }

    async fn request_stream<F>(
        &self,
        lines: &[String],
        cancellation: &CancellationToken,
        mut on_line: F,
    ) -> Result<()>
    where
        F: FnMut(usize, String),
    {
        let request = self.make_request(lines, true);
        let send = self
            .client
            .post(self.endpoint.clone())
            .bearer_auth(&self.api_key)
            .json(&request)
            .send();
        let response = tokio::select! {
            _ = cancellation.cancelled() => bail!("translation cancelled"),
            response = send => response.context("streaming chat completion request failed")?,
        };
        let status = response.status();
        if !status.is_success() {
            let body = response
                .text()
                .await
                .context("failed to read streaming error response")?;
            bail!(
                "streaming chat completion returned HTTP {status}: {}",
                body.trim()
            );
        }

        let mut events = response.bytes_stream().eventsource();
        let mut parser = NumberedStreamParser::new(lines.len());
        loop {
            let event = tokio::select! {
                _ = cancellation.cancelled() => bail!("translation cancelled"),
                event = events.next() => event,
            };
            let Some(event) = event else {
                break;
            };
            let event = event.context("invalid server-sent event stream")?;
            match parse_sse_data(&event.data)? {
                SseData::Done => break,
                SseData::Content(content) => {
                    for (index, text) in parser.push(&content) {
                        on_line(index, text);
                    }
                }
                SseData::Empty => {}
            }
        }
        for (index, text) in parser.finish() {
            on_line(index, text);
        }
        Ok(())
    }

    /// 连接预热：向站点根路径发一个短请求，让连接池里的 TCP+TLS 连接提前
    /// 就绪（响应内容无关，读完响应体连接才归还连接池）。在后台线程调用。
    pub fn prewarm(&self) {
        let client = self.client.clone();
        let mut url = self.endpoint.clone();
        url.set_path("/");
        let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        else {
            return;
        };
        let _ = runtime.block_on(async move {
            let response = client
                .get(url)
                .timeout(std::time::Duration::from_secs(5))
                .send()
                .await?;
            response.bytes().await
        });
    }

    async fn request_batch(&self, lines: &[String]) -> Result<String> {
        let request = self.make_request(lines, false);

        let response = self
            .client
            .post(self.endpoint.clone())
            .bearer_auth(&self.api_key)
            .json(&request)
            .send()
            .await
            .context("chat completion request failed")?;
        let status = response.status();
        let body = response
            .text()
            .await
            .context("failed to read chat completion response")?;
        if !status.is_success() {
            bail!("chat completion returned HTTP {status}: {body}");
        }

        let response: ChatResponse =
            serde_json::from_str(&body).context("invalid chat completion response JSON")?;
        response
            .choices
            .into_iter()
            .next()
            .and_then(|choice| choice.message.content)
            .filter(|content| !content.trim().is_empty())
            .ok_or_else(|| anyhow!("chat completion response has no textual content"))
    }

    fn make_request<'a>(&'a self, lines: &[String], stream: bool) -> ChatRequest<'a> {
        ChatRequest {
            model: &self.model,
            stream,
            thinking: supports_thinking_toggle(&self.endpoint)
                .then_some(Thinking { r#type: "disabled" }),
            messages: vec![
                Message {
                    role: "system",
                    content: format!(
                        "Translate every numbered item into {}. Return every translation using \
                         exactly the same <N> marker. Preserve order and do not include analysis, \
                         explanations, <think> blocks, or unnumbered text. Keep only proper nouns \
                         and literal code untranslated: product and brand names, library, \
                         framework, and programming-language names, acronyms (e.g. API, SDK, GPU), \
                         code identifiers, file paths, and shell commands. Translate all ordinary \
                         words—including common technical vocabulary such as 'embedded' or \
                         'command'—and translate a standalone word as a dictionary entry.",
                        self.target_lang
                    ),
                },
                Message {
                    role: "user",
                    content: format_numbered(lines),
                },
            ],
        }
    }
}

enum SseData {
    Done,
    Content(String),
    Empty,
}

fn parse_sse_data(data: &str) -> Result<SseData> {
    if data.trim() == "[DONE]" {
        return Ok(SseData::Done);
    }
    let value: StreamResponse =
        serde_json::from_str(data).context("invalid streaming chat completion JSON")?;
    let content = value
        .choices
        .into_iter()
        .filter_map(|choice| choice.delta.content)
        .collect::<String>();
    if content.is_empty() {
        Ok(SseData::Empty)
    } else {
        Ok(SseData::Content(content))
    }
}

/// Turn either an API root or an existing endpoint into `/chat/completions`.
pub fn normalize_chat_completions_url(api_base: &str) -> Result<Url> {
    let mut url = Url::parse(api_base.trim()).context("invalid API base URL")?;
    if !matches!(url.scheme(), "http" | "https") {
        bail!("API base URL must use http or https");
    }
    url.set_query(None);
    url.set_fragment(None);

    let path = url.path().trim_end_matches('/');
    let normalized = if path.ends_with("/chat/completions") {
        path.to_owned()
    } else {
        format!("{path}/chat/completions")
    };
    url.set_path(&normalized);
    Ok(url)
}

#[derive(Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    stream: bool,
    messages: Vec<Message>,
    /// 关闭思考模式（DeepSeek V4 / GLM 4.5+ / Kimi K2 均接受该结构）。
    /// 翻译任务不需要推理，think 阶段纯粹是首 token 延迟。
    #[serde(skip_serializing_if = "Option::is_none")]
    thinking: Option<Thinking>,
}

#[derive(Serialize)]
struct Thinking {
    r#type: &'static str,
}

/// 已知接受 `thinking: {type: "disabled"}` 的网关/官方 API：OpenCode Zen/Go
/// （转发给上游）、DeepSeek 官方、智谱官方。其余供应商不发送，避免严格
/// 服务端因未知字段 400。
fn supports_thinking_toggle(endpoint: &Url) -> bool {
    matches!(
        endpoint.host_str(),
        Some("opencode.ai") | Some("api.deepseek.com") | Some("open.bigmodel.cn")
    )
}

#[derive(Serialize)]
struct Message {
    role: &'static str,
    content: String,
}

#[derive(Deserialize)]
struct ChatResponse {
    #[serde(default)]
    choices: Vec<Choice>,
}

#[derive(Deserialize)]
struct Choice {
    message: ResponseMessage,
}

#[derive(Deserialize)]
struct ResponseMessage {
    content: Option<String>,
    // Intentionally ignored: reasoning must never leak into translated text.
    #[serde(default, rename = "reasoning_content")]
    _reasoning_content: Option<serde_json::Value>,
}

#[derive(Deserialize)]
struct StreamResponse {
    #[serde(default)]
    choices: Vec<StreamChoice>,
}

#[derive(Deserialize)]
struct StreamChoice {
    delta: StreamDelta,
}

#[derive(Deserialize)]
struct StreamDelta {
    #[serde(default)]
    content: Option<String>,
    // DeepSeek and Qwen may stream reasoning separately. It is deliberately ignored.
    #[serde(default, rename = "reasoning_content")]
    _reasoning_content: Option<serde_json::Value>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_content_and_ignores_reasoning_delta() {
        let data = r#"{"choices":[{"delta":{"reasoning_content":"secret","content":"<1> ok"}}]}"#;
        match parse_sse_data(data).unwrap() {
            SseData::Content(content) => assert_eq!(content, "<1> ok"),
            _ => panic!("expected content"),
        }
    }

    #[test]
    fn recognizes_done_and_reasoning_only_events() {
        assert!(matches!(parse_sse_data("[DONE]").unwrap(), SseData::Done));
        let reasoning = r#"{"choices":[{"delta":{"reasoning_content":"secret"}}]}"#;
        assert!(matches!(parse_sse_data(reasoning).unwrap(), SseData::Empty));
    }

    #[test]
    fn thinking_disabled_for_known_gateways() {
        for base in [
            "https://opencode.ai/zen/v1",
            "https://api.deepseek.com/v1",
            "https://open.bigmodel.cn/v4",
        ] {
            let translator =
                OpenAiTranslator::new(base, "sk-x", "m", "简体中文", None).expect("translator");
            let request = translator.make_request(&["hello".to_owned()], false);
            let json = serde_json::to_value(&request).expect("serialize");
            assert_eq!(json["thinking"]["type"], "disabled", "{base} 应关思考");
        }
    }

    #[test]
    fn thinking_not_sent_for_other_hosts() {
        let translator =
            OpenAiTranslator::new("https://api.openai.com/v1", "sk-x", "gpt", "简体中文", None)
                .expect("translator");
        let request = translator.make_request(&["hello".to_owned()], false);
        let json = serde_json::to_value(&request).expect("serialize");
        assert!(
            json.get("thinking").is_none(),
            "未知供应商不得发送 thinking"
        );
    }
}
