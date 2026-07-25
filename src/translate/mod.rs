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
                Box::pin(translate_stream_chain(chain, lines, cancellation, &mut on_line)).await
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
            tracing::info!(label, remaining = missing.len(), "falling back to next translator");
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
            .translate_stream(
                &["hello".to_owned()],
                &CancellationToken::new(),
                |_, _| {},
            )
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
}
