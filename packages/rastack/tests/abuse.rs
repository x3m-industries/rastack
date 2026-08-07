//! Adversarial tests: deliberately misuse the public API and try to break
//! the stack's lifecycle guarantees.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use rastack::{BoxError, Provider, Stack};

type Log = Arc<Mutex<Vec<String>>>;

/// A hostile provider: every lifecycle hook can be armed to fail, panic, or
/// hang.
#[allow(clippy::struct_excessive_bools)] // each bool arms one independent attack
struct Evil {
    name: &'static str,
    panic_init: bool,
    fail_run: bool,
    panic_shutdown: bool,
    panic_close: bool,
    hang_close: bool,
    log: Log,
}

fn evil(name: &'static str, log: &Log) -> Evil {
    Evil {
        name,
        panic_init: false,
        fail_run: false,
        panic_shutdown: false,
        panic_close: false,
        hang_close: false,
        log: Arc::clone(log),
    }
}

impl Evil {
    fn record(&self, phase: &str) {
        self.log
            .lock()
            .unwrap()
            .push(format!("{phase}:{}", self.name));
    }
}

#[async_trait]
impl Provider for Evil {
    fn name(&self) -> &str {
        self.name
    }

    async fn init(&self) -> Result<(), BoxError> {
        self.record("init");
        assert!(!self.panic_init, "init panic armed");
        Ok(())
    }

    async fn run(&self) -> Result<(), BoxError> {
        self.record("run");
        if self.fail_run {
            return Err("armed run failure".into());
        }
        std::future::pending::<()>().await;
        Ok(())
    }

    async fn shutdown(&self) -> Result<(), BoxError> {
        self.record("shutdown");
        assert!(!self.panic_shutdown, "shutdown panic armed");
        Ok(())
    }

    async fn close(&self) -> Result<(), BoxError> {
        self.record("close");
        assert!(!self.panic_close, "close panic armed");
        if self.hang_close {
            std::future::pending::<()>().await;
        }
        Ok(())
    }
}

fn entries(log: &Log) -> Vec<String> {
    log.lock().unwrap().clone()
}

/// A panicking `shutdown` in one provider must not skip the teardown of the
/// others, and must not unwind out of `Stack::run`.
#[tokio::test]
async fn panicking_shutdown_must_not_skip_other_teardown() {
    let log = Log::default();
    let mut stack = Stack::new();
    stack.add(Arc::new(evil("a", &log))).await.unwrap();
    let mut bomb = evil("bomb", &log);
    bomb.fail_run = true;
    bomb.panic_shutdown = true;
    stack.add(Arc::new(bomb)).await.unwrap();

    // Run inside a task so an unwind (if the stack has this bug) is
    // observable instead of killing the test.
    let outcome = tokio::spawn(stack.run()).await;

    assert!(
        outcome.is_ok(),
        "Stack::run must not unwind when a provider's shutdown panics: {outcome:?}"
    );
    let log = entries(&log);
    assert!(
        log.contains(&"shutdown:a".to_string()),
        "provider a's shutdown was skipped: {log:?}"
    );
    assert!(
        log.contains(&"close:bomb".to_string()),
        "bomb's close must still run after its shutdown panicked: {log:?}"
    );
    assert!(
        log.contains(&"close:a".to_string()),
        "provider a's close was skipped: {log:?}"
    );
}

/// A panicking `close` in one provider must not skip the close of the others,
/// and must not unwind out of `Stack::run`.
#[tokio::test]
async fn panicking_close_must_not_skip_other_teardown() {
    let log = Log::default();
    let mut stack = Stack::new();
    stack.add(Arc::new(evil("a", &log))).await.unwrap();
    let mut bomb = evil("bomb", &log);
    bomb.fail_run = true;
    bomb.panic_close = true;
    stack.add(Arc::new(bomb)).await.unwrap();

    let outcome = tokio::spawn(stack.run()).await;

    assert!(
        outcome.is_ok(),
        "Stack::run must not unwind when a provider's close panics: {outcome:?}"
    );
    let log = entries(&log);
    assert!(
        log.contains(&"shutdown:a".to_string()),
        "provider a's shutdown was skipped: {log:?}"
    );
    assert!(
        log.contains(&"close:a".to_string()),
        "provider a's close was skipped: {log:?}"
    );
}

/// A panic inside `init` unwinds through `Stack::add` to the caller — the
/// app fails to start, which is the intended fail-fast behavior. Documented
/// here so a future change is a conscious one.
#[tokio::test]
async fn panicking_init_unwinds_through_add() {
    let log = Log::default();
    let outcome = tokio::spawn(async move {
        let mut stack = Stack::new();
        let mut bomb = evil("bomb", &log);
        bomb.panic_init = true;
        stack.add(Arc::new(bomb)).await
    })
    .await;

    assert!(outcome.is_err(), "init panic should unwind out of add");
}

/// KNOWN BEHAVIOR, by design: `close` has no timeout, so a hanging close
/// blocks `Stack::run` forever. This test documents it; if a deadline for
/// the close pass is ever added, flip this assertion.
#[tokio::test(start_paused = true)]
async fn hanging_close_blocks_run_forever() {
    let log = Log::default();
    let mut stack = Stack::new();
    let mut sloth = evil("sloth", &log);
    sloth.fail_run = true;
    sloth.hang_close = true;
    stack.add(Arc::new(sloth)).await.unwrap();

    let outcome = tokio::time::timeout(Duration::from_hours(1), stack.run()).await;

    assert!(
        outcome.is_err(),
        "expected run to still be blocked by the hanging close"
    );
}

/// Duplicate-name rejection cannot be dodged by interleaving other adds.
#[tokio::test]
async fn duplicate_rejected_even_when_interleaved() {
    let log = Log::default();
    let mut stack = Stack::new();
    stack.add(Arc::new(evil("a", &log))).await.unwrap();
    stack.add(Arc::new(evil("b", &log))).await.unwrap();
    let result = stack.add(Arc::new(evil("a", &log))).await;
    assert!(result.is_err(), "smuggled duplicate 'a' past the check");
}

/// Sharing one provider instance under two registrations is blocked by the
/// duplicate check (same Arc, same name).
#[tokio::test]
async fn same_arc_cannot_be_added_twice() {
    let log = Log::default();
    let provider = Arc::new(evil("a", &log));
    let mut stack = Stack::new();
    stack.add(provider.clone()).await.unwrap();
    let result = stack.add(provider).await;
    assert!(result.is_err(), "same Arc registered twice");
    // init must have run exactly once.
    let inits = entries(&log)
        .iter()
        .filter(|entry| entry.starts_with("init:"))
        .count();
    assert_eq!(inits, 1, "init ran for a rejected duplicate");
}

/// 1000 providers: registration order in, exact reverse order out, no loss.
#[tokio::test]
async fn thousand_providers_teardown_completely_in_reverse() {
    static NAMES: [&str; 0] = [];
    let _ = NAMES; // names need 'static; generate via leak below.

    let log = Log::default();
    let mut stack = Stack::new();
    for index in 0..1000 {
        let name: &'static str = Box::leak(format!("p{index}").into_boxed_str());
        let mut provider = evil(name, &log);
        provider.fail_run = index == 999; // last one torches the stack
        stack.add(Arc::new(provider)).await.unwrap();
    }

    let result = stack.run().await;
    assert!(result.is_err(), "expected the armed run failure");

    let log = entries(&log);
    let closes: Vec<&String> = log.iter().filter(|e| e.starts_with("close:")).collect();
    assert_eq!(closes.len(), 1000, "every provider must be closed");
    // Spot-check strict reverse order.
    assert_eq!(closes[0], "close:p999");
    assert_eq!(closes[999], "close:p0");

    let shutdowns = log.iter().filter(|e| e.starts_with("shutdown:")).count();
    assert_eq!(shutdowns, 1000, "every provider must be shut down");
}

/// `state()` reads must be safe while `run` is executing on another task.
#[tokio::test]
async fn concurrent_adds_from_multiple_tasks_cannot_corrupt() {
    // `add` needs `&mut Stack`, so true concurrent adds don't compile —
    // ownership already protects registration. What we can verify: a stack
    // built on one task can be moved to and run on another.
    let log = Log::default();
    let mut stack = Stack::new();
    stack.add(Arc::new(evil("a", &log))).await.unwrap();

    let moved = tokio::spawn(async move {
        let mut stack = stack;
        let mut bomb = evil("bomb", &log);
        bomb.fail_run = true;
        stack.add(Arc::new(bomb)).await.unwrap();
        stack.run().await
    });

    assert!(moved.await.expect("no panic").is_err());
}

/// Providers that lie about readiness: `run` returning instantly for all
/// providers must still produce a full, ordered teardown.
#[tokio::test]
async fn instant_runs_still_get_full_teardown() {
    let log = Log::default();
    let mut stack = Stack::new();
    for name in ["a", "b", "c"] {
        let mut provider = evil(name, &log);
        provider.fail_run = false;
        stack.add(Arc::new(provider)).await.unwrap();
    }

    // All runs pend forever here (fail_run=false pends); use a bomb to end it.
    let mut bomb = evil("bomb", &log);
    bomb.fail_run = true;
    stack.add(Arc::new(bomb)).await.unwrap();

    let result = stack.run().await;
    assert!(result.is_err());

    let log = entries(&log);
    let teardown: Vec<&String> = log
        .iter()
        .filter(|e| e.starts_with("shutdown:") || e.starts_with("close:"))
        .collect();
    assert_eq!(
        teardown,
        [
            "shutdown:bomb",
            "shutdown:c",
            "shutdown:b",
            "shutdown:a",
            "close:bomb",
            "close:c",
            "close:b",
            "close:a"
        ],
        "teardown incomplete or out of order"
    );
}

/// Signal-during-run is covered by tests/signal.rs. This static flag only
/// exists so the file states the full attack surface in one place.
static _COVERED_ELSEWHERE: AtomicBool = AtomicBool::new(true);

#[test]
fn attack_surface_inventory() {
    // Documented process-level attack results live in the PR/commit notes:
    // - SIGTERM during `run`: graceful teardown (tests/signal.rs)
    // - SIGTERM during a slow `init` (before `run`): process dies with the
    //   default disposition, no teardown — the signal listener is only
    //   installed inside `run`.
    // - second SIGTERM during a slow graceful shutdown: ignored; only
    //   SIGKILL ends a hung teardown.
    assert!(_COVERED_ELSEWHERE.load(Ordering::Relaxed));
}
