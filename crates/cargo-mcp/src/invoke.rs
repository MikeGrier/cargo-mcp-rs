// Copyright (c) Michael Grier. All rights reserved.

//! Subprocess invocation of `cargo`.
//!
//! Every MCP tool call spawns `cargo` with the appropriate subcommand and
//! flags, capturing stdout and stderr. stdin is always closed to prevent
//! interactive prompts or hangs.
//!
//! ## Cancellation
//!
//! Call [`set_cancel_token`] with the `Arc<AtomicBool>` returned by
//! [`crate::line_reader::LineReader::register_cancel`] before invoking any
//! cargo subprocess. The subprocess functions poll the token after each chunk
//! of output and kill the child process if it is set. Call
//! `set_cancel_token(None)` once the tool call returns.
//!
//! ## Environment
//!
//! - `CARGO_TERM_COLOR=never` — suppresses ANSI colour codes that would be
//!   noise in MCP text responses.
//! - `NO_COLOR=1` — belt-and-suspenders colour suppression for any tool in
//!   the Cargo pipeline that respects the informal `NO_COLOR` convention.

use std::cell::RefCell;
use std::io::{BufRead, BufReader, Read};
use std::process::{Command, Stdio};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::thread;
use std::time::Duration;

// ── cancellation ──────────────────────────────────────────────────────────────

/// Error returned when a cargo operation is cancelled by the client.
#[derive(Debug)]
pub struct CancelledError;

impl std::fmt::Display for CancelledError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Operation cancelled by client request.")
    }
}

impl std::error::Error for CancelledError {}

thread_local! {
    /// The cancel token for the cargo operation currently running on this thread.
    /// Set by [`set_cancel_token`]; polled inside the subprocess runners.
    static CANCEL_TOKEN: RefCell<Option<Arc<AtomicBool>>> = const { RefCell::new(None) };
}

/// Install (or clear) the cancel token for the current thread.
///
/// Pass `Some(token)` before spawning a cargo subprocess and `None` after it
/// returns. The subprocess runners poll this token and kill the child process
/// if it is set to `true`.
pub fn set_cancel_token(token: Option<Arc<AtomicBool>>) {
    CANCEL_TOKEN.with(|c| *c.borrow_mut() = token);
}

/// Returns `true` if the current thread's cancel token has been set.
fn is_cancelled() -> bool {
    CANCEL_TOKEN.with(|c| {
        c.borrow()
            .as_ref()
            .map(|t| t.load(Ordering::Acquire))
            .unwrap_or(false)
    })
}

// ── output types ──────────────────────────────────────────────────────────────

/// The result of a completed Cargo invocation.
pub struct CargoOutput {
    /// Content written to stdout.
    pub stdout: String,
    /// Content written to stderr (progress messages, diagnostics in text mode).
    pub stderr: String,
    /// The process exit code (or -1 if the process was killed by a signal).
    pub exit_code: i32,
}

// ── subprocess runners ────────────────────────────────────────────────────────

/// Run `cargo <args>`, calling `on_stdout_line` for each stdout line as it
/// arrives, and return the complete output after the process exits.
///
/// Stderr is drained in a background thread to prevent pipe-buffer deadlock
/// when the process produces large amounts of output on both streams.
/// The `on_stdout_line` callback is invoked on the calling thread only.
///
/// If the thread-local cancel token is set mid-run, the child is killed and
/// [`CancelledError`] is returned.
///
/// Returns a [`CargoOutput`] on success (even if cargo itself reports errors —
/// the exit code distinguishes success from failure). Returns `Err` only for
/// OS-level spawn failures or cancellation.
pub fn run_cargo_streaming(
    args: &[&str],
    working_dir: Option<&str>,
    on_stdout_line: &mut dyn FnMut(&str),
) -> Result<CargoOutput, Box<dyn std::error::Error>> {
    let mut cmd = Command::new("cargo");
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("CARGO_TERM_COLOR", "never")
        .env("NO_COLOR", "1");

    if let Some(dir) = working_dir {
        cmd.current_dir(dir);
    }

    let mut child = cmd.spawn()?;
    let stdout_pipe = child.stdout.take().expect("stdout is piped");
    let stderr_pipe = child.stderr.take().expect("stderr is piped");

    // Drain stderr on a background thread to avoid deadlock when the stdout
    // pipe buffer fills while stderr is also accumulating.
    let stderr_thread = thread::spawn(move || -> String {
        let mut buf = String::new();
        let _ = BufReader::new(stderr_pipe).read_to_string(&mut buf);
        buf
    });

    // Stream stdout line by line on the calling thread.
    let mut stdout_buf = String::new();
    let mut cancelled = false;
    for line in BufReader::new(stdout_pipe).lines() {
        match line {
            Ok(l) => {
                on_stdout_line(&l);
                stdout_buf.push_str(&l);
                stdout_buf.push('\n');
            }
            Err(_) => break,
        }
        if is_cancelled() {
            cancelled = true;
            break;
        }
    }

    if cancelled {
        let _ = child.kill();
        let _ = child.wait();
        let _ = stderr_thread.join();
        return Err(Box::new(CancelledError));
    }

    let stderr_buf = stderr_thread.join().unwrap_or_default();
    let status = child.wait()?;

    Ok(CargoOutput {
        stdout: stdout_buf,
        stderr: stderr_buf,
        exit_code: status.code().unwrap_or(-1),
    })
}

/// Run `cargo <args>` and capture the complete output without streaming.
///
/// Convenience wrapper around [`run_cargo_streaming`] for call sites that do
/// not need incremental progress callbacks.
pub fn run_cargo(
    args: &[&str],
    working_dir: Option<&str>,
) -> Result<CargoOutput, Box<dyn std::error::Error>> {
    run_cargo_streaming(args, working_dir, &mut |_| {})
}

/// Run `cargo <args>`, piping stdout **directly** into `dest_file` at the OS
/// level instead of buffering it in memory.
///
/// Use this for commands whose stdout can be very large (e.g. `cargo metadata`
/// in a workspace with thousands of transitive dependencies). Because the OS
/// plumbs the pipe straight to the file, the Rust process's heap is never
/// charged for the output.
///
/// If the thread-local cancel token is set mid-run, the child is killed and
/// [`CancelledError`] is returned.
///
/// `CargoOutput::stdout` is always empty when this function is used; only
/// `stderr` and `exit_code` are meaningful in the returned value.
pub fn run_cargo_to_file(
    args: &[&str],
    working_dir: Option<&str>,
    dest_file: std::fs::File,
) -> Result<CargoOutput, Box<dyn std::error::Error>> {
    let mut cmd = Command::new("cargo");
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::from(dest_file)) // OS-level pipe → file, no heap buffer
        .stderr(Stdio::piped())
        .env("CARGO_TERM_COLOR", "never")
        .env("NO_COLOR", "1");

    if let Some(dir) = working_dir {
        cmd.current_dir(dir);
    }

    let mut child = cmd.spawn()?;
    let stderr_pipe = child.stderr.take().expect("stderr is piped");

    // Drain stderr on a background thread to avoid deadlock when stdout
    // fills the pipe buffer while stderr is also accumulating.
    let stderr_thread = thread::spawn(move || -> String {
        let mut buf = String::new();
        let _ = BufReader::new(stderr_pipe).read_to_string(&mut buf);
        buf
    });

    // Poll for completion, checking the cancel token every 50 ms.
    let status = loop {
        match child.try_wait()? {
            Some(s) => break s,
            None => {
                if is_cancelled() {
                    let _ = child.kill();
                    let _ = child.wait();
                    let _ = stderr_thread.join();
                    return Err(Box::new(CancelledError));
                }
                thread::sleep(Duration::from_millis(50));
            }
        }
    };

    let stderr_buf = stderr_thread.join().unwrap_or_default();

    Ok(CargoOutput {
        stdout: String::new(), // nothing buffered; caller reads from dest_file
        stderr: stderr_buf,
        exit_code: status.code().unwrap_or(-1),
    })
}
