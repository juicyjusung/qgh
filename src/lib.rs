mod app;
pub mod chunking;
mod cli;
mod commands;
mod config;
#[doc(hidden)]
pub mod context;
mod coverage;
pub mod embedding;
mod error;
mod freshness;
mod github;
mod index;
mod local_models;
mod mcp;
mod model;
mod output;
mod paths;
#[cfg(feature = "fastembed-provider")]
mod qwen;
mod resolution;
mod store;
mod time;

/// Internal release/live-qrels adapters. Not a CLI, MCP, or config surface.
#[doc(hidden)]
pub mod search_eval {
    #[cfg(feature = "fastembed-provider")]
    pub use crate::config::LocalModelDevice;
    pub use crate::index::{
        production_lexical_profile_for_eval, rebuild as rebuild_lexical_index_for_eval,
        search_with_lexical_profile_for_eval, search_with_metadata_boost_v1_for_eval,
        EvalLexicalProfile, SearchFilters, SearchHit,
    };
    #[cfg(feature = "fastembed-provider")]
    pub use crate::local_models::{
        qwen_model_spec, PreparedQwenModelStore, QWEN_EMBEDDING_PRESET_ID, QWEN_RERANKER_PRESET_ID,
    };
    pub use crate::model::IndexSource as EvalIndexSource;
    #[cfg(feature = "fastembed-provider")]
    pub use crate::qwen::{
        load_qwen_embedding, load_qwen_reranker, QwenEmbeddingParts, QwenReranker,
        QWEN_EMBEDDING_OUTPUT_DIMENSION, QWEN_RERANK_DEPTH, QWEN_RERANK_MAX_LENGTH,
    };
}

pub async fn run() -> i32 {
    app::run_from_env().await
}
