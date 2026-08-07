//! Integration test for the stack's exit-signal handling.
//!
//! This binary runs twice. `cargo test` starts it as the orchestrator, which
//! re-executes itself with `RASTACK_SIGNAL_CHILD=1` as a real stack app
//! hosting a server-style provider. Once the child reports it is serving, the
//! orchestrator sends SIGTERM (what `docker stop` / Kubernetes send) and
//! asserts the child tears down gracefully: shutdown, then close, then a
//! clean exit code.

use async_trait::async_trait;
use rastack::{BoxError, Provider, Stack};

const READY: &str = "CHILD_READY";
const SHUTDOWN: &str = "CHILD_SHUTDOWN";
const CLOSE: &str = "CHILD_CLOSE";

fn main() {
    if std::env::var_os("RASTACK_SIGNAL_CHILD").is_some() {
        child();
    } else {
        orchestrate();
    }
}

//##############################################################################
// Child: a real app on the stack, serving until killed
//##############################################################################

/// Prints a marker line to stdout and flushes, so the orchestrator sees it
/// immediately through the pipe.
fn emit(marker: &str) {
    use std::io::Write;

    let mut stdout = std::io::stdout().lock();
    writeln!(stdout, "{marker}").expect("failed to write marker");
    stdout.flush().expect("failed to flush marker");
}

/// Server-style provider: `run` holds its future open until the stack
/// tears it down.
struct ServerLike;

#[async_trait]
impl Provider for ServerLike {
    fn name(&self) -> &'static str {
        "server-like"
    }

    async fn run(&self) -> Result<(), BoxError> {
        emit(READY);
        std::future::pending::<()>().await;
        Ok(())
    }

    async fn shutdown(&self) -> Result<(), BoxError> {
        emit(SHUTDOWN);
        Ok(())
    }

    async fn close(&self) -> Result<(), BoxError> {
        emit(CLOSE);
        Ok(())
    }
}

fn child() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("failed to build tokio runtime");

    let result = runtime.block_on(async {
        let mut stack = Stack::new();
        stack.add(std::sync::Arc::new(ServerLike)).await?;
        stack.run().await
    });

    if let Err(error) = result {
        eprintln!("child stack failed: {error}");
        std::process::exit(1);
    }
}

//##############################################################################
// Orchestrator: spawn the child, signal it, assert graceful teardown
//##############################################################################

#[cfg(unix)]
fn orchestrate() {
    use std::io::{BufRead, BufReader};
    use std::process::{Command, Stdio};
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    let exe = std::env::current_exe().expect("failed to resolve current executable");
    let mut child = Command::new(exe)
        .env("RASTACK_SIGNAL_CHILD", "1")
        .stdout(Stdio::piped())
        .spawn()
        .expect("failed to spawn child app");

    let stdout = child.stdout.take().expect("child stdout must be piped");
    let (sender, receiver) = mpsc::channel::<String>();
    let reader = std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            if sender.send(line).is_err() {
                break;
            }
        }
    });

    // 1. Wait until the provider's `run` is actually serving.
    wait_for_marker(&receiver, READY, Duration::from_secs(20), &mut child);

    // 2. Deliver a real SIGTERM to the child process.
    let kill_status = Command::new("kill")
        .args(["-TERM", &child.id().to_string()])
        .status()
        .expect("failed to run kill");
    assert!(kill_status.success(), "kill -TERM failed: {kill_status:?}");

    // 3. The child must exit cleanly within the deadline.
    let deadline = Instant::now() + Duration::from_secs(20);
    let exit = loop {
        if let Some(status) = child.try_wait().expect("failed to poll child") {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            panic!("child did not exit after SIGTERM; is signal handling broken?");
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    assert!(
        exit.success(),
        "child exited with {exit:?}, expected a clean exit after SIGTERM"
    );

    // 4. Teardown must have reached the provider, shutdown before close.
    reader.join().expect("reader thread panicked");
    let teardown: Vec<String> = receiver.try_iter().collect();
    let shutdown_at = teardown.iter().position(|line| line == SHUTDOWN);
    let close_at = teardown.iter().position(|line| line == CLOSE);
    assert!(
        shutdown_at.is_some(),
        "provider shutdown never ran; child output: {teardown:?}"
    );
    assert!(
        close_at.is_some(),
        "provider close never ran; child output: {teardown:?}"
    );
    assert!(
        shutdown_at < close_at,
        "shutdown must run before close; child output: {teardown:?}"
    );

    println!("signal integration test passed: SIGTERM -> shutdown -> close -> clean exit");
}

#[cfg(unix)]
fn wait_for_marker(
    receiver: &std::sync::mpsc::Receiver<String>,
    marker: &str,
    timeout: std::time::Duration,
    child: &mut std::process::Child,
) {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            let _ = child.kill();
            panic!("timed out waiting for '{marker}' from the child app");
        }
        match receiver.recv_timeout(remaining) {
            Ok(line) if line == marker => return,
            Ok(_) => {}
            Err(_) => {
                let _ = child.kill();
                panic!("child ended before printing '{marker}'");
            }
        }
    }
}

#[cfg(not(unix))]
fn orchestrate() {
    // Windows console control events cannot be delivered to a specific child
    // process from a test; the unix run covers the shared teardown logic.
    println!("signal integration test skipped: requires unix signals");
}
