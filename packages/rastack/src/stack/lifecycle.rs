use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::Duration;

use tokio::task::JoinSet;

use crate::config::LoadConfig;
use crate::providers::app::{AppConfig, AppProvider};
use crate::providers::tracing::{TracingConfig, TracingProvider};
use crate::stack::provider::{BoxError, Config, ConfigError, NotImplemented, Provider};

/// Logs a provider's registration with its `info()` details.
fn log_initialized(provider: &dyn Provider) {
    let name = provider.name();
    match provider.info() {
        Some(info) if !info.is_empty() => {
            let details = info
                .iter()
                .map(|(key, value)| match value {
                    // Text rendering: plain strings without JSON quoting.
                    serde_json::Value::String(text) => format!("{key}={text}"),
                    other => format!("{key}={other}"),
                })
                .collect::<Vec<_>>()
                .join(", ");
            tracing::info!("provider '{name}' initialized ({details})");
        }
        _ => tracing::info!("provider '{name}' initialized"),
    }
}

const DEFAULT_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(10);

//##############################################################################
// StackState
//##############################################################################

/// Lifecycle state of a [`Stack`], tracked atomically. Transitions are
/// one-way (`Created → Running → ShuttingDown → Closed`), which guarantees
/// each phase executes at most once.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StackState {
    /// Built; providers can be added.
    Created = 0,
    /// `run` was called; provider runs are executing.
    Running = 1,
    /// The graceful `shutdown` pass is executing (or done).
    ShuttingDown = 2,
    /// The forced `close` pass ran; the stack is finished.
    Closed = 3,
}

impl StackState {
    fn from_u8(value: u8) -> Self {
        match value {
            0 => Self::Created,
            1 => Self::Running,
            2 => Self::ShuttingDown,
            _ => Self::Closed,
        }
    }
}

//##############################################################################
// StackError
//##############################################################################

/// Everything that can go wrong while building or running a [`Stack`].
#[derive(Debug, thiserror::Error)]
pub enum StackError {
    /// A provider with the same name is already registered.
    #[error("provider '{0}' is already registered")]
    DuplicateProvider(String),

    /// The stack was not in the state required for the attempted operation.
    #[error("stack is in state {actual:?}, expected {expected:?}")]
    InvalidState {
        /// The state the operation requires.
        expected: StackState,
        /// The state the stack was actually in.
        actual: StackState,
    },

    /// A provider's config failed validation; the provider was not initialized.
    #[error("provider '{provider}' has invalid config: {source}")]
    Config {
        /// Name of the provider whose config failed validation.
        provider: String,
        /// The validation failure.
        #[source]
        source: ConfigError,
    },

    /// A provider failed its `init` phase and was not registered.
    #[error("provider '{provider}' failed to init: {source}")]
    Init {
        /// Name of the provider whose `init` failed.
        provider: String,
        /// The init failure.
        #[source]
        source: BoxError,
    },

    /// A provider's `run` returned an error; the stack was torn down.
    #[error("provider '{provider}' run failed: {source}")]
    Run {
        /// Name of the provider whose `run` failed.
        provider: String,
        /// The run failure.
        #[source]
        source: BoxError,
    },

    /// A provider's `run` panicked; the stack was torn down.
    #[error("provider '{provider}' run panicked: {message}")]
    RunPanic {
        /// Name of the provider whose `run` panicked.
        provider: String,
        /// The panic message.
        message: String,
    },

    /// A config failed to load from its layers (env, config file, cli), so
    /// the provider could not even be constructed.
    #[error("config loading failed: {0}")]
    ConfigLoad(#[from] ConfigError),
}

//##############################################################################
// Stack
//##############################################################################

/// Lifecycle manager for [`Provider`]s.
///
/// Providers are initialized when added, started together by [`run`](Stack::run),
/// and torn down in reverse registration order — first a graceful `shutdown`
/// pass (bounded per provider by a timeout), then a forced `close` pass.
///
/// Two core providers are embedded in every stack, built by [`new`](Stack::new):
///
/// - [`TracingProvider`] — installed immediately, so the stack's own logs
///   follow `LOG_FORMAT` / `LOG_LEVEL` from the first line
/// - [`AppProvider`] — the app's identity (`APP_NAME` or the executable name)
///
/// Retrieve them via [`tracing`](Stack::tracing) / [`app`](Stack::app) to
/// inject into providers that need them.
pub struct Stack {
    providers: Vec<Arc<dyn Provider>>,
    tracing: Arc<TracingProvider>,
    app: Arc<AppProvider>,
    state: AtomicU8,
    shutdown_timeout: Duration,
}

impl Default for Stack {
    fn default() -> Self {
        Self::new()
    }
}

impl Stack {
    /// Creates a stack with the embedded core providers: tracing (config
    /// loaded from all layers, subscriber installed immediately) and app
    /// identity. Core config problems fall back to defaults with a warning
    /// — creating a stack never fails.
    #[must_use]
    pub fn new() -> Self {
        // Core provider: tracing. Config resolves through all layers (env,
        // config file, cli) and the subscriber installs right here, so
        // every log line below already follows it. Logging must never
        // prevent startup: an invalid config falls back to defaults with a
        // warning instead of failing. Warnings buffer until the subscriber
        // is installed.
        let (tracing_result, tracing_warnings) = crate::config::global().resolve();
        let tracing_config = tracing_result
            .map_err(|error: ConfigError| error.to_string())
            .and_then(|config: TracingConfig| {
                config
                    .validate()
                    .map_err(|error| error.to_string())
                    .map(|()| config)
            });
        let (tracing_config, tracing_fallback) = match tracing_config {
            Ok(config) => (config, None),
            Err(error) => (TracingConfig::default(), Some(error)),
        };
        let tracing = TracingProvider::new(tracing_config);
        tracing.install();
        for warning in tracing_warnings {
            tracing::warn!("{warning}");
        }
        if let Some(error) = tracing_fallback {
            tracing::warn!("invalid tracing config, falling back to defaults: {error}");
        }

        // Core provider: app identity. Same policy: never prevent startup.
        let app_config = AppConfig::load().unwrap_or_else(|error| {
            tracing::warn!("invalid app config, falling back to defaults: {error}");
            AppConfig::default()
        });
        let app = AppProvider::new(app_config);

        let providers: Vec<Arc<dyn Provider>> = vec![
            Arc::clone(&tracing) as Arc<dyn Provider>,
            Arc::clone(&app) as Arc<dyn Provider>,
        ];
        for provider in &providers {
            log_initialized(provider.as_ref());
        }

        Self {
            providers,
            tracing,
            app,
            state: AtomicU8::new(StackState::Created as u8),
            shutdown_timeout: DEFAULT_SHUTDOWN_TIMEOUT,
        }
    }

    /// Sets the per-provider timeout for the graceful `shutdown` pass.
    #[must_use]
    pub fn with_shutdown_timeout(mut self, timeout: Duration) -> Self {
        self.shutdown_timeout = timeout;
        self
    }

    /// The embedded tracing provider — inject it into providers that need
    /// the logging configuration.
    #[must_use]
    pub fn tracing(&self) -> Arc<TracingProvider> {
        Arc::clone(&self.tracing)
    }

    /// The embedded app provider — inject it into providers that need the
    /// app identity (e.g. a mongodb client stamping the app name on its
    /// connections).
    #[must_use]
    pub fn app(&self) -> Arc<AppProvider> {
        Arc::clone(&self.app)
    }

    /// Current lifecycle state.
    #[must_use]
    pub fn state(&self) -> StackState {
        StackState::from_u8(self.state.load(Ordering::Acquire))
    }

    /// Initializes `provider` and, when init succeeds, registers it.
    ///
    /// Provider constructors return `Arc<Self>`, which coerces to
    /// `Arc<dyn Provider>` here — keep a clone when another provider needs
    /// the same instance injected.
    ///
    /// # Errors
    ///
    /// Returns [`StackError::InvalidState`] when the stack already runs,
    /// [`StackError::DuplicateProvider`] when a provider with the same name is
    /// already registered, [`StackError::Config`] when the provider's config
    /// fails validation, or [`StackError::Init`] when the provider's `init`
    /// fails.
    pub async fn add(&mut self, provider: Arc<dyn Provider>) -> Result<(), StackError> {
        let actual = self.state();
        if actual != StackState::Created {
            return Err(StackError::InvalidState {
                expected: StackState::Created,
                actual,
            });
        }

        let name = provider.name();
        if self.providers.iter().any(|p| p.name() == name) {
            return Err(StackError::DuplicateProvider(name.to_owned()));
        }

        if let Some(config) = provider.config() {
            config.validate().map_err(|source| StackError::Config {
                provider: name.to_owned(),
                source,
            })?;
        }

        provider.init().await.map_err(|source| StackError::Init {
            provider: name.to_owned(),
            source,
        })?;

        log_initialized(provider.as_ref());

        self.providers.push(provider);
        Ok(())
    }

    /// Starts every provider's `run` and drives the stack until one of:
    ///
    /// - an exit signal arrives → graceful teardown, returns `Ok`
    /// - every `run` has returned cleanly (or none exists) → the work is
    ///   done, graceful teardown, returns `Ok`
    /// - any `run` returns an error or panics → immediate teardown, returns
    ///   that provider's failure
    ///
    /// Teardown always executes both cycles in reverse registration order:
    /// graceful `shutdown` (per-provider timeout), then forced `close`.
    ///
    /// # Errors
    ///
    /// Returns [`StackError::InvalidState`] when the stack was already run,
    /// [`StackError::Run`] when a provider's `run` returned an error, or
    /// [`StackError::RunPanic`] when a provider's `run` panicked.
    pub async fn run(self) -> Result<(), StackError> {
        self.transition(StackState::Created, StackState::Running)?;

        let mut tasks = JoinSet::new();
        let mut names: HashMap<tokio::task::Id, String> = HashMap::new();
        for provider in &self.providers {
            let provider = Arc::clone(provider);
            let name = provider.name().to_owned();
            let handle = tasks.spawn(async move { provider.run().await });
            names.insert(handle.id(), name);
        }

        let outcome = loop {
            tokio::select! {
                () = wait_for_exit_signal() => {
                    tracing::info!("exit signal received, shutting down");
                    break Ok(());
                }
                joined = tasks.join_next_with_id() => match joined {
                    None => {
                        tracing::info!("all provider runs completed, closing");
                        break Ok(());
                    }
                    Some(Ok((id, Ok(())))) => {
                        let name = names.get(&id).map_or("unknown", String::as_str);
                        tracing::info!("provider '{name}' run completed");
                    }
                    // Default `run`: the provider has no long-running work —
                    // skip it silently.
                    Some(Ok((_, Err(source)))) if source.is::<NotImplemented>() => {}
                    Some(Ok((id, Err(source)))) => {
                        let provider = names
                            .remove(&id)
                            .unwrap_or_else(|| "unknown".to_owned());
                        tracing::error!(
                            "provider '{provider}' run failed: {source}, shutting down"
                        );
                        break Err(StackError::Run { provider, source });
                    }
                    Some(Err(join_error)) => {
                        let provider = names
                            .remove(&join_error.id())
                            .unwrap_or_else(|| "unknown".to_owned());
                        let message = join_error.to_string();
                        tracing::error!(
                            "provider '{provider}' run panicked: {message}, shutting down"
                        );
                        break Err(StackError::RunPanic { provider, message });
                    }
                },
            }
        };

        self.shutdown().await;
        self.close().await;
        tasks.shutdown().await;

        outcome
    }

    /// Graceful pass: `shutdown` on each provider in reverse order, each
    /// bounded by the configured timeout. Runs at most once.
    async fn shutdown(&self) {
        if self
            .transition(StackState::Running, StackState::ShuttingDown)
            .is_err()
        {
            return;
        }

        for provider in self.providers.iter().rev() {
            let name = provider.name();
            // Each hook runs in its own task: a panicking provider must
            // never unwind into the teardown loop and skip the others.
            let hook = Arc::clone(provider);
            let mut task = tokio::spawn(async move { hook.shutdown().await });
            match tokio::time::timeout(self.shutdown_timeout, &mut task).await {
                Ok(Ok(Ok(()))) => {
                    tracing::info!("provider '{name}' shut down");
                }
                // Default `shutdown`: nothing to do, nothing to log.
                Ok(Ok(Err(err))) if err.is::<NotImplemented>() => {}
                Ok(Ok(Err(err))) => {
                    tracing::error!("provider '{name}' shutdown error: {err}");
                }
                Ok(Err(panic)) => {
                    tracing::error!("provider '{name}' shutdown panicked: {panic}");
                }
                Err(_) => {
                    task.abort();
                    tracing::warn!(
                        "provider '{name}' shutdown timed out after {:?}",
                        self.shutdown_timeout
                    );
                }
            }
        }
    }

    /// Forced pass: `close` on each provider in reverse order, no timeout.
    /// Runs at most once.
    async fn close(&self) {
        if self
            .transition(StackState::ShuttingDown, StackState::Closed)
            .is_err()
        {
            return;
        }

        for provider in self.providers.iter().rev() {
            let name = provider.name();
            // Panic isolation, same as the shutdown pass.
            let hook = Arc::clone(provider);
            match tokio::spawn(async move { hook.close().await }).await {
                Ok(Ok(())) => {
                    tracing::info!("provider '{name}' closed");
                }
                // Default `close`: nothing to do, nothing to log.
                Ok(Err(err)) if err.is::<NotImplemented>() => {}
                Ok(Err(err)) => {
                    tracing::error!("provider '{name}' close error: {err}");
                }
                Err(panic) => {
                    tracing::error!("provider '{name}' close panicked: {panic}");
                }
            }
        }
    }

    fn transition(&self, from: StackState, to: StackState) -> Result<(), StackError> {
        self.state
            .compare_exchange(from as u8, to as u8, Ordering::AcqRel, Ordering::Acquire)
            .map(|_| ())
            .map_err(|actual| StackError::InvalidState {
                expected: from,
                actual: StackState::from_u8(actual),
            })
    }
}

//##############################################################################
// Exit signals
//##############################################################################

/// Resolves when any process exit signal arrives.
///
/// Unix: SIGINT, SIGTERM (docker/kubernetes stop), SIGQUIT, SIGHUP.
/// Windows: Ctrl-C, Ctrl-Break, console close, user logoff, system shutdown.
#[cfg(unix)]
async fn wait_for_exit_signal() {
    use tokio::signal::unix::{SignalKind, signal};

    let mut interrupt = signal(SignalKind::interrupt()).expect("failed to install SIGINT handler");
    let mut terminate = signal(SignalKind::terminate()).expect("failed to install SIGTERM handler");
    let mut quit = signal(SignalKind::quit()).expect("failed to install SIGQUIT handler");
    let mut hangup = signal(SignalKind::hangup()).expect("failed to install SIGHUP handler");

    tokio::select! {
        _ = interrupt.recv() => {}
        _ = terminate.recv() => {}
        _ = quit.recv() => {}
        _ = hangup.recv() => {}
    }
}

#[cfg(windows)]
async fn wait_for_exit_signal() {
    use tokio::signal::windows;

    let mut ctrl_c = windows::ctrl_c().expect("failed to install Ctrl-C handler");
    let mut ctrl_break = windows::ctrl_break().expect("failed to install Ctrl-Break handler");
    let mut ctrl_close = windows::ctrl_close().expect("failed to install Ctrl-Close handler");
    let mut ctrl_logoff = windows::ctrl_logoff().expect("failed to install Ctrl-Logoff handler");
    let mut ctrl_shutdown =
        windows::ctrl_shutdown().expect("failed to install Ctrl-Shutdown handler");

    tokio::select! {
        _ = ctrl_c.recv() => {}
        _ = ctrl_break.recv() => {}
        _ = ctrl_close.recv() => {}
        _ = ctrl_logoff.recv() => {}
        _ = ctrl_shutdown.recv() => {}
    }
}

#[cfg(not(any(unix, windows)))]
async fn wait_for_exit_signal() {
    let _ = tokio::signal::ctrl_c().await;
}

//##############################################################################
// Tests
//##############################################################################

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use async_trait::async_trait;

    use super::*;
    use crate::stack::provider::{Config, ConfigError};

    //##########################################################################
    // Test harness: one configurable provider driven by a `Spec`
    //##########################################################################

    type Events = Arc<Mutex<Vec<String>>>;

    fn events() -> Events {
        Arc::new(Mutex::new(Vec::new()))
    }

    fn recorded(events: &Events) -> Vec<String> {
        events.lock().unwrap().clone()
    }

    #[derive(Clone, Copy)]
    enum RunBehavior {
        /// Default `run`: returns `Ok` immediately.
        Instant,
        /// Finishes some work, then returns `Ok`.
        Delayed,
        /// Returns an error.
        Fail,
        /// Panics.
        Panic,
        /// Never returns (server-style).
        Forever,
    }

    #[derive(Clone, Copy)]
    struct Spec {
        name: &'static str,
        /// `None` → provider has no config; `Some(valid)` → config validity.
        config: Option<bool>,
        init_ok: bool,
        run: RunBehavior,
        shutdown_sleep: Option<Duration>,
    }

    impl Spec {
        const fn named(name: &'static str) -> Self {
            Self {
                name,
                config: None,
                init_ok: true,
                run: RunBehavior::Instant,
                shutdown_sleep: None,
            }
        }

        const fn with_run(mut self, run: RunBehavior) -> Self {
            self.run = run;
            self
        }

        const fn with_config(mut self, valid: bool) -> Self {
            self.config = Some(valid);
            self
        }

        const fn with_failing_init(mut self) -> Self {
            self.init_ok = false;
            self
        }

        const fn with_slow_shutdown(mut self, sleep: Duration) -> Self {
            self.shutdown_sleep = Some(sleep);
            self
        }
    }

    struct TestConfig {
        valid: bool,
    }

    impl Config for TestConfig {
        fn validate(&self) -> Result<(), ConfigError> {
            if self.valid {
                Ok(())
            } else {
                Err(ConfigError::MissingField("field"))
            }
        }
    }

    struct TestProvider {
        spec: Spec,
        config: Option<TestConfig>,
        events: Events,
    }

    fn provider(spec: Spec, events: &Events) -> Arc<TestProvider> {
        Arc::new(TestProvider {
            config: spec.config.map(|valid| TestConfig { valid }),
            spec,
            events: Arc::clone(events),
        })
    }

    impl TestProvider {
        fn record(&self, phase: &str) {
            self.events
                .lock()
                .unwrap()
                .push(format!("{phase}:{}", self.spec.name));
        }
    }

    #[async_trait]
    impl Provider for TestProvider {
        fn name(&self) -> &str {
            self.spec.name
        }

        fn config(&self) -> Option<&dyn Config> {
            self.config.as_ref().map(|config| config as &dyn Config)
        }

        async fn init(&self) -> Result<(), BoxError> {
            self.record("init");
            if self.spec.init_ok {
                Ok(())
            } else {
                Err("init failed".into())
            }
        }

        async fn run(&self) -> Result<(), BoxError> {
            self.record("run");
            match self.spec.run {
                RunBehavior::Instant => Ok(()),
                RunBehavior::Delayed => {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                    Ok(())
                }
                RunBehavior::Fail => Err("run failed".into()),
                RunBehavior::Panic => panic!("run panicked"),
                RunBehavior::Forever => {
                    std::future::pending::<()>().await;
                    Ok(())
                }
            }
        }

        async fn shutdown(&self) -> Result<(), BoxError> {
            self.record("shutdown");
            if let Some(sleep) = self.spec.shutdown_sleep {
                tokio::time::sleep(sleep).await;
            }
            Ok(())
        }

        async fn close(&self) -> Result<(), BoxError> {
            self.record("close");
            Ok(())
        }
    }

    //##########################################################################
    // Expected outcomes for table entries
    //##########################################################################

    #[derive(Debug)]
    enum Expect {
        Ok,
        Duplicate(&'static str),
        InvalidConfig(&'static str),
        FailedInit(&'static str),
        FailedRun(&'static str),
        PanickedRun(&'static str),
    }

    fn assert_outcome(case: &str, result: &Result<(), StackError>, expected: &Expect) {
        match (expected, result) {
            (Expect::Ok, Ok(())) => {}
            (Expect::Duplicate(want), Err(StackError::DuplicateProvider(name))) => {
                assert_eq!(name, want, "case '{case}': wrong provider name");
            }
            (Expect::InvalidConfig(want), Err(StackError::Config { provider, .. }))
            | (Expect::FailedInit(want), Err(StackError::Init { provider, .. }))
            | (Expect::FailedRun(want), Err(StackError::Run { provider, .. }))
            | (Expect::PanickedRun(want), Err(StackError::RunPanic { provider, .. })) => {
                assert_eq!(provider, want, "case '{case}': wrong provider name");
            }
            (expected, result) => {
                panic!("case '{case}': expected {expected:?}, got {result:?}");
            }
        }
    }

    async fn build_stack(specs: &[Spec], events: &Events) -> Result<Stack, StackError> {
        let mut stack = Stack::new();
        for spec in specs {
            stack.add(provider(*spec, events)).await?;
        }
        Ok(stack)
    }

    //##########################################################################
    // Add phase
    //##########################################################################

    #[tokio::test]
    async fn add_table() {
        let cases: &[(&str, &[Spec], Expect)] = &[
            ("single provider", &[Spec::named("a")], Expect::Ok),
            (
                "multiple distinct providers",
                &[Spec::named("a"), Spec::named("b"), Spec::named("c")],
                Expect::Ok,
            ),
            (
                "valid config accepted",
                &[Spec::named("a").with_config(true)],
                Expect::Ok,
            ),
            (
                "duplicate name rejected",
                &[Spec::named("a"), Spec::named("a")],
                Expect::Duplicate("a"),
            ),
            (
                "duplicate after other providers rejected",
                &[Spec::named("a"), Spec::named("b"), Spec::named("a")],
                Expect::Duplicate("a"),
            ),
            (
                "invalid config rejected",
                &[Spec::named("bad").with_config(false)],
                Expect::InvalidConfig("bad"),
            ),
            (
                "invalid config among valid providers rejected",
                &[Spec::named("a"), Spec::named("bad").with_config(false)],
                Expect::InvalidConfig("bad"),
            ),
            (
                "failing init rejected",
                &[Spec::named("bad").with_failing_init()],
                Expect::FailedInit("bad"),
            ),
            (
                "failing init among valid providers rejected",
                &[Spec::named("a"), Spec::named("bad").with_failing_init()],
                Expect::FailedInit("bad"),
            ),
        ];

        for (case, specs, expected) in cases {
            let events = events();
            let result = build_stack(specs, &events).await.map(|_| ());
            assert_outcome(case, &result, expected);
        }
    }

    #[tokio::test]
    async fn add_skips_init_when_config_invalid() {
        let events = events();
        let result = build_stack(&[Spec::named("bad").with_config(false)], &events)
            .await
            .map(|_| ());

        assert_outcome("invalid config", &result, &Expect::InvalidConfig("bad"));
        assert!(
            recorded(&events).is_empty(),
            "init must not run when config validation fails"
        );
    }

    #[tokio::test]
    async fn add_blocked_when_not_in_created_state() {
        let mut stack = Stack::new();
        stack
            .state
            .store(StackState::Running as u8, Ordering::Release);

        let result = stack.add(provider(Spec::named("late"), &events())).await;
        assert!(matches!(
            result,
            Err(StackError::InvalidState {
                expected: StackState::Created,
                actual: StackState::Running,
            })
        ));
    }

    //##########################################################################
    // Run phase
    //##########################################################################

    #[tokio::test]
    async fn run_table() {
        let cases: &[(&str, &[Spec], Expect)] = &[
            ("no providers closes cleanly", &[], Expect::Ok),
            (
                "instant runs close cleanly",
                &[Spec::named("a"), Spec::named("b")],
                Expect::Ok,
            ),
            (
                "delayed runs close cleanly",
                &[
                    Spec::named("a").with_run(RunBehavior::Delayed),
                    Spec::named("b").with_run(RunBehavior::Delayed),
                ],
                Expect::Ok,
            ),
            (
                "mixed instant and delayed close cleanly",
                &[
                    Spec::named("a"),
                    Spec::named("b").with_run(RunBehavior::Delayed),
                ],
                Expect::Ok,
            ),
            (
                "failing run tears down with error",
                &[Spec::named("a").with_run(RunBehavior::Fail)],
                Expect::FailedRun("a"),
            ),
            (
                "panicking run tears down with error",
                &[Spec::named("a").with_run(RunBehavior::Panic)],
                Expect::PanickedRun("a"),
            ),
            (
                "failure among healthy providers tears down",
                &[
                    Spec::named("server").with_run(RunBehavior::Forever),
                    Spec::named("task").with_run(RunBehavior::Delayed),
                    Spec::named("bad").with_run(RunBehavior::Fail),
                ],
                Expect::FailedRun("bad"),
            ),
            (
                "panic among healthy providers tears down",
                &[
                    Spec::named("server").with_run(RunBehavior::Forever),
                    Spec::named("bad").with_run(RunBehavior::Panic),
                ],
                Expect::PanickedRun("bad"),
            ),
        ];

        for (case, specs, expected) in cases {
            let events = events();
            let stack = build_stack(specs, &events)
                .await
                .unwrap_or_else(|error| panic!("case '{case}': add failed: {error}"));

            let result = stack.run().await;
            assert_outcome(case, &result, expected);
        }
    }

    #[tokio::test]
    async fn teardown_runs_in_reverse_order_for_all_providers() {
        let events = events();
        let stack = build_stack(
            &[
                Spec::named("a").with_run(RunBehavior::Forever),
                Spec::named("b").with_run(RunBehavior::Delayed),
                Spec::named("c").with_run(RunBehavior::Fail),
            ],
            &events,
        )
        .await
        .unwrap();

        let result = stack.run().await;
        assert_outcome("teardown order", &result, &Expect::FailedRun("c"));

        let teardown: Vec<String> = recorded(&events)
            .into_iter()
            .filter(|event| event.starts_with("shutdown:") || event.starts_with("close:"))
            .collect();
        assert_eq!(
            teardown,
            [
                "shutdown:c",
                "shutdown:b",
                "shutdown:a",
                "close:c",
                "close:b",
                "close:a",
            ],
            "every provider must be torn down exactly once, in reverse order"
        );
    }

    //##########################################################################
    // State machine: single-use transitions, double calls, illegal jumps
    //##########################################################################

    #[test]
    fn transition_table() {
        let cases: &[(&str, StackState, StackState, StackState, bool)] = &[
            // (case, current, from, to, should_succeed)
            (
                "created to running",
                StackState::Created,
                StackState::Created,
                StackState::Running,
                true,
            ),
            (
                "running to shutting down",
                StackState::Running,
                StackState::Running,
                StackState::ShuttingDown,
                true,
            ),
            (
                "shutting down to closed",
                StackState::ShuttingDown,
                StackState::ShuttingDown,
                StackState::Closed,
                true,
            ),
            (
                "second run blocked",
                StackState::Running,
                StackState::Created,
                StackState::Running,
                false,
            ),
            (
                "second shutdown blocked",
                StackState::ShuttingDown,
                StackState::Running,
                StackState::ShuttingDown,
                false,
            ),
            (
                "second close blocked",
                StackState::Closed,
                StackState::ShuttingDown,
                StackState::Closed,
                false,
            ),
            (
                "shutdown before run blocked",
                StackState::Created,
                StackState::Running,
                StackState::ShuttingDown,
                false,
            ),
            (
                "close before shutdown blocked",
                StackState::Running,
                StackState::ShuttingDown,
                StackState::Closed,
                false,
            ),
            (
                "closed is terminal",
                StackState::Closed,
                StackState::Created,
                StackState::Running,
                false,
            ),
        ];

        for (case, current, from, to, should_succeed) in cases {
            let stack = Stack::new();
            stack.state.store(*current as u8, Ordering::Release);

            let result = stack.transition(*from, *to);
            assert_eq!(
                result.is_ok(),
                *should_succeed,
                "case '{case}': unexpected transition result: {result:?}"
            );

            let expected_state = if *should_succeed { *to } else { *current };
            assert_eq!(
                stack.state(),
                expected_state,
                "case '{case}': state must only change on success"
            );
        }
    }

    #[tokio::test]
    async fn shutdown_and_close_execute_exactly_once() {
        let events = events();
        let stack = build_stack(&[Spec::named("a"), Spec::named("b")], &events)
            .await
            .unwrap();
        stack
            .state
            .store(StackState::Running as u8, Ordering::Release);

        stack.shutdown().await;
        stack.shutdown().await; // second call must be a no-op
        stack.close().await;
        stack.close().await; // second call must be a no-op

        let teardown: Vec<String> = recorded(&events)
            .into_iter()
            .filter(|event| event.starts_with("shutdown:") || event.starts_with("close:"))
            .collect();
        assert_eq!(
            teardown,
            ["shutdown:b", "shutdown:a", "close:b", "close:a"],
            "double calls must not re-run provider teardown"
        );
        assert_eq!(stack.state(), StackState::Closed);
    }

    #[tokio::test]
    async fn close_skipped_unless_shutting_down() {
        let events = events();
        let stack = build_stack(&[Spec::named("a")], &events).await.unwrap();
        stack
            .state
            .store(StackState::Running as u8, Ordering::Release);

        stack.close().await;

        assert_eq!(stack.state(), StackState::Running);
        assert!(
            !recorded(&events).iter().any(|e| e.starts_with("close:")),
            "close must not reach providers before shutdown"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn shutdown_timeout_is_enforced_per_provider() {
        let events = events();
        let mut stack = Stack::new().with_shutdown_timeout(Duration::from_millis(50));
        stack
            .add(provider(
                Spec::named("slow").with_slow_shutdown(Duration::from_hours(1)),
                &events,
            ))
            .await
            .unwrap();
        stack
            .add(provider(
                Spec::named("slower").with_slow_shutdown(Duration::from_hours(1)),
                &events,
            ))
            .await
            .unwrap();
        stack
            .state
            .store(StackState::Running as u8, Ordering::Release);

        let started = tokio::time::Instant::now();
        stack.shutdown().await;

        assert!(
            started.elapsed() < Duration::from_secs(1),
            "shutdown must be cut off by the timeout, not wait out slow providers"
        );
        assert_eq!(stack.state(), StackState::ShuttingDown);
    }

    #[tokio::test]
    async fn state_starts_created() {
        assert_eq!(Stack::new().state(), StackState::Created);
    }

    /// A provider that doesn't override `run` (default: [`NoRun`] marker)
    /// must count as completed, not as a failure.
    #[tokio::test]
    async fn provider_without_run_closes_cleanly() {
        struct Bare;

        #[async_trait]
        impl Provider for Bare {
            fn name(&self) -> &'static str {
                "bare"
            }
        }

        let mut stack = Stack::new();
        stack.add(Arc::new(Bare)).await.unwrap();
        assert!(stack.run().await.is_ok());
    }
}
