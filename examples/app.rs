//! A complete rastack app: tracing + app identity + an axum HTTP server.
//!
//! Run with:
//!
//! ```sh
//! cargo run --example app
//! APP_NAME=my-service PORT=3000 LOG_FORMAT=json cargo run --example app
//! ```
//!
//! Then: `curl http://localhost:8080/health`
//!
//! Every rastack app follows the same strict order:
//!
//! 1. create the stack — the core providers (tracing, app) are embedded:
//!    logging follows `LOG_FORMAT` / `LOG_LEVEL` from the first line, and
//!    the app identity (`APP_NAME`, or the executable name) is available
//!    via `stack.app()`
//! 2. per optional provider, in dependency order: create it, add it
//! 3. use provider handles (deliver routes, inject into dependents)
//! 4. run the stack
//!
//! The axum provider is server-style: its `run` stays open, so the stack
//! keeps serving until an exit signal (Ctrl-C, SIGTERM, ...) arrives, then
//! tears down in reverse order — axum drains its connections first, the
//! core providers go down last.

use axum::Router;
use axum::routing::get;
use rastack::{AxumConfig, AxumProvider, LoadConfig, Stack, StackError};

#[tokio::main]
async fn main() -> Result<(), StackError> {
    // 1. create the stack (embeds tracing + app core providers)
    let mut stack = Stack::new();

    // 2. create + add optional providers, dependencies first
    //    configs load in layers: defaults < ENV < config file < cli args
    let axum_provider = AxumProvider::new(AxumConfig::load()?);
    stack.add(axum_provider.clone()).await?;

    // 3. use providers
    axum_provider.add_routes(Router::new().route("/health", get(|| async { "ok" })));

    // 4. run the stack
    stack.run().await
}
