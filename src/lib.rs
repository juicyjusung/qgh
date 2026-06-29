mod app;
mod cli;
mod commands;
mod config;
mod error;
mod github;
mod index;
mod mcp;
mod model;
mod output;
mod paths;
mod resolution;
mod store;
mod time;

pub async fn run() -> i32 {
    app::run_from_env().await
}
