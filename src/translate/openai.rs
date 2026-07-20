//! Client for synchronous and streaming OpenAI-compatible chat completion APIs.

use anyhow::{Context, Result, anyhow, bail};
use eventsource_stream::Eventsource;
use futures::StreamExt;
use reqwest::{Client, Proxy, Url};
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::config::AppConfig;

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

    /// Construct a translator and decrypt the API key held by application config.
    pub fn from_config(config: &AppConfig) -> Result<Self> {
        let api_key = config
            .api_key()?
            .ok_or_else(|| anyhow!("API key has not been configured"))?;
        Self::new(
            &config.api_base,
            api_key,
            &config.model,
            &config.target_lang,
            config.proxy.as_deref(),
        )
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

    /// Stream translations. The callback receives the one-based source line
    /// number and is called only when a complete `<N>` section is available.
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
                        on_line(index + 1, text);
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
                on_line(index + 1, value);
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
            messages: vec![
                Message {
                    role: "system",
                    content: format!(
                        "Translate every numbered item into {}. Return every translation using \
                         exactly the same <N> marker. Preserve order and do not include analysis, \
                         explanations, <think> blocks, or unnumbered text.",
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
}
