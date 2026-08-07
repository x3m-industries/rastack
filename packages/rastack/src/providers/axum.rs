use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::Router;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::net::TcpListener;
use tokio::sync::Notify;

use crate::{BoxError, Config, ConfigError, Provider, ProviderInfo};

//##############################################################################
// AxumConfig
//##############################################################################

/// Config layers (via [`LoadConfig`](crate::LoadConfig)): env `PORT`, file
/// key `port`, cli `--port=...`. Defaults to `8080`.
#[derive(Debug, Serialize, Deserialize)]
pub struct AxumConfig {
    /// TCP port the server binds during `init`. Layers: env `PORT`, file
    /// key `port`, cli `--port=...`; defaults to `8080`. Must not be 0.
    pub port: u16,
}

impl Default for AxumConfig {
    fn default() -> Self {
        Self { port: 8080 }
    }
}

impl Config for AxumConfig {
    fn validate(&self) -> Result<(), ConfigError> {
        if self.port == 0 {
            return Err(ConfigError::InvalidField {
                field: "port",
                reason: "port must not be 0".to_string(),
            });
        }

        Ok(())
    }
}

//##############################################################################
// AxumProvider
//##############################################################################

/// Delivery provider: acquires the port during `init`, serves the router
/// during `run`, and drains connections gracefully during `shutdown`.
///
/// Routes are delivered to it before the stack runs:
///
/// ```ignore
/// let axum_provider = AxumProvider::new(AxumConfig::load());
/// axum_provider.add_routes(Router::new().route("/health", get(health)));
/// stack.add(axum_provider.clone()).await?;
/// ```
pub struct AxumProvider {
    config: AxumConfig,
    router: Mutex<Router>,
    listener: Mutex<Option<TcpListener>>,
    /// Bridge between the `shutdown` hook and the serve loop in `run`:
    /// the hook fires it, the serve loop stops accepting and drains.
    stop_signal: Arc<Notify>,
}

impl AxumProvider {
    /// Creates the provider around a loaded [`AxumConfig`], with an empty
    /// router — deliver routes via [`set_router`](AxumProvider::set_router)
    /// or [`add_routes`](AxumProvider::add_routes) before the stack runs.
    #[must_use]
    pub fn new(config: AxumConfig) -> Arc<Self> {
        Arc::new(Self {
            config,
            router: Mutex::new(Router::new()),
            listener: Mutex::new(None),
            stop_signal: Arc::new(Notify::new()),
        })
    }

    /// Replaces the router wholesale.
    ///
    /// # Panics
    ///
    /// Panics when the internal router lock is poisoned, which can only
    /// happen if a previous router update panicked.
    pub fn set_router(&self, router: Router) {
        *self.router.lock().unwrap() = router;
    }

    /// Merges additional routes into the current router.
    ///
    /// # Panics
    ///
    /// Panics when the internal router lock is poisoned, which can only
    /// happen if a previous router update panicked.
    pub fn add_routes(&self, routes: Router) {
        let mut current = self.router.lock().unwrap();
        *current = std::mem::take(&mut *current).merge(routes);
    }
}

#[async_trait]
impl Provider for AxumProvider {
    fn name(&self) -> &'static str {
        "axum"
    }

    fn config(&self) -> Option<&dyn Config> {
        Some(&self.config)
    }

    fn info(&self) -> Option<ProviderInfo> {
        Some(vec![("port".to_string(), json!(self.config.port))])
    }

    async fn init(&self) -> Result<(), BoxError> {
        let listener = TcpListener::bind(("0.0.0.0", self.config.port)).await?;
        *self.listener.lock().unwrap() = Some(listener);
        Ok(())
    }

    async fn run(&self) -> Result<(), BoxError> {
        let listener = self
            .listener
            .lock()
            .unwrap()
            .take()
            .ok_or("axum listener missing: init did not run")?;
        let router = self.router.lock().unwrap().clone();
        // Serve until the `shutdown` hook (called by the stack) fires the
        // stop signal; axum then drains in-flight connections and returns.
        let stop_signal = Arc::clone(&self.stop_signal);

        axum::serve(listener, router)
            .with_graceful_shutdown(async move { stop_signal.notified().await })
            .await?;

        Ok(())
    }

    async fn shutdown(&self) -> Result<(), BoxError> {
        // Stop accepting and drain in-flight connections; the serve future
        // in `run` completes once draining is done.
        self.stop_signal.notify_waiters();
        Ok(())
    }
}

//##############################################################################
// Tests
//##############################################################################

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_table() {
        let cases: &[(&str, u16, Result<(), ConfigError>)] = &[
            ("common port accepted", 8080, Ok(())),
            ("low port accepted", 80, Ok(())),
            ("high port accepted", 65535, Ok(())),
            (
                "port 0 rejected",
                0,
                Err(ConfigError::InvalidField {
                    field: "port",
                    reason: "port must not be 0".to_string(),
                }),
            ),
        ];

        for (case, port, expected) in cases {
            let config = AxumConfig { port: *port };
            assert_eq!(&config.validate(), expected, "case '{case}'");
        }
    }

    #[tokio::test]
    async fn run_without_init_fails_cleanly() {
        let provider = AxumProvider::new(AxumConfig { port: 8080 });
        let result = provider.run().await;
        assert!(result.is_err(), "run must not serve without init");
    }
}
