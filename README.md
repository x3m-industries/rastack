# rastack — Rust App Stack

[![CI](https://github.com/x3m-industries/rastack/actions/workflows/ci.yml/badge.svg)](https://github.com/x3m-industries/rastack/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-2024%20edition-orange.svg)](rust-toolchain.toml)

A provider-based application stack for Rust services. A `Stack` owns a set
of **providers** (an axum server, a database client, the logging pipeline),
brings them up in order, keeps them running, and — no matter how the app
ends — tears them down gracefully in reverse order.

```rust
use axum::{Router, routing::get};
use rastack::{AxumConfig, AxumProvider, LoadConfig, Stack, StackError};

#[tokio::main]
async fn main() -> Result<(), StackError> {
    // 1. create the stack (embeds tracing + app core providers)
    let mut stack = Stack::new();

    // 2. create + add optional providers, dependencies first
    let axum_provider = AxumProvider::new(AxumConfig::load()?);
    stack.add(axum_provider.clone()).await?;

    // 3. use providers
    axum_provider.add_routes(Router::new().route("/health", get(|| async { "ok" })));

    // 4. run the stack
    stack.run().await
}
```

```sh
cargo run --example app
APP_NAME=my-service PORT=3000 LOG_FORMAT=json cargo run --example app
curl http://localhost:8080/health
```

## What you get

- **Lifecycle management** — providers move through
  `ctor → init → run → shutdown → close`, driven entirely by the stack.
  Startup order is registration order; teardown is the exact reverse:
  graceful `shutdown` (per-provider timeout), then forced `close`.
- **Exit signals** — SIGINT/SIGTERM/SIGQUIT/SIGHUP on unix (docker,
  kubernetes), console events on Windows, always answered with a graceful
  teardown and a clean exit code.
- **Fail fast, die loud** — a provider failing `init` prevents startup; a
  `run` error or panic tears the whole app down with a typed error.
  A clean return from every `run` means the work was done: the app closes
  nicely (CLI-style apps).
- **Layered config (12-factor)** — every config struct loads from
  defaults ← ENV ← config file ← cli args by convention: field `port`
  reads `PORT`, file key `port`, `--port=...`. CLI and file beat ENV *by
  design* (baked-in image config is not env-overridable), and a shadowed
  ENV var logs a warning.
- **Logging built in** — the embedded tracing provider installs the global
  subscriber before anything else happens; `LOG_FORMAT=json` makes every
  line structured JSON, including the stack's own lifecycle logs.

## Core providers

Every stack embeds two core providers, available for injection into your
own providers:

| Provider | Delivers | Access |
|---|---|---|
| `TracingProvider` | logging/tracing config (`LOG_FORMAT`, `LOG_LEVEL`) | `stack.tracing()` |
| `AppProvider` | app identity (`APP_NAME`, or the executable name) | `stack.app()` |

Optional providers so far: `AxumProvider` (HTTP server: `PORT`). Planned:
`MongodbProvider`, ...

## Writing a provider

A provider is a reusable component implementing the `Provider` trait — all
lifecycle hooks are optional, its config is a plain serde struct:

```rust
#[derive(Serialize, Deserialize)]
pub struct MyConfig {
    /// Docs on every config prop.
    pub endpoint: String,
}
impl Default for MyConfig { /* ... */ }

// MyConfig::load()? now resolves ENV/file/cli — nothing else to write.
```

See [docs/PROVIDER.md](docs/PROVIDER.md) for the full provider contract and
[docs/STACK.md](docs/STACK.md) for the lifecycle, state machine, and signal
handling internals.

## Development

```sh
cargo test --workspace         # unit + abuse + integration (signal, config)
cargo clippy --workspace --all-targets   # pedantic, deny unsafe, deny missing docs
cargo fmt --all
cargo run --example app
```

Testing philosophy: table-driven tests everywhere, plus adversarial suites
— `tests/abuse.rs` attacks the lifecycle (panicking hooks, 1000 providers,
duplicate smuggling, state-machine double calls), `tests/signal.rs` kills a
real process with a real SIGTERM, `tests/config.rs` drives real processes
through env/file/cli config scenarios.

## Contributing

Contributions welcome — see [CONTRIBUTING.md](CONTRIBUTING.md) for the
one-time setup (repo-managed git hooks, commit template), the Conventional
Commits format, and the quality gates. Security reports go through
[SECURITY.md](SECURITY.md), community standards live in
[CODE_OF_CONDUCT.md](CODE_OF_CONDUCT.md).

## License

[MIT](LICENSE)
