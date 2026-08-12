//! Translation clients and numbered-response parsing.

pub mod ollama;
pub mod openai;
pub mod parser;
pub mod vision;

use anyhow::{Result, anyhow, bail};
use tokio_util::sync::CancellationToken;

pub use ollama::OllamaTranslator;
pub use openai::OpenAiTranslator;

/// A common callable facade for cloud-compatible and local Ollama engines.
#[derive(Clone)]
pub enum Translator {
    OpenAi(OpenAiTranslator),
    Ollama(OllamaTranslator),
    /// 失败回退链：`(标签, 客户端)`，按顺序尝试；标签用于错误信息定位。
    Chain(Vec<(String, Translator)>),
}

impl Translator {
    /// 预热首个云端节点的连接（本地 Ollama 无需预热）。
    pub fn prewarm(&self) {
        match self {
            Self::OpenAi(translator) => translator.prewarm(),
            Self::Ollama(_) => {}
            Self::Chain(chain) => {
                if let Some((_, first)) = chain.first() {
                    first.prewarm();
                }
            }
        }
    }

    pub async fn translate(&self, lines: &[String]) -> Result<Vec<String>> {
        match self {
            Self::OpenAi(translator) => translator.translate(lines).await,
            Self::Ollama(translator) => translator.translate(lines).await,
            Self::Chain(chain) => Box::pin(translate_chain(chain, lines)).await,
        }
    }

    /// 流式翻译。`on_line` 回调收到的是 0-based 行索引（与 `lines` 下标对齐），
    /// 每个实现（OpenAI / Ollama / Chain）都必须遵守同一约定。
    pub async fn translate_stream<F>(
        &self,
        lines: &[String],
        cancellation: &CancellationToken,
        on_line: F,
    ) -> Result<Vec<String>>
    where
        F: FnMut(usize, String),
    {
        match self {
            Self::OpenAi(translator) => {
                translator
                    .translate_stream(lines, cancellation, on_line)
                    .await
            }
            Self::Ollama(translator) => {
                translator
                    .translate_stream(lines, cancellation, on_line)
                    .await
            }
            Self::Chain(chain) => {
                let mut on_line = on_line;
                Box::pin(translate_stream_chain(
                    chain,
                    lines,
                    cancellation,
                    &mut on_line,
                ))
                .await
            }
        }
    }
}

async fn translate_chain(chain: &[(String, Translator)], lines: &[String]) -> Result<Vec<String>> {
    let mut failures = Vec::new();
    for (label, translator) in chain {
        match translator.translate(lines).await {
            Ok(translations) => return Ok(translations),
            Err(error) => {
                tracing::warn!(label, error = %format!("{error:#}"), "translator failed, falling back");
                failures.push(format!("{label}: {error:#}"));
            }
        }
    }
    Err(anyhow!("全部供应商均失败：{}", failures.join("；")))
}

/// 流式回退：首个供应商流式输出；失败后仅对缺失的行切换下一个供应商，
/// 已成功流出的行不重复请求。取消信号立即终止，不回退。
///
/// 回调类型擦除为 `dyn FnMut`：否则链式调用每层都会生成新的闭包类型，
/// 单态化永不收敛。
async fn translate_stream_chain(
    chain: &[(String, Translator)],
    lines: &[String],
    cancellation: &CancellationToken,
    on_line: &mut dyn FnMut(usize, String),
) -> Result<Vec<String>> {
    let mut results: Vec<Option<String>> = vec![None; lines.len()];
    let mut failures = Vec::new();
    for (label, translator) in chain {
        if cancellation.is_cancelled() {
            bail!("任务已取消");
        }
        let missing: Vec<usize> = (0..lines.len()).filter(|i| results[*i].is_none()).collect();
        if missing.is_empty() {
            break;
        }
        if !failures.is_empty() {
            tracing::info!(
                label,
                remaining = missing.len(),
                "falling back to next translator"
            );
        }
        let sub_lines: Vec<String> = missing.iter().map(|&i| lines[i].clone()).collect();
        let missing_ref = &missing;
        let results_ref = &mut results;
        let attempt = translator
            .translate_stream(&sub_lines, cancellation, |sub_index, text| {
                let Some(&global_index) = missing_ref.get(sub_index) else {
                    return;
                };
                results_ref[global_index] = Some(text.clone());
                on_line(global_index, text);
            })
            .await;
        match attempt {
            Ok(returned) => {
                // 回调未覆盖的行（理论上成功时无缺失）从返回值补齐。
                for (sub_index, text) in returned.into_iter().enumerate() {
                    let Some(&global_index) = missing.get(sub_index) else {
                        continue;
                    };
                    if results[global_index].is_none() {
                        results[global_index] = Some(text.clone());
                        on_line(global_index, text);
                    }
                }
            }
            Err(error) => {
                if cancellation.is_cancelled() {
                    bail!("任务已取消");
                }
                tracing::warn!(label, error = %format!("{error:#}"), "translator failed, falling back");
                failures.push(format!("{label}: {error:#}"));
            }
        }
    }
    if results.iter().all(Option::is_some) {
        return Ok(results
            .into_iter()
            .map(|line| line.expect("all lines filled"))
            .collect());
    }
    if failures.is_empty() {
        bail!("翻译结果不完整");
    }
    Err(anyhow!("全部供应商均失败：{}", failures.join("；")))
}

impl From<OpenAiTranslator> for Translator {
    fn from(value: OpenAiTranslator) -> Self {
        Self::OpenAi(value)
    }
}

impl From<OllamaTranslator> for Translator {
    fn from(value: OllamaTranslator) -> Self {
        Self::Ollama(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    fn failing_translator(port: u16) -> Translator {
        OpenAiTranslator::new(
            format!("http://127.0.0.1:{port}"),
            "sk-test",
            "test-model",
            "English",
            None,
        )
        .expect("translator should build")
        .into()
    }

    #[tokio::test]
    async fn chain_tries_each_member_and_aggregates_errors() {
        let chain = Translator::Chain(vec![
            ("节点A".into(), failing_translator(1)),
            ("节点B".into(), failing_translator(2)),
        ]);
        let error = chain
            .translate(&["hello".to_owned()])
            .await
            .expect_err("all members fail");
        let message = format!("{error:#}");
        assert!(message.contains("节点A"), "missing member A: {message}");
        assert!(message.contains("节点B"), "missing member B: {message}");
    }

    #[tokio::test]
    async fn chain_stream_falls_back_and_reports_labels() {
        let chain = Translator::Chain(vec![
            ("节点A".into(), failing_translator(1)),
            ("节点B".into(), failing_translator(2)),
        ]);
        let error = chain
            .translate_stream(&["hello".to_owned()], &CancellationToken::new(), |_, _| {})
            .await
            .expect_err("all members fail");
        let message = format!("{error:#}");
        assert!(message.contains("节点A"), "missing member A: {message}");
        assert!(message.contains("节点B"), "missing member B: {message}");
    }

    #[tokio::test]
    async fn chain_stream_stops_on_cancellation_without_fallback() {
        let cancellation = CancellationToken::new();
        cancellation.cancel();
        let chain = Translator::Chain(vec![("节点A".into(), failing_translator(1))]);
        let error = chain
            .translate_stream(&["hello".to_owned()], &cancellation, |_, _| {})
            .await
            .expect_err("cancelled");
        assert_eq!(format!("{error:#}"), "任务已取消");
    }

    /// 最小 SSE mock：读完一个请求后回放固定事件流，循环接受后续连接。
    fn spawn_sse_server(body: &'static str) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock server");
        let port = listener.local_addr().expect("local addr").port();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let mut stream = stream.expect("accept");
                let mut buffer = Vec::new();
                let mut chunk = [0u8; 4096];
                let mut body_range = None;
                loop {
                    if let Some((start, len)) = body_range
                        && buffer.len() >= start + len
                    {
                        break;
                    }
                    let read = stream.read(&mut chunk).expect("read request");
                    if read == 0 {
                        break;
                    }
                    buffer.extend_from_slice(&chunk[..read]);
                    if body_range.is_none()
                        && let Some(pos) =
                            buffer.windows(4).position(|window| window == b"\r\n\r\n")
                    {
                        let headers = String::from_utf8_lossy(&buffer[..pos]).to_lowercase();
                        let len = headers
                            .lines()
                            .find_map(|line| line.strip_prefix("content-length:"))
                            .and_then(|value| value.trim().parse::<usize>().ok())
                            .unwrap_or(0);
                        body_range = Some((pos + 4, len));
                    }
                }
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                stream
                    .write_all(response.as_bytes())
                    .expect("write response");
            }
        });
        port
    }

    /// 回归：流式回调必须是 0-based 行索引。1-based 时第一行译文会落到
    /// 第二行槽位（界面上首行重复盖住次行），末行译文越界被丢弃。
    #[tokio::test]
    async fn chain_stream_reports_zero_based_indexes_and_aligned_results() {
        let body = "data: {\"choices\":[{\"delta\":{\"content\":\"<1> 一\\n<2> 二\\n\"}}]}\n\ndata: [DONE]\n\n";
        let port = spawn_sse_server(body);
        let chain = Translator::Chain(vec![(
            "mock".into(),
            OpenAiTranslator::new(
                format!("http://127.0.0.1:{port}"),
                "sk-test",
                "test-model",
                "English",
                None,
            )
            .expect("translator should build")
            .into(),
        )]);
        let lines = vec!["one".to_owned(), "two".to_owned()];
        let mut events = Vec::new();
        let translated = chain
            .translate_stream(&lines, &CancellationToken::new(), |index, text| {
                events.push((index, text));
            })
            .await
            .expect("stream should succeed");
        assert_eq!(
            events,
            vec![(0, "一".to_owned()), (1, "二".to_owned())],
            "回调索引必须 0-based 且按行序到达"
        );
        assert_eq!(translated, vec!["一".to_owned(), "二".to_owned()]);
    }
}
