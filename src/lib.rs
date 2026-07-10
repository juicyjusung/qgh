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
mod mcp;
mod model;
mod output;
mod paths;
mod resolution;
mod store;
mod time;

/// Internal release/live-qrels adapters. Not a CLI, MCP, or config surface.
#[doc(hidden)]
pub mod search_eval {
    pub use crate::index::{
        search_with_lexical_profile_for_eval, search_with_metadata_boost_v1_for_eval,
        EvalLexicalProfile, SearchFilters, SearchHit,
    };
}

pub async fn run() -> i32 {
    app::run_from_env().await
}
