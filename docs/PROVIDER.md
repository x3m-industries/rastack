# Provider

A **provider** is a reusable building block of a rastack app: one component
that owns one capability — the app identity, an HTTP server, a database
client, the logging pipeline. An app is assembled by adding providers to a
[`Stack`](STACK.md), which manages their entire lifecycle.

Typical traits of a provider:

- it is **reusable** across apps (write the mongodb provider once, use it in
  every service);
- it usually needs **config**, validated before the provider initializes;
- it has a **lifecycle** — construction, setup, serving, graceful stop,
  forced cleanup — driven by the stack, never by the provider itself;
- it may **depend on other providers**, received through its constructor.

Examples of providers this project has or will grow:

| Provider | Delivers | Status |
|---|---|---|
| `TracingProvider` | logging / tracing configuration | core — embedded in every stack |
| `AppProvider` | app identity (name, version) | core — embedded in every stack |
| `AxumProvider` | an axum HTTP server | optional |
| `MongodbProvider` | a mongodb client | planned |

The two core providers are built by `Stack::new()` itself (see
[STACK.md](STACK.md#core-providers)) and retrieved via `stack.tracing()` /
`stack.app()` for injection into other providers.

## The lifecycle layers

Every provider implements the `Provider` trait. All hooks are optional
except `name` — a provider overrides only what it needs.

```
ctor ──▶ init ──▶ run ──▶ shutdown ──▶ close
```

| Layer | Contract |
|---|---|
| **ctor** (`new(...)`) | Pure construction, no external dependencies, cannot fail meaningfully. Receives config and any provider dependencies. Returns `Arc<Self>`, so the same instance can go to the stack *and* be injected into dependents via `.clone()`. |
| **`init`** | Async setup: acquire ports, open connections, prepare resources. Called by `Stack::add`; failure means the provider is never registered and the app does not start. |
| **`run`** | Start the actual work. Server-style providers hold this future open until the stack tears down (an HTTP server serving). Task-style providers do their work and return `Ok` (a CLI job). Default: returns immediately. Returning `Err` or panicking tears the whole stack down. |
| **`shutdown`** | Graceful stop: stop accepting new work, finish in-flight work. Bounded by the stack's per-provider timeout. |
| **`close`** | Forced final cleanup. No timeout; runs even if `shutdown` failed or timed out. |

The stack calls `init` in registration order, and `shutdown`/`close` in
**reverse** registration order — so a provider can rely on its dependencies
being alive both when it starts and while it is shutting down.

## Identity: `name`

`name()` returns the provider's unique identity (e.g. `"app"`, `"axum"`,
`"mongodb"`). The stack rejects a second provider with the same name
(`StackError::DuplicateProvider`), and uses the name in every log line and
error it produces. Return a `&'static str` literal.

## Config

### Defining a config

A provider config is a plain struct with serde derives and a `Default`:

```rust
#[derive(Serialize, Deserialize)]
pub struct AxumConfig {
    pub port: u16,
}

impl Default for AxumConfig {
    fn default() -> Self {
        Self { port: 8080 }
    }
}
```

That is the entire opt-in. The blanket `LoadConfig` trait then provides
layered loading — `AxumConfig::load()?` — with no further code.

### Layered loading (12-factor)

`load()` resolves every field from four layers, weakest to strongest:

| Layer | Source | Example for field `port` |
|---|---|---|
| 1. defaults | the struct's `Default` | `8080` |
| 2. environment | field name, uppercased | `PORT=3000` |
| 3. config file | flat key in `--config=<file>`, or `config.yaml` / `config.toml` / `config.json` next to the executable (then in the working directory) | `port: 3000` |
| 4. cli arguments | `--key=value` (dashes normalize: `--log-level` = `--log_level`) | `--port=3000` |

**CLI and file beat ENV by design.** Config baked into an image (cli args
in an entrypoint, a config file next to the binary) must not be overridable
from the environment. When an environment variable is set but a stronger
layer overrides it, the stack logs a **warning** — that's either a
misconfiguration or an override attempt worth noticing.

The convention is the field name, flat: name the field the way the key
should look everywhere (`app_name` → `APP_NAME` / `app_name:` /
`--app-name`). Values are typed: `PORT=8080` becomes a number, `FLAG=true`
a bool; a value that doesn't fit the field's type fails the load.

A failed load (malformed config file, missing `--config` file, bad value)
returns `ConfigError::Load` — for optional providers that aborts startup
(`AxumConfig::load()?` in `main`); the embedded core providers fall back to
their defaults with a warning, because logging must never prevent startup.

### Validation

Config types additionally implement the `Config` trait:

```rust
impl Config for AppConfig {
    fn validate(&self) -> Result<(), ConfigError> {
        if self.app_name.trim().is_empty() {
            return Err(ConfigError::MissingField("app_name"));
        }
        Ok(())
    }
}
```

A provider exposes its config through `Provider::config()`:

```rust
fn config(&self) -> Option<&dyn Config> {
    Some(&self.config)
}
```

**The stack validates the config before calling `init`** — providers never
call `validate()` themselves, and a provider whose config is invalid is
never initialized. `ConfigError` is a structured enum (`MissingField`,
`InvalidField`, `Load`), so tests assert on exact values rather than
message strings. Required fields are expressed through `validate`: give the
field an empty/sentinel default and reject it there.

## Diagnostics: `info`

After a successful `init`, the stack logs what the provider brought up.
`info()` returns key/value pairs with **native JSON values**
(`serde_json::Value`), so a future structured/JSON log sink keeps numbers as
numbers and bools as bools:

```rust
fn info(&self) -> Option<ProviderInfo> {
    Some(vec![
        ("name".to_string(), json!(self.config.app_name)),
        // ("port".to_string(), json!(8080)),
        // ("tls".to_string(), json!(true)),
    ])
}
```

Share whatever is sufficient for diagnostics — never secrets. `None` (the
default) logs the bare registration.

## Dependencies between providers

Some providers need others. Dependencies are passed **via the constructor**,
before either provider is added to the stack. Example: the mongodb
connection options carry an app name, which mongodb then echoes in its
request logs — so `MongodbProvider` depends on `AppProvider`:

Constructors return `Arc<Self>`, so injecting a dependency is a cheap
`.clone()` — the dependent and the stack share the same instance. Following
the [strict order](STACK.md#the-strict-order) (create + add, one provider
at a time, dependencies first):

```rust
let mut stack = Stack::new();

let app_provider = AppProvider::new(AppConfig::load()); // Arc<AppProvider>
stack.add(app_provider.clone()).await?; // clone: the handle is needed below

// The dependent ctor receives a handle to the dependency.
let mongodb_provider = MongodbProvider::new(mongodb_config, app_provider.clone());
stack.add(mongodb_provider).await?;

stack.run().await
```

`Stack::add` takes `Arc<dyn Provider>`; an `Arc<AppProvider>` coerces to it
automatically at the call site.

Two rules make this safe:

1. **Add dependencies before dependents.** Registration order is startup
   order, and reverse registration order is teardown order — so `mongodb`
   initializes after `app` and is torn down before it.
2. **Take only what you need.** If the dependent just needs a value (the app
   name), copy it out in the ctor instead of holding the whole provider;
   keep the `Arc` clone only when it needs a live handle for its whole
   lifetime.

## Failure philosophy

- `init` failure → the app never starts (`Stack::add` returns the error).
- `run` failure or panic → the whole stack tears down and the app exits
  with an error. An unstable app must die loudly, not limp along.
- All `run`s returning cleanly → the app closed nicely; that was intended.
- `shutdown`/`close` failures → logged, but teardown always continues to
  the remaining providers.

## Minimal provider

```rust
use std::sync::Arc;

use async_trait::async_trait;
use rastack::{BoxError, Provider};

struct HeartbeatProvider;

impl HeartbeatProvider {
    pub fn new() -> Arc<Self> {
        Arc::new(Self)
    }
}

#[async_trait]
impl Provider for HeartbeatProvider {
    fn name(&self) -> &'static str {
        "heartbeat"
    }

    async fn run(&self) -> Result<(), BoxError> {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            eprintln!("thump");
        }
    }
}
```

Everything not overridden (config, info, init, shutdown, close) falls back
to a sensible default: no config, no info, instant init, instant clean run
(for providers that only exist for init-time side effects), no-op teardown.
