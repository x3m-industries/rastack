use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::{Config, ConfigError, Provider, ProviderInfo};

//##############################################################################
// AppConfig
//##############################################################################

/// Config layers (via [`LoadConfig`](crate::LoadConfig)): env `APP_NAME`,
/// file key `app_name`, cli `--app-name=...`. Defaults to the executable's
/// file name.
#[derive(Debug, Serialize, Deserialize)]
pub struct AppConfig {
    /// The app's name, stamped on logs and shared with providers that
    /// identify the app externally. Layers: env `APP_NAME`, file key
    /// `app_name`, cli `--app-name=...`; defaults to the executable name.
    pub app_name: String,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            app_name: std::env::current_exe()
                .ok()
                .and_then(|path| {
                    path.file_stem()
                        .map(|stem| stem.to_string_lossy().into_owned())
                })
                .unwrap_or_default(),
        }
    }
}

impl Config for AppConfig {
    fn validate(&self) -> Result<(), ConfigError> {
        if self.app_name.trim().is_empty() {
            return Err(ConfigError::MissingField("app_name"));
        }

        Ok(())
    }
}

//##############################################################################
// AppProvider
//##############################################################################

/// Core delivery provider, embedded in every [`Stack`](crate::Stack):
/// delivers the app identity. Retrieve it via
/// [`Stack::app`](crate::Stack::app) to inject into providers that need the
/// app name (e.g. a mongodb client stamping it on connections).
pub struct AppProvider {
    config: AppConfig,
}

impl AppProvider {
    /// Creates the provider around a loaded [`AppConfig`].
    #[must_use]
    pub fn new(config: AppConfig) -> Arc<Self> {
        Arc::new(Self { config })
    }

    /// The configured app name (not the provider name, which is `"app"`).
    #[must_use]
    pub fn name(&self) -> &str {
        &self.config.app_name
    }
}

#[async_trait]
impl Provider for AppProvider {
    fn name(&self) -> &'static str {
        "app"
    }

    fn config(&self) -> Option<&dyn Config> {
        Some(&self.config)
    }

    fn info(&self) -> Option<ProviderInfo> {
        Some(vec![("name".to_string(), json!(self.config.app_name))])
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
        let cases: &[(&str, &str, Result<(), ConfigError>)] = &[
            ("non-empty name accepted", "demo", Ok(())),
            ("name with surrounding spaces accepted", "  demo  ", Ok(())),
            (
                "empty name rejected",
                "",
                Err(ConfigError::MissingField("app_name")),
            ),
            (
                "whitespace-only name rejected",
                "   ",
                Err(ConfigError::MissingField("app_name")),
            ),
            (
                "tabs and newlines rejected",
                "\t\n",
                Err(ConfigError::MissingField("app_name")),
            ),
        ];

        for (case, name, expected) in cases {
            let config = AppConfig {
                app_name: (*name).to_string(),
            };
            assert_eq!(&config.validate(), expected, "case '{case}'");
        }
    }

    #[test]
    fn default_name_is_the_executable() {
        // The default (no APP_NAME anywhere) is the exe name — never empty.
        let config = AppConfig::default();
        assert!(!config.app_name.trim().is_empty());
    }

    /// The app provider is embedded in every stack, so adding another one
    /// is a duplicate.
    #[tokio::test]
    async fn second_app_provider_rejected_as_duplicate() {
        let mut stack = crate::Stack::new();
        let result = stack
            .add(AppProvider::new(AppConfig {
                app_name: "other".to_string(),
            }))
            .await;

        assert!(matches!(
            result,
            Err(crate::StackError::DuplicateProvider(ref name)) if name == "app"
        ));
    }
}
