// Copyright (c) Michael Grier. All rights reserved.
//
// Cross-platform integration tests for the timeout and cancellation
// paths in `invoke::run_cargo_streaming_with_timeout`.
//
// Strategy: build the `rm-sleeper` helper (a tiny binary that prints
// `STARTED <pid>` and then sleeps for 600s, ignoring all args), point
// the `CARGO` env var at it, and drive the public invoke API. Because
// `resolve_cargo_binary` consults `CARGO` first, the sleeper is the
// process that gets spawned in place of `cargo`. If the timeout /
// cancellation paths are wired up correctly, the call returns the
// expected error variant well before the 600s sleep would naturally
// elapse.
//
// `cargo-mcp` is a binary-only crate, so the modules under test are
// mounted at the test crate's root via `#[path]` — the same pattern
// used by `rm_end_to_end.rs`.

#[path = "../src/busy_files.rs"]
#[allow(dead_code)]
mod busy_files;
#[path = "../src/invoke.rs"]
#[allow(dead_code)]
mod invoke;
#[path = "../src/rm/mod.rs"]
#[allow(dead_code)]
mod rm;

use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// `CARGO` is process-global; serialize the two tests that mutate it.
static CARGO_ENV_LOCK: Mutex<()> = Mutex::new(());

/// Returns true if a process with `pid` is still live (running or a
/// zombie that hasn't been reaped). Used to verify that the timeout /
/// cancellation paths actually terminated the spawned sleeper.
#[cfg(unix)]
fn process_is_alive(pid: u32) -> bool {
    // `kill(pid, 0)` performs the permission/existence check without
    // delivering a signal. ESRCH means no such process; EPERM means it
    // exists but we lack permission — for our own child either is
    // unlikely once it's been reaped.
    // SAFETY: `kill` with sig=0 has no side effects.
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}

#[cfg(windows)]
fn process_is_alive(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, STILL_ACTIVE};
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    // SAFETY: OpenProcess returns NULL on failure (e.g. PID gone), which
    // we treat as "not alive". On success we query the exit code and
    // always close the handle.
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if h.is_null() {
            return false;
        }
        let mut code: u32 = 0;
        let ok = GetExitCodeProcess(h, &mut code);
        CloseHandle(h);
        ok != 0 && code == STILL_ACTIVE as u32
    }
}

/// Poll `process_is_alive` for up to ~2s to allow the OS a brief moment
/// to finish tearing the child down after `wait()` returns.
fn wait_for_process_exit(pid: u32) -> bool {
    for _ in 0..40 {
        if !process_is_alive(pid) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    !process_is_alive(pid)
}

fn build_sleeper() -> PathBuf {
    let workspace_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();

    let output = Command::new(env!("CARGO"))
        .args([
            "build",
            "-p",
            "rm-test-helpers",
            "--bin",
            "rm-sleeper",
            "--message-format=json-render-diagnostics",
        ])
        .current_dir(&workspace_root)
        .output()
        .expect("invoke cargo to build rm-sleeper");

    assert!(
        output.status.success(),
        "cargo build for rm-sleeper failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    for line in output.stdout.split(|b| *b == b'\n') {
        if line.is_empty() {
            continue;
        }
        let v: serde_json::Value = match serde_json::from_slice(line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if v.get("reason").and_then(|r| r.as_str()) != Some("compiler-artifact") {
            continue;
        }
        if v.pointer("/target/name").and_then(|n| n.as_str()) != Some("rm-sleeper") {
            continue;
        }
        if let Some(exe) = v.get("executable").and_then(|e| e.as_str()) {
            return PathBuf::from(exe);
        }
    }
    panic!("could not find executable for rm-sleeper in cargo build output");
}

/// RAII guard that restores `CARGO` on drop.
struct CargoEnvGuard {
    prev: Option<std::ffi::OsString>,
}

impl CargoEnvGuard {
    fn set(path: &std::path::Path) -> Self {
        let prev = std::env::var_os("CARGO");
        // SAFETY: tests serialize on `CARGO_ENV_LOCK`; no parallel env mutation.
        unsafe { std::env::set_var("CARGO", path) };
        Self { prev }
    }
}

impl Drop for CargoEnvGuard {
    fn drop(&mut self) {
        // SAFETY: tests serialize on `CARGO_ENV_LOCK`.
        unsafe {
            match self.prev.take() {
                Some(v) => std::env::set_var("CARGO", v),
                None => std::env::remove_var("CARGO"),
            }
        }
    }
}

#[test]
fn timeout_returns_timeout_error_and_terminates_subprocess() {
    // Acquire both locks to prevent unit tests from transiently changing
    // CARGO while resolve_cargo_binary runs inside the streaming call.
    let _cargo_g = CARGO_ENV_LOCK.lock().unwrap();
    let _env_g = invoke::TEST_ENV_LOCK.lock().unwrap();
    let sleeper = build_sleeper();
    let _env = CargoEnvGuard::set(&sleeper);

    let mut saw_started = false;
    let mut sleeper_pid: Option<u32> = None;
    let started = Instant::now();
    let result = invoke::run_cargo_streaming_with_timeout(
        &["check"],
        None,
        Some(Duration::from_millis(500)),
        &mut |line| {
            if let Some(rest) = line.strip_prefix("STARTED ") {
                saw_started = true;
                sleeper_pid = rest.trim().parse::<u32>().ok();
            }
        },
    );
    let elapsed = started.elapsed();

    let err = match result {
        Err(e) => e,
        Ok(_) => panic!("expected TimeoutError, got Ok"),
    };
    let timeout_err = err
        .downcast_ref::<invoke::TimeoutError>()
        .unwrap_or_else(|| panic!("expected TimeoutError, got: {err}"));

    assert!(
        saw_started,
        "sleeper never printed STARTED — shim was not actually spawned",
    );
    let pid = sleeper_pid.expect("sleeper STARTED line did not contain a parseable PID");
    assert!(
        wait_for_process_exit(pid),
        "sleeper process (pid={pid}) still alive after timeout path returned",
    );
    // Sanity bound: the call must return well before the sleeper's 600s
    // sleep would naturally complete. Allow generous slack for slow CI.
    assert!(
        elapsed < Duration::from_secs(30),
        "timeout path took too long to return: {elapsed:?}",
    );
    assert!(
        timeout_err.elapsed >= Duration::from_millis(500),
        "reported elapsed ({:?}) shorter than configured timeout",
        timeout_err.elapsed,
    );
}

#[test]
fn cancellation_returns_cancelled_error_and_terminates_subprocess() {
    // Acquire both locks to prevent unit tests from transiently changing
    // CARGO while resolve_cargo_binary runs inside the streaming call.
    let _cargo_g = CARGO_ENV_LOCK.lock().unwrap();
    let _env_g = invoke::TEST_ENV_LOCK.lock().unwrap();
    let sleeper = build_sleeper();
    let _env = CargoEnvGuard::set(&sleeper);

    let token = Arc::new(AtomicBool::new(false));
    invoke::set_cancel_token(Some(token.clone()));

    // Cancel from a sidecar thread, but only AFTER the streaming callback
    // has observed the STARTED line.  A fixed-delay approach (e.g. 500ms)
    // races with spawn latency: the cancel check runs at the *top* of the
    // poll loop, so if the token fires before the STARTED line is dequeued
    // the line is discarded (rx is dropped on the cancel path) and
    // saw_started stays false even though we correctly get CancelledError.
    let (started_tx, started_rx) = std::sync::mpsc::channel::<()>();
    let token_setter = token.clone();
    let setter = std::thread::spawn(move || {
        // Wait until STARTED is confirmed received; 30s is a generous CI cap.
        let _ = started_rx.recv_timeout(Duration::from_secs(30));
        token_setter.store(true, Ordering::Release);
    });

    let mut saw_started = false;
    let mut sleeper_pid: Option<u32> = None;
    let started = Instant::now();
    let result = invoke::run_cargo_streaming_with_timeout(
        &["check"],
        None,
        None, // no wall-clock cap — cancellation is the only exit path
        &mut |line| {
            if let Some(rest) = line.strip_prefix("STARTED ") {
                saw_started = true;
                sleeper_pid = rest.trim().parse::<u32>().ok();
                // Signal the setter thread; it will set the cancel token
                // now that we know the callback has processed STARTED.
                let _ = started_tx.send(());
            }
        },
    );
    let elapsed = started.elapsed();
    setter.join().unwrap();
    invoke::set_cancel_token(None);

    let err = match result {
        Err(e) => e,
        Ok(_) => panic!("expected CancelledError, got Ok"),
    };
    assert!(
        err.downcast_ref::<invoke::CancelledError>().is_some(),
        "expected CancelledError, got: {err}",
    );
    assert!(
        saw_started,
        "sleeper never printed STARTED — shim was not actually spawned",
    );
    let pid = sleeper_pid.expect("sleeper STARTED line did not contain a parseable PID");
    assert!(
        wait_for_process_exit(pid),
        "sleeper process (pid={pid}) still alive after cancel path returned",
    );
    assert!(
        elapsed < Duration::from_secs(30),
        "cancel path took too long to return: {elapsed:?}",
    );
}
