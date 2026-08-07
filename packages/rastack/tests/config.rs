//! Integration test for layered config loading, end to end.
//!
//! This binary runs twice. `cargo test` starts it as the orchestrator; for
//! each scenario it re-executes itself with `RASTACK_CONFIG_CHILD=1`, a
//! crafted environment, real cli arguments and (sometimes) a real config
//! file on disk. The child loads a config struct through
//! `rastack::LoadConfig` — exactly like a provider config would — and
//! prints the resolved values as JSON for the orchestrator to assert.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Command;

use rastack::LoadConfig;
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
#[allow(clippy::struct_field_names)] // prefixed to isolate the env vars
struct ProbeConfig {
    probe_text: String,
    probe_number: u16,
    probe_flag: bool,
}

impl Default for ProbeConfig {
    fn default() -> Self {
        Self {
            probe_text: "default-text".to_string(),
            probe_number: 1,
            probe_flag: false,
        }
    }
}

fn main() {
    if std::env::var_os("RASTACK_CONFIG_CHILD").is_some() {
        child();
    } else {
        orchestrate();
    }
}

//##############################################################################
// Child: load the config exactly like a provider config would
//##############################################################################

fn child() {
    match ProbeConfig::load() {
        Ok(config) => {
            println!("{}", serde_json::to_string(&config).expect("serialize"));
        }
        Err(error) => {
            println!("LOAD_ERROR: {error}");
            std::process::exit(1);
        }
    }
}

//##############################################################################
// Orchestrator: one child process per scenario
//##############################################################################

struct Scenario {
    name: &'static str,
    env: &'static [(&'static str, &'static str)],
    args: &'static [&'static str],
    /// Config file content (yaml) written to a temp file and passed via
    /// `--config`, when set.
    file: Option<&'static str>,
    /// Expected (`probe_text`, `probe_number`, `probe_flag`); `None` =
    /// expect a load error.
    expect: Option<(&'static str, u16, bool)>,
}

const SCENARIOS: &[Scenario] = &[
    Scenario {
        name: "defaults when nothing is set",
        env: &[],
        args: &[],
        file: None,
        expect: Some(("default-text", 1, false)),
    },
    Scenario {
        name: "env vars apply (uppercase, typed)",
        env: &[
            ("PROBE_TEXT", "env-text"),
            ("PROBE_NUMBER", "7"),
            ("PROBE_FLAG", "true"),
        ],
        args: &[],
        file: None,
        expect: Some(("env-text", 7, true)),
    },
    Scenario {
        name: "config file beats env",
        env: &[("PROBE_TEXT", "env-text")],
        args: &[],
        file: Some("probe_text: file-text\nprobe_number: 9\n"),
        expect: Some(("file-text", 9, false)),
    },
    Scenario {
        name: "cli beats config file and env",
        env: &[("PROBE_TEXT", "env-text")],
        args: &["--probe_text=cli-text"],
        file: Some("probe_text: file-text\nprobe_number: 9\n"),
        expect: Some(("cli-text", 9, false)),
    },
    Scenario {
        name: "dashed cli keys normalize",
        env: &[],
        args: &["--probe-number=42", "--probe-flag=true"],
        file: None,
        expect: Some(("default-text", 42, true)),
    },
    Scenario {
        name: "malformed config file fails the load",
        env: &[],
        args: &[],
        file: Some("probe_text: \"unterminated"),
        expect: None,
    },
    Scenario {
        name: "missing --config file fails the load",
        env: &[],
        args: &["--config=/nonexistent/nope.yaml"],
        file: None,
        expect: None,
    },
];

fn orchestrate() {
    let exe = std::env::current_exe().expect("current exe");
    let scratch = std::env::temp_dir().join(format!("rastack-config-{}", std::process::id()));
    std::fs::create_dir_all(&scratch).expect("create scratch dir");

    for (index, scenario) in SCENARIOS.iter().enumerate() {
        let mut args: Vec<String> = scenario.args.iter().map(ToString::to_string).collect();

        let mut file_path: Option<PathBuf> = None;
        if let Some(content) = scenario.file {
            let path = scratch.join(format!("scenario-{index}.yaml"));
            std::fs::write(&path, content).expect("write config file");
            args.push(format!("--config={}", path.display()));
            file_path = Some(path);
        }

        let output = Command::new(&exe)
            .args(&args)
            .env("RASTACK_CONFIG_CHILD", "1")
            // Isolate from ambient PROBE_* vars, then set the scenario's.
            .env_remove("PROBE_TEXT")
            .env_remove("PROBE_NUMBER")
            .env_remove("PROBE_FLAG")
            .envs(scenario.env.iter().copied())
            .output()
            .expect("spawn child");
        let stdout = String::from_utf8_lossy(&output.stdout);

        if let Some(path) = file_path {
            let _ = std::fs::remove_file(path);
        }

        match scenario.expect {
            Some((text, number, flag)) => {
                assert!(
                    output.status.success(),
                    "scenario '{}': child failed: {stdout}",
                    scenario.name
                );
                let resolved: BTreeMap<String, serde_json::Value> =
                    serde_json::from_str(stdout.trim()).unwrap_or_else(|_| {
                        panic!("scenario '{}': not json: {stdout}", scenario.name)
                    });
                assert_eq!(resolved["probe_text"], text, "scenario '{}'", scenario.name);
                assert_eq!(
                    resolved["probe_number"],
                    serde_json::json!(number),
                    "scenario '{}'",
                    scenario.name
                );
                assert_eq!(
                    resolved["probe_flag"],
                    serde_json::json!(flag),
                    "scenario '{}'",
                    scenario.name
                );
            }
            None => {
                assert!(
                    !output.status.success() && stdout.contains("LOAD_ERROR"),
                    "scenario '{}': expected a load error, got: {stdout}",
                    scenario.name
                );
            }
        }
    }

    let _ = std::fs::remove_dir_all(scratch);
    println!(
        "config integration test passed: {} scenarios",
        SCENARIOS.len()
    );
}
