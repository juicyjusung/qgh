use std::error::Error;
use std::fmt;

pub type EmbeddingVector = Vec<f32>;

pub trait EmbeddingProvider {
    fn embed_documents(
        &self,
        texts: &[&str],
    ) -> Result<Vec<EmbeddingVector>, EmbeddingProviderError>;
    fn embed_query(&self, text: &str) -> Result<EmbeddingVector, EmbeddingProviderError>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmbeddingProviderError {
    message: String,
}

impl EmbeddingProviderError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for EmbeddingProviderError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for EmbeddingProviderError {}
