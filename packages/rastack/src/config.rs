//! Layered config loading for provider configs (12-factor style).
//!
//! Every config struct that derives `Serialize` + `Deserialize` and
//! implements `Default` gets [`LoadConfig::load`] for free. Values are
//! resolved from four layers, weakest to strongest:
//!
//! 1. the struct's `Default`
//! 2. environment variables
//! 3. a config file (`--config=<file>`, or `config.yaml` / `config.toml` /
//!    `config.json` next to the executable, then in the working directory)
//! 4. cli arguments (`--key=value`)
//!
//! CLI and file beat ENV **by design**: config baked into an image (cli
//! args, config file) must not be overridable from the environment. When an
//! environment variable is set but overridden by a stronger layer, a
//! warning is logged — that's either a misconfiguration or an override
//! attempt worth noticing.
//!
//! The convention is the field name, flat: a field `log_level` reads env
//! `LOG_LEVEL`, file key `log_level`, and cli `--log-level=...` /
//! `--log_level=...`.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::OnceLock;

use figment::Figment;
use figment::providers::{Format, Json, Serialized, Toml, Yaml};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value};

use crate::stack::provider::ConfigError;

//##############################################################################
// LoadConfig
//##############################################################################

/// Layered loading for config structs: defaults ← ENV ← config file ← CLI.
///
/// Blanket-implemented for every `Serialize + Deserialize + Default` type —
/// a provider config opts in by deriving serde and providing defaults,
/// nothing more.
pub trait LoadConfig: Sized {
    /// Loads the config from all layers.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Load`] when the config file is missing or
    /// malformed, or when a resolved value cannot be converted into the
    /// field's type.
    fn load() -> Result<Self, ConfigError>;
}

impl<T> LoadConfig for T
where
    T: DeserializeOwned + Serialize + Default,
{
    fn load() -> Result<Self, ConfigError> {
        let (result, warnings) = global().resolve::<T>();
        for warning in warnings {
            tracing::warn!("{warning}");
        }
        result
    }
}

//##############################################################################
// Global resolver
//##############################################################################

static RESOLVER: OnceLock<ConfigResolver> = OnceLock::new();

pub(crate) fn global() -> &'static ConfigResolver {
    RESOLVER.get_or_init(ConfigResolver::from_process)
}

//##############################################################################
// ConfigResolver
//##############################################################################

pub(crate) struct ConfigResolver {
    /// Environment variables, keys lowercased.
    env: BTreeMap<String, String>,
    /// `--key=value` cli overrides, keys normalized.
    cli: BTreeMap<String, String>,
    /// Parsed config file (`Ok(None)` = no file found).
    file: Result<Option<FileData>, String>,
}

pub(crate) struct FileData {
    label: String,
    values: Map<String, Value>,
}

impl ConfigResolver {
    fn from_process() -> Self {
        let args: Vec<String> = std::env::args().skip(1).collect();
        let (cli, config_path) = parse_cli(&args);
        let env = std::env::vars()
            .map(|(key, value)| (key.to_lowercase(), value))
            .collect();
        let file = discover_file(config_path);

        Self { env, cli, file }
    }

    /// Resolves `T` from all layers; returns the result plus any warnings
    /// about environment variables shadowed by stronger layers.
    pub(crate) fn resolve<T>(&self) -> (Result<T, ConfigError>, Vec<String>)
    where
        T: DeserializeOwned + Serialize + Default,
    {
        let mut warnings = Vec::new();
        let defaults = T::default();

        let Some(field_defaults) = serde_json::to_value(&defaults)
            .ok()
            .and_then(|value| value.as_object().cloned())
        else {
            return (
                Err(ConfigError::Load(
                    "config must serialize to a table of fields".to_string(),
                )),
                warnings,
            );
        };

        let file = match &self.file {
            Ok(file) => file.as_ref(),
            Err(error) => {
                return (Err(ConfigError::Load(error.clone())), warnings);
            }
        };

        let mut env_layer = Map::new();
        let mut file_layer = Map::new();
        let mut cli_layer = Map::new();
        for (field, default_value) in &field_defaults {
            if let Some(value) = self.env.get(field) {
                env_layer.insert(field.clone(), coerce(default_value, value));
            }
            if let Some(value) = file.and_then(|file| file.values.get(field)) {
                file_layer.insert(field.clone(), value.clone());
            }
            if let Some(value) = self.cli.get(field) {
                cli_layer.insert(field.clone(), coerce(default_value, value));
            }
        }

        // ENV set but beaten by a stronger layer: worth a warning — either a
        // misconfiguration or an attempt to override baked-in config.
        for field in field_defaults.keys() {
            if !env_layer.contains_key(field) {
                continue;
            }
            if cli_layer.contains_key(field) {
                warnings.push(format!(
                    "config key '{field}': environment variable is overridden by a \
                     cli argument (potential misconfiguration)"
                ));
            } else if file_layer.contains_key(field) {
                let label = file.map_or(String::new(), |f| format!(" ({})", f.label));
                warnings.push(format!(
                    "config key '{field}': environment variable is overridden by the \
                     config file{label} (potential misconfiguration)"
                ));
            }
        }

        let figment = Figment::from(Serialized::defaults(defaults))
            .merge(Serialized::defaults(env_layer))
            .merge(Serialized::defaults(file_layer))
            .merge(Serialized::defaults(cli_layer));

        let result = figment
            .extract::<T>()
            .map_err(|error| ConfigError::Load(error.to_string()));
        (result, warnings)
    }
}

//##############################################################################
// CLI parsing
//##############################################################################

fn normalize_key(key: &str) -> String {
    key.trim().to_lowercase().replace('-', "_")
}

/// Coerces a raw env/cli string into the JSON type of the field's default
/// value, so `PORT=8080` becomes a number and `FLAG=true` a bool. When the
/// string doesn't parse as that type it passes through unchanged and the
/// final extraction reports a proper type error.
fn coerce(default_value: &Value, raw: &str) -> Value {
    match default_value {
        Value::Bool(_) => raw
            .parse::<bool>()
            .map_or_else(|_| Value::String(raw.to_string()), Value::Bool),
        Value::Number(_) => raw
            .parse::<i64>()
            .map(Value::from)
            .or_else(|_| raw.parse::<u64>().map(Value::from))
            .or_else(|_| raw.parse::<f64>().map(Value::from))
            .unwrap_or_else(|_| Value::String(raw.to_string())),
        _ => Value::String(raw.to_string()),
    }
}

/// Splits raw process arguments into `--key=value` overrides and the
/// `--config` file path (clap handles `--config <path>` and
/// `--config=<path>`; unknown flags are ignored).
fn parse_cli(args: &[String]) -> (BTreeMap<String, String>, Option<PathBuf>) {
    let mut overrides = BTreeMap::new();
    let mut rest: Vec<String> = Vec::new();

    for arg in args {
        if let Some(body) = arg.strip_prefix("--")
            && let Some((key, value)) = body.split_once('=')
        {
            let key = normalize_key(key);
            if key != "config" {
                overrides.insert(key, value.to_string());
                continue;
            }
        }
        rest.push(arg.clone());
    }

    let config_path = clap::Command::new("rastack")
        .no_binary_name(true)
        .disable_help_flag(true)
        .disable_version_flag(true)
        .ignore_errors(true)
        .arg(
            clap::Arg::new("config")
                .long("config")
                .value_name("FILE")
                .num_args(1),
        )
        .try_get_matches_from(&rest)
        .ok()
        .and_then(|matches| matches.get_one::<String>("config").cloned())
        .map(PathBuf::from);

    (overrides, config_path)
}

//##############################################################################
// Config file discovery + parsing
//##############################################################################

const FILE_NAMES: [&str; 3] = ["config.yaml", "config.toml", "config.json"];

fn discover_file(explicit: Option<PathBuf>) -> Result<Option<FileData>, String> {
    if let Some(path) = explicit {
        if !path.exists() {
            return Err(format!("--config file '{}' not found", path.display()));
        }
        return read_file(&path).map(Some);
    }

    let mut directories = Vec::new();
    if let Some(exe_dir) = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(std::path::Path::to_path_buf))
    {
        directories.push(exe_dir);
    }
    if let Ok(cwd) = std::env::current_dir() {
        directories.push(cwd);
    }

    for directory in directories {
        for name in FILE_NAMES {
            let candidate = directory.join(name);
            if candidate.exists() {
                return read_file(&candidate).map(Some);
            }
        }
    }

    Ok(None)
}

fn read_file(path: &std::path::Path) -> Result<FileData, String> {
    let extension = path
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or("");
    let figment = match extension {
        "yaml" | "yml" => Figment::from(Yaml::file(path)),
        "toml" => Figment::from(Toml::file(path)),
        "json" => Figment::from(Json::file(path)),
        other => {
            return Err(format!(
                "config file '{}': unsupported extension '{other}' \
                 (expected yaml, toml or json)",
                path.display()
            ));
        }
    };

    parse_file_figment(&figment, &path.display().to_string())
}

fn parse_file_figment(figment: &Figment, label: &str) -> Result<FileData, String> {
    let value: Value = figment
        .extract()
        .map_err(|error| format!("config file '{label}': {error}"))?;
    let values = value
        .as_object()
        .cloned()
        .ok_or_else(|| format!("config file '{label}': must contain a top-level table"))?;

    Ok(FileData {
        label: label.to_string(),
        values,
    })
}

//##############################################################################
// Tests
//##############################################################################

#[cfg(test)]
mod tests {
    use serde::Deserialize;

    use super::*;

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct TestConfig {
        text: String,
        number: u16,
        flag: bool,
    }

    impl Default for TestConfig {
        fn default() -> Self {
            Self {
                text: "default-text".to_string(),
                number: 1,
                flag: false,
            }
        }
    }

    fn config(text: &str, number: u16, flag: bool) -> TestConfig {
        TestConfig {
            text: text.to_string(),
            number,
            flag,
        }
    }

    fn resolver(
        env: &[(&str, &str)],
        cli: &[(&str, &str)],
        file_yaml: Option<&str>,
    ) -> ConfigResolver {
        ConfigResolver {
            env: env
                .iter()
                .map(|(key, value)| (normalize_key(key), (*value).to_string()))
                .collect(),
            cli: cli
                .iter()
                .map(|(key, value)| (normalize_key(key), (*value).to_string()))
                .collect(),
            file: match file_yaml {
                None => Ok(None),
                Some(yaml) => {
                    parse_file_figment(&Figment::from(Yaml::string(yaml)), "inline").map(Some)
                }
            },
        }
    }

    #[test]
    fn precedence_table() {
        #[allow(clippy::type_complexity)]
        let cases: &[(
            &str,
            &[(&str, &str)], // env
            &[(&str, &str)], // cli
            Option<&str>,    // file yaml
            TestConfig,
            usize, // expected warning count
        )] = &[
            (
                "defaults only",
                &[],
                &[],
                None,
                config("default-text", 1, false),
                0,
            ),
            (
                "env only",
                &[("TEXT", "env-text"), ("NUMBER", "7"), ("FLAG", "true")],
                &[],
                None,
                config("env-text", 7, true),
                0,
            ),
            (
                "file only",
                &[],
                &[],
                Some("text: file-text\nnumber: 9"),
                config("file-text", 9, false),
                0,
            ),
            (
                "cli only",
                &[],
                &[("text", "cli-text"), ("number", "3")],
                None,
                config("cli-text", 3, false),
                0,
            ),
            (
                "file beats env",
                &[("TEXT", "env-text")],
                &[],
                Some("text: file-text"),
                config("file-text", 1, false),
                1,
            ),
            (
                "cli beats env",
                &[("TEXT", "env-text")],
                &[("text", "cli-text")],
                None,
                config("cli-text", 1, false),
                1,
            ),
            (
                "cli beats file",
                &[],
                &[("text", "cli-text")],
                Some("text: file-text"),
                config("cli-text", 1, false),
                0,
            ),
            (
                "cli beats file beats env",
                &[("TEXT", "env-text"), ("NUMBER", "7")],
                &[("text", "cli-text")],
                Some("text: file-text\nnumber: 9"),
                config("cli-text", 9, false),
                2,
            ),
            (
                "layers merge per key",
                &[("FLAG", "true")],
                &[("text", "cli-text")],
                Some("number: 9"),
                config("cli-text", 9, true),
                0,
            ),
        ];

        for (case, env, cli, file, expected, expected_warnings) in cases {
            let (result, warnings) = resolver(env, cli, *file).resolve::<TestConfig>();
            assert_eq!(
                result.as_ref().ok(),
                Some(expected),
                "case '{case}': {result:?}"
            );
            assert_eq!(
                warnings.len(),
                *expected_warnings,
                "case '{case}': warnings {warnings:?}"
            );
        }
    }

    #[test]
    fn warning_names_the_stronger_layer() {
        let (_, warnings) =
            resolver(&[("TEXT", "env")], &[("text", "cli")], None).resolve::<TestConfig>();
        assert!(
            warnings[0].contains("cli argument"),
            "warning must name the cli layer: {warnings:?}"
        );

        let (_, warnings) =
            resolver(&[("TEXT", "env")], &[], Some("text: file")).resolve::<TestConfig>();
        assert!(
            warnings[0].contains("config file"),
            "warning must name the file layer: {warnings:?}"
        );
    }

    #[test]
    fn unparseable_value_fails() {
        let (result, _) =
            resolver(&[("NUMBER", "not-a-number")], &[], None).resolve::<TestConfig>();
        assert!(matches!(result, Err(ConfigError::Load(_))), "{result:?}");
    }

    #[test]
    fn malformed_file_fails_every_load() {
        let resolver = resolver(&[], &[], Some("text: \"unterminated"));
        let (result, _) = resolver.resolve::<TestConfig>();
        assert!(matches!(result, Err(ConfigError::Load(_))), "{result:?}");
    }

    #[test]
    fn unknown_file_keys_are_ignored() {
        let (result, _) =
            resolver(&[], &[], Some("text: file-text\nunrelated: 42")).resolve::<TestConfig>();
        assert_eq!(result.unwrap(), config("file-text", 1, false));
    }

    #[test]
    fn parse_cli_table() {
        #[allow(clippy::type_complexity)]
        let cases: &[(&str, &[&str], &[(&str, &str)], Option<&str>)] = &[
            ("empty", &[], &[], None),
            (
                "key=value pairs",
                &["--text=cli-text", "--number=3"],
                &[("text", "cli-text"), ("number", "3")],
                None,
            ),
            (
                "dashes normalize to underscores",
                &["--log-level=debug"],
                &[("log_level", "debug")],
                None,
            ),
            (
                "config with equals",
                &["--config=/tmp/c.yaml"],
                &[],
                Some("/tmp/c.yaml"),
            ),
            (
                "config with space",
                &["--config", "/tmp/c.yaml"],
                &[],
                Some("/tmp/c.yaml"),
            ),
            (
                "unknown flags ignored",
                &["--verbose", "-q", "positional", "--text=x"],
                &[("text", "x")],
                None,
            ),
        ];

        for (case, args, expected_overrides, expected_config) in cases {
            let args: Vec<String> = args.iter().map(ToString::to_string).collect();
            let (overrides, config_path) = parse_cli(&args);
            let expected: BTreeMap<String, String> = expected_overrides
                .iter()
                .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
                .collect();
            assert_eq!(overrides, expected, "case '{case}'");
            assert_eq!(
                config_path,
                expected_config.map(PathBuf::from),
                "case '{case}'"
            );
        }
    }
}
