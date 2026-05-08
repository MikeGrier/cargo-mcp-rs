// Copyright (c) Michael Grier. All rights reserved.

//! Subprocess invocation of `cargo`.
//!
//! Every MCP tool call spawns `cargo` with the appropriate subcommand and
//! flags, capturing stdout and stderr. stdin is always closed to prevent
//! interactive prompts or hangs.
//!
//! ## Toolchain resolution
//!
//! The `cargo` binary is located via [`resolve_cargo_binary`] (and `rustc`
//! via [`resolve_rustc_binary`]) using a three-tier strategy: the `CARGO`
//! env var, then the rustup proxy at `$CARGO_HOME/bin/`, then a bare-name
//! `PATH` lookup. Honouring the rustup proxy directly ensures
//! `rust-toolchain.toml` is respected regardless of `PATH` ordering. See
//! [`ResolutionSource`] and the `cargo_diagnostic` MCP tool for diagnostics.
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
//!
//! ## Logging
//!
//! Each invocation writes a one-line `cargo-mcp: invoking <path> ...` record
//! to stderr (which VS Code surfaces in the *MCP Logs: cargo* output channel)
//! so the resolved binary is visible without enabling any extra tracing.

use std::cell::RefCell;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
};
use std::thread;
use std::time::Duration;

// ── retry-on-busy configuration ──────────────────────────────────────────────

/// Whether to retry idempotent cargo invocations that fail with a transient
/// "file in use" / "access denied" / "sharing violation" error.
///
/// Set once at process start by [`set_retry_config`]; defaults to enabled.
static RETRY_ENABLED: AtomicBool = AtomicBool::new(true);

/// Delay between retry attempts, in milliseconds.
static RETRY_DELAY_MS: AtomicU64 = AtomicU64::new(500);

/// Maximum total attempts (initial + retries). Must be at least 1.
static RETRY_MAX_ATTEMPTS: AtomicU32 = AtomicU32::new(3);

/// Configure retry-on-busy behaviour. Called once from `main` after CLI parse.
pub fn set_retry_config(enabled: bool, delay_ms: u64, max_attempts: u32) {
    RETRY_ENABLED.store(enabled, Ordering::Relaxed);
    RETRY_DELAY_MS.store(delay_ms, Ordering::Relaxed);
    RETRY_MAX_ATTEMPTS.store(max_attempts.max(1), Ordering::Relaxed);
}

/// Cargo subcommands whose retry on a transient file-busy error is safe
/// because the operation is idempotent — re-running cannot produce duplicate
/// state changes (no crates published twice, no `Cargo.toml` mutated twice,
/// etc.). Anything not on this list is **never** retried by
/// [`run_cargo_streaming`], regardless of the user's retry settings.
///
/// Notably **excluded** even though they are read-only-ish:
/// - `fix` — modifies source files. A partial first attempt could leave the
///   tree in a half-edited state; we don't want to redo edits on top of that.
/// - `update` — mutates `Cargo.lock`. Same partial-state concern.
///
/// `clean` is included because deleting an already-(partially-)deleted
/// directory tree is a true no-op: the end state matches the goal regardless
/// of how many times it runs.
const IDEMPOTENT_SUBCOMMANDS: &[&str] = &[
    "check", "build", "test", "clippy", "fmt", "doc", "tree", "clean", "metadata",
];

/// Returns `true` iff `args[0]` (the cargo subcommand) is in
/// [`IDEMPOTENT_SUBCOMMANDS`].
fn is_retry_safe(args: &[&str]) -> bool {
    args.first()
        .is_some_and(|sub| IDEMPOTENT_SUBCOMMANDS.contains(sub))
}

/// Is the given combined cargo stderr/stdout indicative of a transient
/// Windows file-locking error that an idempotent retry could clear?
///
/// Pattern matching is **case-sensitive** because every known producer of
/// these messages (cargo, rustc, Windows `FormatMessage`) emits them with
/// stable casing. Matches are anchored with surrounding parentheses where
/// applicable to avoid false positives such as `os error 320`.
///
/// Recognised patterns:
/// - `(os error 32)` — `ERROR_SHARING_VIOLATION` ("being used by another
///   process"). On non-Windows hosts the same numeric code maps to `EPIPE`,
///   which is **not** a retry-worthy condition, so this pattern is gated to
///   `cfg!(windows)`.
/// - `(os error 5)` — `ERROR_ACCESS_DENIED`. Same Windows-only gate; on
///   POSIX, errno 5 is `EIO` which we don't want to retry.
/// - `being used by another process` — Windows-formatted form of error 32.
/// - `Access is denied` / `access is denied` — both common casings of the
///   Windows-formatted form of error 5.
/// - `sharing violation` / `Sharing violation` — both common casings of the
///   Windows-formatted form of error 32.
///
/// These show up in cargo / rustc output when an antivirus, file indexer, or
/// previous build process has briefly grabbed a handle on a `.exe`, `.pdb`,
/// `.rmeta`, or `.lock` file in `target/`.
pub fn is_transient_busy_error(stderr_or_stdout: &str) -> bool {
    let s = stderr_or_stdout;
    let os_error_match =
        cfg!(windows) && (s.contains("(os error 32)") || s.contains("(os error 5)"));
    os_error_match
        || s.contains("being used by another process")
        || s.contains("Access is denied")
        || s.contains("access is denied")
        || s.contains("sharing violation")
        || s.contains("Sharing violation")
}

// ── toolchain resolution ──────────────────────────────────────────────────────

/// Where the cargo (or rustc) binary path came from when resolved.
///
/// Used for diagnostic logging so users can tell *which* cargo cargo-mcp
/// actually invoked when the wrong toolchain is in play.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolutionSource {
    /// `CARGO` (or `RUSTC`) environment variable pointed at an existing file.
    CargoEnv,
    /// Found at `$CARGO_HOME/bin/` (or `~/.cargo/bin/`) **with** a sibling
    /// `rustup` binary — the canonical rustup proxy location. Invoking this
    /// honours `rust-toolchain.toml`.
    RustupProxy,
    /// Found at the rustup-proxy location **without** a sibling `rustup`.
    /// Treated as a regular cargo (toolchain file likely won't be honoured).
    RustupProxyNoSibling,
    /// No env override and no proxy on disk; fall back to a bare name and let
    /// the OS resolve via `PATH` at spawn time.
    PathLookup,
}

impl ResolutionSource {
    /// Numeric step (1, 2, or 3) matching the resolver's tier order.
    pub fn step(self) -> u8 {
        match self {
            Self::CargoEnv => 1,
            Self::RustupProxy | Self::RustupProxyNoSibling => 2,
            Self::PathLookup => 3,
        }
    }
}

/// Resolve which `cargo` binary to invoke.
///
/// Three-tier resolution (first match wins):
///
/// 1. **`CARGO` env var** — if set and points to an existing file, use it.
///    Standard cargo escape hatch; nested cargo invocations rely on it.
/// 2. **Rustup proxy** at `$CARGO_HOME/bin/cargo[.exe]` (default
///    `~/.cargo/bin/cargo[.exe]`). When present **with** a sibling `rustup`
///    binary, this is the rustup proxy and invoking it honours
///    `rust-toolchain.toml` regardless of `PATH` ordering.
/// 3. **`PATH` lookup** — fall back to the bare name `"cargo"` and let the OS
///    resolve it at spawn time.
///
/// The corresponding diagnostic surface is the `cargo_diagnostic` MCP tool,
/// which reports the resolved path, the resolution step, and surrounding
/// environment so toolchain-mismatch problems can be diagnosed in one shot.
pub fn resolve_cargo_binary() -> (PathBuf, ResolutionSource) {
    resolve_binary("cargo", "CARGO")
}

/// Resolve which `rustc` binary to invoke (used by `cargo_diagnostic`).
///
/// Mirrors [`resolve_cargo_binary`]: env var → rustup proxy → PATH.
pub fn resolve_rustc_binary() -> (PathBuf, ResolutionSource) {
    resolve_binary("rustc", "RUSTC")
}

fn resolve_binary(name: &str, env_var: &str) -> (PathBuf, ResolutionSource) {
    // Step 1: explicit env var override.
    if let Some(v) = std::env::var_os(env_var)
        && !v.is_empty()
    {
        let p = PathBuf::from(&v);
        if p.is_file() {
            return (p, ResolutionSource::CargoEnv);
        }
        // Set but not a file — fall through (don't error, don't honour).
    }

    // Step 2: rustup proxy at CARGO_HOME/bin or ~/.cargo/bin.
    if let Some(cargo_home) = cargo_home_dir() {
        let bin_name = if cfg!(windows) {
            format!("{name}.exe")
        } else {
            name.to_string()
        };
        let path = cargo_home.join("bin").join(&bin_name);
        if path.is_file() {
            let rustup_name = if cfg!(windows) {
                "rustup.exe"
            } else {
                "rustup"
            };
            let sibling = cargo_home.join("bin").join(rustup_name);
            let source = if sibling.exists() {
                ResolutionSource::RustupProxy
            } else {
                ResolutionSource::RustupProxyNoSibling
            };
            return (path, source);
        }
    }

    // Step 3: bare name — PATH lookup happens at spawn time.
    (PathBuf::from(name), ResolutionSource::PathLookup)
}

/// Compute the effective `CARGO_HOME` directory.
///
/// Honours the `CARGO_HOME` env var if set and non-empty, otherwise
/// `~/.cargo` (`%USERPROFILE%\.cargo` on Windows).
pub fn cargo_home_dir() -> Option<PathBuf> {
    if let Some(v) = std::env::var_os("CARGO_HOME")
        && !v.is_empty()
    {
        return Some(PathBuf::from(v));
    }
    user_home_dir().map(|h| h.join(".cargo"))
}

/// Best-effort home directory: `%USERPROFILE%` on Windows, `$HOME` elsewhere.
///
/// Returns `None` if neither is set (unusual; resolver then falls through to
/// PATH lookup rather than panicking).
pub fn user_home_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    let var = "USERPROFILE";
    #[cfg(not(windows))]
    let var = "HOME";
    std::env::var_os(var).and_then(|v| {
        if v.is_empty() {
            None
        } else {
            Some(PathBuf::from(v))
        }
    })
}

/// Walk `start` and its ancestors looking for `rust-toolchain.toml` (or the
/// legacy `rust-toolchain`). Returns the first match found, or `None`.
pub fn find_toolchain_file(start: &Path) -> Option<PathBuf> {
    let mut cur: Option<&Path> = Some(start);
    while let Some(dir) = cur {
        for name in ["rust-toolchain.toml", "rust-toolchain"] {
            let candidate = dir.join(name);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
        cur = dir.parent();
    }
    None
}

/// Emit a one-line diagnostic to stderr describing a cargo invocation.
///
/// VS Code captures the cargo-mcp server's stderr in the "MCP Logs: cargo"
/// output channel, so this surfaces "which cargo did I just run" without
/// requiring the caller to wire through an MCP log channel.
fn log_invocation(path: &Path, source: ResolutionSource, working_dir: Option<&str>, args: &[&str]) {
    eprintln!(
        "cargo-mcp: invoking {} (source={:?}, step={}) cwd={:?} args={:?}",
        path.display(),
        source,
        source.step(),
        working_dir.unwrap_or("."),
        args,
    );
    if matches!(source, ResolutionSource::RustupProxyNoSibling) {
        eprintln!(
            "cargo-mcp: warning: {} exists but no sibling rustup found \
             — rust-toolchain.toml may not be honoured",
            path.display(),
        );
    }
}

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

/// Append a self-diagnosing hint to `stderr` when its content suggests the
/// failure was caused by `working_dir` defaulting to the cargo-mcp server's
/// own CWD rather than the user's workspace.
///
/// Triggered by:
/// - `error: could not find `Cargo.toml`` (no manifest under cwd)
/// - `error: no override and no rust-toolchain.toml found` (rustup couldn't
///   resolve a toolchain because no manifest/toolchain file is in scope)
/// - `error: rustup could not choose a version of cargo to run` (same root
///   cause from rustup's angle)
///
/// This short-circuits the misdiagnosis loop where an agent retries with the
/// same arguments instead of pointing at the workspace explicitly.
fn maybe_append_working_dir_hint(stderr: &mut String, working_dir: Option<&str>) {
    let triggers = [
        "could not find `Cargo.toml`",
        "no override and no rust-toolchain.toml found",
        "rustup could not choose a version",
    ];
    if !triggers.iter().any(|t| stderr.contains(t)) {
        return;
    }
    let effective_cwd = working_dir.map(String::from).unwrap_or_else(|| {
        std::env::current_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| "<unknown>".into())
    });
    let source = if working_dir.is_some() {
        "(passed by caller)"
    } else {
        "(default \u{2014} cargo-mcp server's process CWD; this is almost \
         certainly NOT your workspace)"
    };
    if !stderr.ends_with('\n') {
        stderr.push('\n');
    }
    stderr.push_str(&format!(
        "\nhint: cargo-mcp's effective working directory was {effective_cwd} {source}. \
         Pass `working_dir` explicitly, set to the absolute path of your workspace \
         root (the directory containing the top-level Cargo.toml), then retry. \
         Use the cargo_diagnostic tool for a full toolchain/path report.\n"
    ));
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
    // Retry only for cargo subcommands that are inherently idempotent. This
    // is the gate that keeps `cargo_publish`, `cargo_add`, `cargo_remove`
    // (and anything else not in `IDEMPOTENT_SUBCOMMANDS`) from being silently
    // re-executed on a transient busy error.
    let max_attempts = if RETRY_ENABLED.load(Ordering::Relaxed) && is_retry_safe(args) {
        RETRY_MAX_ATTEMPTS.load(Ordering::Relaxed).max(1) as usize
    } else {
        1
    };
    let delay = Duration::from_millis(RETRY_DELAY_MS.load(Ordering::Relaxed));

    let mut last: Option<CargoOutput> = None;
    for attempt in 1..=max_attempts {
        let out = run_cargo_streaming_once(args, working_dir, on_stdout_line)?;
        let busy = out.exit_code != 0
            && (is_transient_busy_error(&out.stderr) || is_transient_busy_error(&out.stdout));
        if !busy || attempt == max_attempts {
            return Ok(out);
        }
        // Surface the retry as a synthetic line so the streaming caller can
        // forward it as a progress notification.
        let msg = format!(
            "cargo-mcp: transient file-busy error; retrying in {ms}ms (attempt {next}/{total})",
            ms = delay.as_millis(),
            next = attempt + 1,
            total = max_attempts,
        );
        on_stdout_line(&msg);
        last = Some(out);
        // Honour cancellation while sleeping.
        let step = Duration::from_millis(50);
        let mut remaining = delay;
        while remaining > Duration::ZERO {
            if is_cancelled() {
                return Err(Box::new(CancelledError));
            }
            let s = std::cmp::min(step, remaining);
            thread::sleep(s);
            remaining -= s;
        }
    }
    // Unreachable (loop returns inside), but keep a safe fallback.
    Ok(last.unwrap_or(CargoOutput {
        stdout: String::new(),
        stderr: String::new(),
        exit_code: -1,
    }))
}

/// Single-attempt body of [`run_cargo_streaming`]; see that function for the
/// retry policy and contract.
fn run_cargo_streaming_once(
    args: &[&str],
    working_dir: Option<&str>,
    on_stdout_line: &mut dyn FnMut(&str),
) -> Result<CargoOutput, Box<dyn std::error::Error>> {
    let (cargo_path, source) = resolve_cargo_binary();
    log_invocation(&cargo_path, source, working_dir, args);
    let mut cmd = Command::new(&cargo_path);
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

    let mut stderr_buf = stderr_thread.join().unwrap_or_default();
    let status = child.wait()?;
    let exit_code = status.code().unwrap_or(-1);
    if exit_code != 0 {
        maybe_append_working_dir_hint(&mut stderr_buf, working_dir);
    }

    Ok(CargoOutput {
        stdout: stdout_buf,
        stderr: stderr_buf,
        exit_code,
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
    let (cargo_path, source) = resolve_cargo_binary();
    log_invocation(&cargo_path, source, working_dir, args);
    let mut cmd = Command::new(&cargo_path);
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

    let mut stderr_buf = stderr_thread.join().unwrap_or_default();
    let exit_code = status.code().unwrap_or(-1);
    if exit_code != 0 {
        maybe_append_working_dir_hint(&mut stderr_buf, working_dir);
    }

    Ok(CargoOutput {
        stdout: String::new(), // nothing buffered; caller reads from dest_file
        stderr: stderr_buf,
        exit_code,
    })
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    //! Tests for [`resolve_cargo_binary`] et al.
    //!
    //! These tests mutate process-global environment variables, so they
    //! serialize through [`ENV_LOCK`]. Each test snapshots the relevant vars
    //! up front and restores them on drop via [`EnvGuard`].
    use super::*;
    use std::ffi::{OsStr, OsString};
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// Serializes any test that mutates the global `RETRY_*` atomics, so
    /// parallel test execution can't race on shared retry config.
    static RETRY_LOCK: Mutex<()> = Mutex::new(());

    /// RAII guard that restores a set of env vars on drop.
    struct EnvGuard {
        saved: Vec<(&'static str, Option<OsString>)>,
    }

    impl EnvGuard {
        fn snapshot(vars: &[&'static str]) -> Self {
            let saved = vars.iter().map(|&v| (v, std::env::var_os(v))).collect();
            Self { saved }
        }

        fn set(&self, key: &str, value: impl AsRef<OsStr>) {
            // SAFETY: tests serialized via ENV_LOCK; no other thread is
            // observing or mutating env in parallel.
            unsafe {
                std::env::set_var(key, value);
            }
        }

        fn unset(&self, key: &str) {
            // SAFETY: tests serialized via ENV_LOCK.
            unsafe {
                std::env::remove_var(key);
            }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            for (k, v) in self.saved.drain(..) {
                // SAFETY: tests serialized via ENV_LOCK.
                unsafe {
                    match v {
                        Some(val) => std::env::set_var(k, val),
                        None => std::env::remove_var(k),
                    }
                }
            }
        }
    }

    /// Create a unique temp directory under `std::env::temp_dir()`.
    fn unique_tempdir(label: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "cargo-mcp-test-{}-{}-{}",
            label,
            std::process::id(),
            n,
        ));
        std::fs::create_dir_all(&dir).expect("create tempdir");
        dir
    }

    fn bin_name(name: &str) -> String {
        if cfg!(windows) {
            format!("{name}.exe")
        } else {
            name.to_string()
        }
    }

    fn write_fake_bin(dir: &Path, name: &str) -> PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let path = dir.join(bin_name(name));
        std::fs::write(&path, b"#!/bin/sh\n").unwrap();
        path
    }

    #[test]
    fn working_dir_hint_appended_for_missing_manifest() {
        let mut stderr = String::from(
            "error: could not find `Cargo.toml` in `/some/path` or any parent directory\n",
        );
        maybe_append_working_dir_hint(&mut stderr, None);
        assert!(stderr.contains("hint: cargo-mcp's effective working directory"));
        assert!(stderr.contains("default"));
        assert!(stderr.contains("Pass `working_dir` explicitly"));
    }

    #[test]
    fn working_dir_hint_appended_for_toolchain_missing() {
        let mut stderr =
            String::from("error: no override and no rust-toolchain.toml found in /some/path\n");
        maybe_append_working_dir_hint(&mut stderr, Some("/explicit/wd"));
        assert!(stderr.contains("hint:"));
        assert!(stderr.contains("/explicit/wd"));
        assert!(stderr.contains("(passed by caller)"));
    }

    #[test]
    fn working_dir_hint_not_appended_for_unrelated_error() {
        let original = "error: unresolved import `nonexistent`\n";
        let mut stderr = String::from(original);
        maybe_append_working_dir_hint(&mut stderr, None);
        assert_eq!(stderr, original, "hint must not fire on unrelated errors");
    }

    #[test]
    fn cargo_env_var_honoured_when_pointing_at_existing_file() {
        let _g = ENV_LOCK.lock().unwrap();
        let guard = EnvGuard::snapshot(&["CARGO", "CARGO_HOME", "HOME", "USERPROFILE"]);
        let dir = unique_tempdir("cargo_env");
        let fake = write_fake_bin(&dir, "my-cargo");
        guard.set("CARGO", &fake);

        let (path, source) = resolve_cargo_binary();
        assert_eq!(path, fake);
        assert_eq!(source, ResolutionSource::CargoEnv);
        assert_eq!(source.step(), 1);
    }

    #[test]
    fn cargo_env_var_pointing_at_missing_file_is_skipped() {
        let _g = ENV_LOCK.lock().unwrap();
        let guard = EnvGuard::snapshot(&["CARGO", "CARGO_HOME", "HOME", "USERPROFILE"]);
        let dir = unique_tempdir("cargo_env_missing");
        guard.set("CARGO", dir.join("does-not-exist"));
        // No CARGO_HOME / HOME → should fall through to PathLookup.
        guard.unset("CARGO_HOME");
        guard.unset("HOME");
        guard.unset("USERPROFILE");

        let (path, source) = resolve_cargo_binary();
        assert_eq!(source, ResolutionSource::PathLookup);
        assert_eq!(path, PathBuf::from("cargo"));
    }

    #[test]
    fn rustup_proxy_with_sibling_is_preferred_over_path() {
        let _g = ENV_LOCK.lock().unwrap();
        let guard = EnvGuard::snapshot(&["CARGO", "CARGO_HOME", "HOME", "USERPROFILE"]);
        let cargo_home = unique_tempdir("cargo_home");
        let bin_dir = cargo_home.join("bin");
        let cargo_path = write_fake_bin(&bin_dir, "cargo");
        write_fake_bin(&bin_dir, "rustup");
        guard.unset("CARGO");
        guard.set("CARGO_HOME", &cargo_home);

        let (path, source) = resolve_cargo_binary();
        assert_eq!(path, cargo_path);
        assert_eq!(source, ResolutionSource::RustupProxy);
        assert_eq!(source.step(), 2);
    }

    #[test]
    fn rustup_proxy_without_sibling_emits_no_sibling_variant() {
        let _g = ENV_LOCK.lock().unwrap();
        let guard = EnvGuard::snapshot(&["CARGO", "CARGO_HOME", "HOME", "USERPROFILE"]);
        let cargo_home = unique_tempdir("cargo_home_nosib");
        let bin_dir = cargo_home.join("bin");
        let cargo_path = write_fake_bin(&bin_dir, "cargo");
        // Note: no sibling rustup written.
        guard.unset("CARGO");
        guard.set("CARGO_HOME", &cargo_home);

        let (path, source) = resolve_cargo_binary();
        assert_eq!(path, cargo_path);
        assert_eq!(source, ResolutionSource::RustupProxyNoSibling);
        assert_eq!(source.step(), 2);
    }

    #[test]
    fn no_proxy_falls_back_to_path_lookup() {
        let _g = ENV_LOCK.lock().unwrap();
        let guard = EnvGuard::snapshot(&["CARGO", "CARGO_HOME", "HOME", "USERPROFILE"]);
        let empty_home = unique_tempdir("empty_home");
        // No bin/cargo under this home.
        guard.unset("CARGO");
        guard.set("CARGO_HOME", &empty_home);

        let (path, source) = resolve_cargo_binary();
        assert_eq!(path, PathBuf::from("cargo"));
        assert_eq!(source, ResolutionSource::PathLookup);
        assert_eq!(source.step(), 3);
    }

    #[test]
    fn unset_home_does_not_panic() {
        let _g = ENV_LOCK.lock().unwrap();
        let guard = EnvGuard::snapshot(&["CARGO", "CARGO_HOME", "HOME", "USERPROFILE"]);
        guard.unset("CARGO");
        guard.unset("CARGO_HOME");
        guard.unset("HOME");
        guard.unset("USERPROFILE");

        let (path, source) = resolve_cargo_binary();
        assert_eq!(path, PathBuf::from("cargo"));
        assert_eq!(source, ResolutionSource::PathLookup);
    }

    #[test]
    fn rustc_resolution_mirrors_cargo() {
        let _g = ENV_LOCK.lock().unwrap();
        let guard = EnvGuard::snapshot(&["RUSTC", "CARGO_HOME", "HOME", "USERPROFILE"]);
        let cargo_home = unique_tempdir("rustc_home");
        let bin_dir = cargo_home.join("bin");
        let rustc_path = write_fake_bin(&bin_dir, "rustc");
        write_fake_bin(&bin_dir, "rustup");
        guard.unset("RUSTC");
        guard.set("CARGO_HOME", &cargo_home);

        let (path, source) = resolve_rustc_binary();
        assert_eq!(path, rustc_path);
        assert_eq!(source, ResolutionSource::RustupProxy);
    }

    #[test]
    fn find_toolchain_file_walks_ancestors() {
        let root = unique_tempdir("toolchain_walk");
        let nested = root.join("a").join("b").join("c");
        std::fs::create_dir_all(&nested).unwrap();
        let toolchain = root.join("rust-toolchain.toml");
        std::fs::write(&toolchain, b"[toolchain]\nchannel = \"stable\"\n").unwrap();

        let found = find_toolchain_file(&nested).expect("should find toolchain file");
        assert_eq!(found, toolchain);
    }

    #[test]
    fn find_toolchain_file_returns_none_when_absent() {
        let root = unique_tempdir("toolchain_none");
        assert!(find_toolchain_file(&root).is_none());
    }

    // ── retry-on-busy detection ──────────────────────────────────────────────

    #[test]
    fn detects_windows_sharing_violation_via_phrase() {
        // The phrase "being used by another process" is recognised on every
        // host (it's a Windows-formatted message that may surface in stderr
        // captured by cross-compilation tooling, etc.).
        let stderr = "error: failed to remove file `target\\debug\\foo.exe`: \
                      The process cannot access the file because it is being used \
                      by another process. (os error 32)";
        assert!(is_transient_busy_error(stderr));
    }

    #[test]
    fn detects_windows_access_denied_via_phrase() {
        // Even on non-Windows, the phrase "Access is denied" alone is enough.
        let stderr =
            "error: failed to write `target\\debug\\foo.pdb`: Access is denied. (os error 5)";
        assert!(is_transient_busy_error(stderr));
    }

    #[test]
    fn detects_lowercase_access_is_denied() {
        let stderr = "io error: access is denied";
        assert!(is_transient_busy_error(stderr));
    }

    #[test]
    fn detects_sharing_violation_phrase() {
        let stderr = "rustc: a sharing violation occurred";
        assert!(is_transient_busy_error(stderr));
    }

    #[test]
    fn does_not_match_unrelated_compile_errors() {
        let stderr = "error[E0432]: unresolved import `foo::bar`";
        assert!(!is_transient_busy_error(stderr));
    }

    #[test]
    fn does_not_match_arbitrary_os_error_codes() {
        let stderr = "error: os error 2 (No such file or directory)";
        assert!(!is_transient_busy_error(stderr));
    }

    #[test]
    fn os_error_32_without_parens_is_not_a_match_to_avoid_false_positives() {
        // Without the surrounding parens this could be `os error 320` etc.;
        // the previous (looser) implementation would mis-match. Verify the
        // tightened pattern rejects substring lookalikes.
        let stderr = "error: random text mentioning os error 320";
        assert!(!is_transient_busy_error(stderr));
    }

    #[cfg(windows)]
    #[test]
    fn detects_parenthesised_os_error_32_on_windows() {
        let stderr = "io error (os error 32)";
        assert!(is_transient_busy_error(stderr));
    }

    #[cfg(windows)]
    #[test]
    fn detects_parenthesised_os_error_5_on_windows() {
        let stderr = "io error (os error 5)";
        assert!(is_transient_busy_error(stderr));
    }

    #[cfg(not(windows))]
    #[test]
    fn ignores_parenthesised_os_error_32_on_non_windows_because_it_means_epipe() {
        // On POSIX, errno 32 is EPIPE — not retry-worthy. The phrase
        // version of the same message is what we want to match instead.
        let stderr = "io error (os error 32)";
        assert!(!is_transient_busy_error(stderr));
    }

    #[test]
    fn is_retry_safe_allows_idempotent_subcommands() {
        for sub in [
            "check", "build", "test", "clippy", "fmt", "doc", "tree", "clean", "metadata",
        ] {
            assert!(is_retry_safe(&[sub]), "{sub} should be retry-safe");
        }
    }

    #[test]
    fn is_retry_safe_rejects_non_idempotent_subcommands() {
        // `fix` and `update` modify the working tree / lockfile, so a
        // partial first attempt leaves state behind that we can't safely
        // retry on top of. They must NOT be in the allowlist even though
        // they're read-mostly.
        for sub in [
            "publish", "add", "remove", "yank", "owner", "login", "fix", "update",
        ] {
            assert!(
                !is_retry_safe(&[sub]),
                "{sub} should NOT be retry-safe (state-changing)"
            );
        }
    }

    #[test]
    fn is_retry_safe_rejects_empty_args() {
        assert!(!is_retry_safe(&[]));
    }

    #[test]
    fn set_retry_config_clamps_max_attempts_to_one() {
        // Serialize against any other test that touches RETRY_* atomics.
        let _g = RETRY_LOCK.lock().unwrap();

        // Save and restore so other tests aren't disturbed.
        let prev_enabled = RETRY_ENABLED.load(Ordering::Relaxed);
        let prev_delay = RETRY_DELAY_MS.load(Ordering::Relaxed);
        let prev_max = RETRY_MAX_ATTEMPTS.load(Ordering::Relaxed);

        set_retry_config(true, 100, 0);
        assert_eq!(RETRY_MAX_ATTEMPTS.load(Ordering::Relaxed), 1);

        set_retry_config(false, 250, 5);
        assert!(!RETRY_ENABLED.load(Ordering::Relaxed));
        assert_eq!(RETRY_DELAY_MS.load(Ordering::Relaxed), 250);
        assert_eq!(RETRY_MAX_ATTEMPTS.load(Ordering::Relaxed), 5);

        // Restore.
        set_retry_config(prev_enabled, prev_delay, prev_max);
    }
}
