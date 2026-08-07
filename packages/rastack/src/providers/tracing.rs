use std::sync::Arc;

use async_trait::async_trait;
use serde_json::json;

use serde::{Deserialize, Serialize};

use crate::{Config, ConfigError, Provider, ProviderInfo};

//##############################################################################
// TracingConfig
//##############################################################################

const FORMATS: [&str; 2] = ["text", "json"];

/// Config layers (via [`LoadConfig`](crate::LoadConfig)): env `LOG_FORMAT`
/// / `LOG_LEVEL`, file keys `log_format` / `log_level`, cli
/// `--log-format=...` / `--log-level=...`. Defaults: `text` / `info`.
#[derive(Debug, Serialize, Deserialize)]
pub struct TracingConfig {
    /// Log output format: `"text"` or `"json"`.
    pub log_format: String,
    /// Log level: `"trace"`, `"debug"`, `"info"`, `"warn"` or `"error"`.
    pub log_level: String,
}

impl Default for TracingConfig {
    fn default() -> Self {
        Self {
            log_format: "text".to_string(),
            log_level: "info".to_string(),
        }
    }
}

impl Config for TracingConfig {
    fn validate(&self) -> Result<(), ConfigError> {
        if !FORMATS.contains(&self.log_format.as_str()) {
            return Err(ConfigError::InvalidField {
                field: "log_format",
                reason: format!("'{}' is not one of {FORMATS:?}", self.log_format),
            });
        }

        if self.log_level.parse::<tracing::Level>().is_err() {
            return Err(ConfigError::InvalidField {
                field: "log_level",
                reason: format!("'{}' is not a log level", self.log_level),
            });
        }

        Ok(())
    }
}

//##############################################################################
// TracingProvider
//##############################################################################

/// Core config provider, embedded in every [`Stack`](crate::Stack): the
/// stack builds it in [`Stack::new`](crate::Stack::new) and installs the
/// global tracing subscriber immediately, so the stack's own logs follow
/// `LOG_FORMAT` / `LOG_LEVEL` from the very first line. Retrieve it via
/// [`Stack::tracing`](crate::Stack::tracing).
pub struct TracingProvider {
    config: TracingConfig,
}

impl TracingProvider {
    /// Creates the provider around a loaded [`TracingConfig`].
    #[must_use]
    pub fn new(config: TracingConfig) -> Arc<Self> {
        Arc::new(Self { config })
    }

    /// Installs the global tracing subscriber. Best-effort: when a
    /// subscriber is already installed (tests building several stacks in
    /// one process), the existing one is kept.
    pub(crate) fn install(&self) {
        let level = self
            .config
            .log_level
            .parse::<tracing::Level>()
            .unwrap_or(tracing::Level::INFO);
        let builder = tracing_subscriber::fmt().with_max_level(level);

        let result = if self.config.log_format == "json" {
            builder.json().try_init()
        } else {
            builder.try_init()
        };
        drop(result);
    }
}

#[async_trait]
impl Provider for TracingProvider {
    fn name(&self) -> &'static str {
        "tracing"
    }

    fn config(&self) -> Option<&dyn Config> {
        Some(&self.config)
    }

    fn info(&self) -> Option<ProviderInfo> {
        Some(vec![
            ("format".to_string(), json!(self.config.log_format)),
            ("level".to_string(), json!(self.config.log_level)),
        ])
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
        let cases: &[(&str, &str, &str, Result<(), &str>)] = &[
            // (case, format, level, expected: Ok / Err(field))
            ("text info accepted", "text", "info", Ok(())),
            ("json error accepted", "json", "error", Ok(())),
            ("uppercase level accepted", "text", "WARN", Ok(())),
            ("unknown format rejected", "xml", "info", Err("log_format")),
            ("empty format rejected", "", "info", Err("log_format")),
            (
                "unknown level rejected",
                "text",
                "verbose",
                Err("log_level"),
            ),
            ("empty level rejected", "text", "", Err("log_level")),
        ];

        for (case, format, level, expected) in cases {
            let config = TracingConfig {
                log_format: (*format).to_string(),
                log_level: (*level).to_string(),
            };
            let result = config.validate();
            match expected {
                Ok(()) => assert_eq!(result, Ok(()), "case '{case}'"),
                Err(field) => assert!(
                    matches!(result, Err(ConfigError::InvalidField { field: f, .. }) if f == *field),
                    "case '{case}': got {result:?}"
                ),
            }
        }
    }
}
