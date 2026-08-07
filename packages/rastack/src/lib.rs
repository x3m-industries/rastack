//! rastack — a provider-based application stack for Rust services.
//!
//! A [`Stack`] owns a set of [`Provider`]s (an axum server, a database
//! client, ...), brings them up in order, keeps them running, and tears
//! them down in reverse order on exit signals, completion or failure.
//! Provider configs load in layers ([`LoadConfig`]): defaults ← ENV ←
//! config file ← cli arguments.
//!
//! See `docs/STACK.md` and `docs/PROVIDER.md` in the repository for the
//! full story, and `examples/app.rs` for a complete app.

mod config;
mod providers;
mod stack;

pub use config::LoadConfig;
pub use providers::app::{AppConfig, AppProvider};
pub use providers::axum::{AxumConfig, AxumProvider};
pub use providers::tracing::{TracingConfig, TracingProvider};
pub use stack::lifecycle::{Stack, StackError, StackState};
pub use stack::provider::{BoxError, Config, ConfigError, Provider, ProviderInfo};
