//! Ollama translation client supporting native JSONL and OpenAI-compatible SSE.

use anyhow::{Context, Result, anyhow, bail};
use futures::StreamExt;
use reqwest::{Client, Proxy, Url};
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use super::{
    openai::OpenAiTranslator,
    parser::{NumberedStreamParser, format_numbered, parse_numbered},
};

#[derive(Clone)]
pub struct OllamaTranslator {
    backend: Backend,
}

#[derive(Clone)]
enum Backend {
    Native {
        client: Client,
        endpoint: Url,
        model: String,
        target_lang: String,
    },
    #[allow(dead_code)]
    OpenAi(OpenAiTranslator),
}

impl OllamaTranslator {
    /// Create a translator for Ollama's native `/api/chat` endpoint.
    pub fn new(
        api_base: impl AsRef<str>,
        model: impl Into<String>,
        target_lang: impl Into<String>,
        proxy: Option<&str>,
    ) -> Result<Self> {
        let model = model.into();
        if model.trim().is_empty() {
            bail!("Ollama model is empty");
        }
        let mut builder = Client::builder().timeout(std::time::Duration::from_secs(120));
        if let Some(proxy) = proxy.filter(|value| !value.trim().is_empty()) {
            builder = builder
                .proxy(Proxy::all(proxy).with_context(|| format!("invalid proxy URL: {proxy}"))?);
        }
        Ok(Self {
            backend: Backend::Native {
                client: builder
                    .build()
                    .context("failed to build Ollama HTTP client")?,
                endpoint: normalize_ollama_chat_url(api_base.as_ref())?,
                model,
                target_lang: target_lang.into(),
            },
        })
    }

    /// Create a translator for an Ollama OpenAI-compatible `/chat/completions`
    /// endpoint. Ollama accepts any non-empty API key by default.
    #[allow(dead_code)]
    pub fn new_openai_compatible(
        api_base: impl AsRef<str>,
        api_key: impl Into<String>,
        model: impl Into<String>,
        target_lang: impl Into<String>,
        proxy: Option<&str>,
    ) -> Result<Self> {
        Ok(Self {
            backend: Backend::OpenAi(OpenAiTranslator::new(
                api_base,
                api_key,
                model,
                target_lang,
                proxy,
            )?),
        })
    }

    pub async fn translate(&self, lines: &[String]) -> Result<Vec<String>> {
        match &self.backend {
            Backend::OpenAi(translator) => translator.translate(lines).await,
            Backend::Native { .. } => self.translate_native(lines).await,
        }
    }

    /// Same callback contract as `OpenAiTranslator::translate_stream`.
    pub async fn translate_stream<F>(
        &self,
        lines: &[String],
        cancellation: &CancellationToken,
        mut on_line: F,
    ) -> Result<Vec<String>>
    where
        F: FnMut(usize, String),
    {
        match &self.backend {
            Backend::OpenAi(translator) => {
                translator
                    .translate_stream(lines, cancellation, on_line)
                    .await
            }
            Backend::Native { .. } => {
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
                    self.request_native_stream(
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
                    .with_context(|| {
                        format!("failed to translate missing Ollama line {}", index + 1)
                    })?;
                    if aligned[index].is_none() {
                        bail!(
                            "Ollama response did not contain required marker <1> for line {}",
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
        }
    }

    async fn translate_native(&self, lines: &[String]) -> Result<Vec<String>> {
        if lines.is_empty() {
            return Ok(Vec::new());
        }
        let first = self.request_native_batch(lines).await?;
        let mut aligned = parse_numbered(&first, lines.len());
        if aligned.iter().any(Option::is_none) {
            aligned = parse_numbered(&self.request_native_batch(lines).await?, lines.len());
        }
        for index in 0..aligned.len() {
            if aligned[index].is_some() {
                continue;
            }
            let response = self
                .request_native_batch(std::slice::from_ref(&lines[index]))
                .await
                .with_context(|| {
                    format!("failed to translate missing Ollama line {}", index + 1)
                })?;
            aligned[index] = parse_numbered(&response, 1).into_iter().next().flatten();
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
        self.request_native_stream(lines, cancellation, |index, text| {
            if aligned[index].is_none() {
                aligned[index] = Some(text.clone());
                on_line(index + 1, text);
            }
        })
        .await
    }

    async fn request_native_batch(&self, lines: &[String]) -> Result<String> {
        let (client, endpoint) = self.native_client()?;
        let response = client
            .post(endpoint)
            .json(&self.native_request(lines, false)?)
            .send()
            .await
            .context("Ollama chat request failed")?;
        let status = response.status();
        let body = response
            .text()
            .await
            .context("failed to read Ollama response")?;
        if !status.is_success() {
            bail!("Ollama returned HTTP {status}: {}", body.trim());
        }
        let response: OllamaChunk =
            serde_json::from_str(&body).context("invalid Ollama response JSON")?;
        response
            .message
            .and_then(|message| message.content)
            .filter(|content| !content.trim().is_empty())
            .ok_or_else(|| anyhow!("Ollama response has no textual content"))
    }

    async fn request_native_stream<F>(
        &self,
        lines: &[String],
        cancellation: &CancellationToken,
        mut on_line: F,
    ) -> Result<()>
    where
        F: FnMut(usize, String),
    {
        let (client, endpoint) = self.native_client()?;
        let send = client
            .post(endpoint)
            .json(&self.native_request(lines, true)?)
            .send();
        let response = tokio::select! {
            _ = cancellation.cancelled() => bail!("translation cancelled"),
            response = send => response.context("streaming Ollama request failed")?,
        };
        let status = response.status();
        if !status.is_success() {
            let body = response
                .text()
                .await
                .context("failed to read Ollama error response")?;
            bail!("streaming Ollama returned HTTP {status}: {}", body.trim());
        }

        let mut stream = response.bytes_stream();
        let mut jsonl = Vec::new();
        let mut parser = NumberedStreamParser::new(lines.len());
        let mut done = false;
        while !done {
            let chunk = tokio::select! {
                _ = cancellation.cancelled() => bail!("translation cancelled"),
                chunk = stream.next() => chunk,
            };
            let Some(chunk) = chunk else {
                break;
            };
            let chunk = chunk.context("failed to read Ollama JSONL stream")?;
            jsonl.extend_from_slice(&chunk);
            while let Some(newline) = jsonl.iter().position(|byte| *byte == b'\n') {
                let line = std::str::from_utf8(&jsonl[..newline])
                    .context("Ollama JSONL line was not valid UTF-8")?
                    .trim()
                    .to_owned();
                jsonl.drain(..=newline);
                if !line.is_empty() {
                    done = consume_jsonl(&line, &mut parser, &mut on_line)?;
                }
            }
        }
        if !jsonl.is_empty() && !done {
            let line = std::str::from_utf8(&jsonl)
                .context("Ollama JSONL tail was not valid UTF-8")?
                .trim();
            if !line.is_empty() {
                consume_jsonl(line, &mut parser, &mut on_line)?;
            }
        }
        for (index, text) in parser.finish() {
            on_line(index, text);
        }
        Ok(())
    }

    fn native_client(&self) -> Result<(&Client, Url)> {
        match &self.backend {
            Backend::Native {
                client, endpoint, ..
            } => Ok((client, endpoint.clone())),
            Backend::OpenAi(_) => bail!("not an Ollama native endpoint"),
        }
    }

    fn native_request(&self, lines: &[String], stream: bool) -> Result<OllamaRequest<'_>> {
        let Backend::Native {
            model, target_lang, ..
        } = &self.backend
        else {
            bail!("not an Ollama native endpoint");
        };
        Ok(OllamaRequest {
            model,
            stream,
            messages: vec![
                OllamaMessage {
                    role: "system",
                    content: format!(
                        "Translate every numbered item into {target_lang}. Return exactly the same \
                         <N> markers, in order, without analysis or <think> blocks."
                    ),
                },
                OllamaMessage {
                    role: "user",
                    content: format_numbered(lines),
                },
            ],
        })
    }
}

fn consume_jsonl<F>(line: &str, parser: &mut NumberedStreamParser, on_line: &mut F) -> Result<bool>
where
    F: FnMut(usize, String),
{
    let chunk: OllamaChunk = serde_json::from_str(line).context("invalid Ollama JSONL chunk")?;
    if let Some(content) = chunk.message.and_then(|message| message.content) {
        for (index, text) in parser.push(&content) {
            on_line(index, text);
        }
    }
    Ok(chunk.done)
}

pub fn normalize_ollama_chat_url(api_base: &str) -> Result<Url> {
    let mut url = Url::parse(api_base.trim()).context("invalid Ollama base URL")?;
    if !matches!(url.scheme(), "http" | "https") {
        bail!("Ollama base URL must use http or https");
    }
    url.set_query(None);
    url.set_fragment(None);
    let path = url.path().trim_end_matches('/');
    let normalized = if path.ends_with("/api/chat") {
        path.to_owned()
    } else {
        format!("{path}/api/chat")
    };
    url.set_path(&normalized);
    Ok(url)
}

#[derive(Serialize)]
struct OllamaRequest<'a> {
    model: &'a str,
    stream: bool,
    messages: Vec<OllamaMessage>,
}

#[derive(Serialize)]
struct OllamaMessage {
    role: &'static str,
    content: String,
}

#[derive(Deserialize)]
struct OllamaChunk {
    #[serde(default)]
    message: Option<OllamaResponseMessage>,
    #[serde(default)]
    done: bool,
}

#[derive(Deserialize)]
struct OllamaResponseMessage {
    #[serde(default)]
    content: Option<String>,
    // Newer Ollama models may expose thinking separately.
    #[serde(default)]
    _thinking: Option<serde_json::Value>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_fragment_friendly_jsonl_chunks_and_done() {
        let mut parser = NumberedStreamParser::new(2);
        let mut lines = Vec::new();
        assert!(
            !consume_jsonl(
                r#"{"message":{"content":"<1> one\n<"},"done":false}"#,
                &mut parser,
                &mut |index, text| lines.push((index, text)),
            )
            .unwrap()
        );
        assert!(
            consume_jsonl(
                r#"{"message":{"content":"2> two"},"done":true}"#,
                &mut parser,
                &mut |index, text| lines.push((index, text)),
            )
            .unwrap()
        );
        lines.extend(parser.finish());
        assert_eq!(lines, vec![(0, "one".into()), (1, "two".into())]);
    }

    #[test]
    fn normalizes_native_endpoint() {
        assert_eq!(
            normalize_ollama_chat_url("http://localhost:11434")
                .unwrap()
                .as_str(),
            "http://localhost:11434/api/chat"
        );
    }
}
