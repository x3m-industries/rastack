# Stack

The `Stack` is the lifecycle manager at the heart of every rastack app. It
owns a set of [providers](PROVIDER.md), brings them up in order, keeps them
running, and — no matter how the app ends — tears them down in reverse order.

## The strict order

**Every rastack app is written in the same strict order** — this is the
canonical skeleton:

1. **create the stack** — the core providers (tracing, app) are embedded
2. **per optional provider, in dependency order: create it, then add it**
3. **use provider handles** — deliver routes, inject into dependents
4. **run the stack**

```rust
use rastack::{AxumConfig, AxumProvider, Stack, StackError};

#[tokio::main]
async fn main() -> Result<(), StackError> {
    // 1. create the stack (embeds tracing + app core providers)
    let mut stack = Stack::new();

    // 2. create + add optional providers, dependencies first
    let axum_provider = AxumProvider::new(AxumConfig::load());
    stack.add(axum_provider.clone()).await?;

    // 3. use providers
    axum_provider.add_routes(my_routes());

    // 4. run the stack
    stack.run().await
}
```

## Core providers

Two providers are embedded in every stack — `Stack::new()` builds them
before anything else:

- **`TracingProvider`** (required): the stack needs logging, and the
  stack's own logs follow the tracing config too. `new()` loads
  `LOG_FORMAT` / `LOG_LEVEL` and installs the global tracing subscriber
  immediately, so `LOG_FORMAT=json` makes *every* line JSON — including the
  stack's lifecycle logs. Logging must never prevent startup: an invalid
  config falls back to text/info with a warning.
- **`AppProvider`** (embedded, delivers the app identity): the name comes
  from `APP_NAME`, or falls back to the executable name.

Both are registered like normal providers (they participate in teardown and
the duplicate check — adding a second `app` or `tracing` provider fails),
and both are retrievable for injection into future providers:

```rust
let app = stack.app();         // Arc<AppProvider>
let tracing = stack.tracing(); // Arc<TracingProvider>
let mongodb_provider = MongodbProvider::new(mongodb_config, app);
```

Why the order is strict:

- **`add` does real work immediately** (config validation + `init`), so the
  create/add pairs *are* the startup sequence — reading `main` top to bottom
  reads exactly how the app boots, and reverse order is exactly how it
  tears down.
- **Dependencies before dependents.** A provider that needs another is
  created after it and receives the handle in its ctor; the pairing makes
  it impossible to add them in an order that contradicts construction.
- **Explicit provider variables** keep every handle available for injection
  into later providers (pass a `.clone()` to `add` when the handle is still
  needed afterwards).

## State machine

The stack tracks its state in a single atomic (`AtomicU8`), advanced only
through compare-and-swap transitions. Transitions are one-way:

```
Created ──run()──▶ Running ──teardown──▶ ShuttingDown ──▶ Closed
```

Because every transition is a compare-and-swap on the expected previous
state, each phase can execute **at most once** — a second `run`, a repeated
shutdown pass, or a close before shutdown is structurally impossible, not
just discouraged. On top of that, `run(self)` consumes the stack, so calling
it twice is a compile error.

## `add`: registration is initialization

`add` is async and does real work. For each provider, in order:

1. **State guard** — providers can only be added while `Created`.
2. **Duplicate check** — provider names are unique; adding a second provider
   with the same name fails with `StackError::DuplicateProvider`.
3. **Config validation** — if the provider exposes a config
   (`Provider::config()`), the stack calls `validate()` on it. Providers
   never validate their own config; the stack guarantees it happened.
4. **`init`** — the provider's async setup hook. Only when it succeeds is
   the provider registered.
5. **Info log** — the provider's `info()` key/values are logged, so startup
   output shows what each provider brought up.

A failure at any step returns a typed `StackError` and the provider is *not*
registered. Fail fast: if a provider cannot initialize, the app should not
start.

## `run`: drive until done or dying

`run` transitions to `Running`, spawns every provider's `run` as its own
tokio task, and then waits for one of three outcomes:

| Outcome | Meaning | Result |
|---|---|---|
| Exit signal received | Operator/platform asked us to stop | graceful teardown, `Ok(())` |
| Every `run` returned `Ok` | All work finished (CLI-style app) | graceful teardown, `Ok(())` |
| Any `run` returns `Err` or panics | The app is unstable | immediate teardown, `Err(StackError::Run / RunPanic)` |

Key semantics:

- A provider's `run` returning **cleanly** does not stop the stack — it is
  logged and the stack keeps going while other runs are still alive. Server
  providers (axum) simply never return; task providers return when their
  work is done. When the *last* run returns, the stack closes nicely.
- A run **error or panic** takes the whole app down, loudly. Panics are
  caught at the task boundary (they cannot unwind into the stack), attributed
  to the provider by task id, and reported as `StackError::RunPanic`.
- The failure that triggered teardown is returned from `run()` after
  teardown completes, so `main` naturally exits non-zero.

## Exit signals

The stack listens for every relevant kill signal, so it behaves correctly on
desktops, in containers, and under orchestrators:

- **Unix**: `SIGINT` (Ctrl-C), `SIGTERM` (docker stop, Kubernetes),
  `SIGQUIT`, `SIGHUP`
- **Windows**: Ctrl-C, Ctrl-Break, console close, user logoff, system
  shutdown
- **Other platforms**: Ctrl-C fallback

Signal handling is verified by a real-process integration test
(`packages/rastack/tests/signal.rs`) that spawns an app, sends it a genuine
`SIGTERM`, and asserts the teardown order and a clean exit code.

## Teardown: two passes, reverse order

Teardown always runs both cycles, in **reverse registration order** (last
added, first torn down — dependents go down before their dependencies):

1. **`shutdown` pass (graceful, bounded)** — stop accepting new work, drain
   in-flight work. Each provider gets at most the configured timeout
   (default 10s, tune with `Stack::with_shutdown_timeout`). A slow or hung
   provider is cut off and logged; the pass moves on.
2. **`close` pass (forced, unbounded)** — final cleanup. No timeout: this is
   the last word, and it runs even for providers whose shutdown failed or
   timed out.

Teardown failures are logged, never propagated — the passes always march
through every provider. After both passes, any still-running provider tasks
are aborted.

## Errors

All fallible stack operations return `StackError` (a `thiserror` enum), with
the failing provider's name attached and the underlying cause preserved via
`Error::source()`:

| Variant | When |
|---|---|
| `DuplicateProvider` | `add` with an already-registered name |
| `InvalidState` | operation attempted in the wrong lifecycle state |
| `Config` | config validation failed (`ConfigError` inside) |
| `Init` | provider `init` failed |
| `Run` | a provider's `run` returned an error |
| `RunPanic` | a provider's `run` panicked |

## Implementation notes

- Providers are stored as `Vec<Arc<dyn Provider>>`. `Arc` because each
  provider's `run` is spawned onto its own task while the stack retains the
  provider for teardown. Provider ctors return `Arc<Self>` and `add` takes
  `Arc<dyn Provider>` directly (auto-coerced), so the same instance can also
  be injected into dependent providers via `.clone()`.
- The `Provider` trait uses `#[async_trait]` (boxed futures) because
  natively-async trait methods are not yet dyn-compatible; the stack needs
  `dyn Provider` to hold heterogeneous providers.
- Hot paths borrow (`name()` returns `&str`); owned strings are only
  allocated on error paths and once per provider at spawn time.
- The stack logs through `tracing`, whose subscriber the embedded
  `TracingProvider` installs during `Stack::new()` — stack lifecycle logs
  follow the same `LOG_FORMAT` / `LOG_LEVEL` as everything else.
