//! Translation clients and numbered-response parsing.

pub mod ollama;
pub mod openai;
pub mod parser;
pub mod vision;

use anyhow::Result;
use tokio_util::sync::CancellationToken;

pub use ollama::OllamaTranslator;
pub use openai::OpenAiTranslator;

/// A common callable facade for cloud-compatible and local Ollama engines.
#[derive(Clone)]
pub enum Translator {
    OpenAi(OpenAiTranslator),
    Ollama(OllamaTranslator),
}

impl Translator {
    pub async fn translate(&self, lines: &[String]) -> Result<Vec<String>> {
        match self {
            Self::OpenAi(translator) => translator.translate(lines).await,
            Self::Ollama(translator) => translator.translate(lines).await,
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
        }
    }
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
