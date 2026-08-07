use async_trait::async_trait;
use serde_json::Value;

/// Boxed error type returned from provider lifecycle hooks.
pub type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// Marker returned by the default `run`/`shutdown`/`close` implementations
/// so the stack can tell "hook not implemented" apart from a hook that
/// actually ran — unimplemented hooks are skipped silently instead of being
/// logged.
#[derive(Debug)]
pub(crate) struct NotImplemented;

impl std::fmt::Display for NotImplemented {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("lifecycle hook not implemented")
    }
}

impl std::error::Error for NotImplemented {}

/// Diagnostic details a provider shares after init: keys mapped to native
/// JSON values (string, number, bool, array, object).
pub type ProviderInfo = Vec<(String, Value)>;

/// Reason a [`Config`] failed validation. Structured so callers and unit
/// tests can match on the exact failure instead of comparing message strings.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConfigError {
    /// A required field is missing or empty.
    #[error("required field '{0}' is missing or empty")]
    MissingField(&'static str),

    /// A field holds an invalid value.
    #[error("field '{field}' is invalid: {reason}")]
    InvalidField {
        /// Name of the invalid field.
        field: &'static str,
        /// Why the value is invalid.
        reason: String,
    },

    /// Loading the config from its layers (env, config file, cli) failed.
    #[error("failed to load config: {0}")]
    Load(String),
}

/// Validation for provider configs. The stack calls [`validate`](Config::validate)
/// before a provider's `init` — providers never validate themselves.
pub trait Config {
    /// Checks the config for invalid or missing values.
    ///
    /// # Errors
    ///
    /// Returns a [`ConfigError`] describing why the configuration is invalid.
    fn validate(&self) -> Result<(), ConfigError>;
}

/// A unit of the application managed by the [`Stack`](crate::Stack).
///
/// Lifecycle layers, in order:
/// 1. ctor — pure construction, no external dependencies
/// 2. [`init`](Provider::init) — setup: validate config, acquire ports, connect, ...
/// 3. [`run`](Provider::run) — start serving; expected to keep running
/// 4. [`shutdown`](Provider::shutdown) — stop accepting, drain in-flight work (bounded by a timeout)
/// 5. [`close`](Provider::close) — forced cleanup, no timeout
#[async_trait]
pub trait Provider: Send + Sync + 'static {
    /// Unique name of this provider. A [`Stack`](crate::Stack) refuses to
    /// register two providers with the same name.
    fn name(&self) -> &str;

    /// The provider's configuration, when it has one. [`Stack::add`](crate::Stack::add)
    /// validates it before calling [`init`](Provider::init), so providers
    /// never need to call [`Config::validate`] themselves.
    fn config(&self) -> Option<&dyn Config> {
        None
    }

    /// Optional key/value details logged by the stack right after this
    /// provider's [`init`](Provider::init) succeeds. Values are native JSON
    /// types (via [`serde_json::json!`]), so structured log output keeps
    /// numbers as numbers and bools as bools. Share whatever is sufficient
    /// for diagnostics; `None` logs the bare registration.
    fn info(&self) -> Option<ProviderInfo> {
        None
    }

    /// Setup phase, called by [`Stack::add`](crate::Stack::add). The provider
    /// is only registered when this succeeds.
    ///
    /// # Errors
    ///
    /// Returns an error when setup fails; the provider is then not added.
    async fn init(&self) -> Result<(), BoxError> {
        Ok(())
    }

    /// Start the provider's work.
    ///
    /// Long-lived providers (servers) hold this future open until they stop;
    /// the stack keeps running as long as at least one `run` is still going.
    /// Task-style providers (CLI work) return `Ok(())` when their work is
    /// done — once every `run` has returned cleanly, the stack closes nicely.
    /// Providers that don't override `run` have no long-running work: the
    /// stack skips them silently (no "run completed" log).
    ///
    /// # Errors
    ///
    /// Returning an error (or panicking) tears the whole stack down: shutdown
    /// and close cycles still execute, but the stack exits with an error.
    async fn run(&self) -> Result<(), BoxError> {
        Err(Box::new(NotImplemented))
    }

    /// Graceful stop: stop accepting new work, finish in-flight work.
    /// Bounded by the stack's per-provider shutdown timeout.
    ///
    /// # Errors
    ///
    /// Returns an error when graceful shutdown fails; the stack proceeds to
    /// [`close`](Provider::close) regardless.
    async fn shutdown(&self) -> Result<(), BoxError> {
        Err(Box::new(NotImplemented))
    }

    /// Final forced cleanup. Not subject to a timeout.
    ///
    /// # Errors
    ///
    /// Returns an error when cleanup fails; reported, never retried.
    async fn close(&self) -> Result<(), BoxError> {
        Err(Box::new(NotImplemented))
    }
}
