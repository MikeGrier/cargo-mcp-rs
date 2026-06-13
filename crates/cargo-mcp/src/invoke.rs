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
//! - `RUSTC=<resolved rustc>` — pinned to the rustc resolved by
//!   [`resolve_rustc_binary`] (when not [`ResolutionSource::PathLookup`]) so
//!   that `cargo` does not fall back to a stray `rustc` that happens to come
//!   first on `PATH`. Without this, environments that prepend a non-rustup
//!   `rustc` ahead of the rustup proxy bin dir would silently bypass
//!   `rust-toolchain.toml` even though `cargo` itself is the rustup proxy.
//!   Honoured only if the caller has not already set `RUSTC`.
//!
//! Tool callers can layer additional `set`/`unset` operations on top via
//! [`set_extra_env`]; the per-tool `env` parameter funnels through that
//! mechanism so a tool call can request e.g. `RUSTFLAGS=…` or
//! `FIREBIRD_DUMP_MIR=1` for that one invocation without restarting the
//! server. Extra env is applied **after** the built-in defaults above, so a
//! caller-supplied value wins.
//!
//! ## Logging
//!
//! Each invocation writes a one-line `cargo-mcp: invoking <path> ...` record
//! to the MCP `notifications/message` channel at `info` level, so the
//! resolved binary is visible in the client's MCP output pane without
//! enabling any extra tracing.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
    mpsc,
};
use std::thread;
use std::time::{Duration, Instant};

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

/// Whether to call into the Restart Manager (`crate::rm`) to identify the
/// processes holding a busy file when cargo reports a transient file-lock
/// error. Off by default because it is the only feature in the production
/// binary that exercises `unsafe` Win32 FFI; users who want the diagnostic
/// must opt in via `--unsafe-windows-rm=true` (or the matching VS Code
/// setting).
static RM_LOOKUP_ENABLED: AtomicBool = AtomicBool::new(false);

/// Configure retry-on-busy behaviour. Called once from `main` after CLI parse.
pub fn set_retry_config(enabled: bool, delay_ms: u64, max_attempts: u32) {
    RETRY_ENABLED.store(enabled, Ordering::Relaxed);
    RETRY_DELAY_MS.store(delay_ms, Ordering::Relaxed);
    RETRY_MAX_ATTEMPTS.store(max_attempts.max(1), Ordering::Relaxed);
}

/// Enable or disable the Restart Manager "who holds this file" lookup.
pub fn set_rm_lookup_enabled(enabled: bool) {
    RM_LOOKUP_ENABLED.store(enabled, Ordering::Relaxed);
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

/// Returns `true` iff the cargo subcommand is in [`IDEMPOTENT_SUBCOMMANDS`].
///
/// A leading `+<toolchain>` override (e.g. `+nightly`) may precede the
/// subcommand (`cargo +nightly test ...`); it is skipped when locating the
/// subcommand so toolchain-pinned invocations remain retry-eligible.
fn is_retry_safe(args: &[&str]) -> bool {
    let sub = match args.first() {
        Some(first) if first.starts_with('+') => args.get(1),
        other => other,
    };
    sub.is_some_and(|sub| IDEMPOTENT_SUBCOMMANDS.contains(sub))
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

/// Inject `RUSTC=<resolved rustc>` into `cmd`'s environment so the spawned
/// `cargo` does not fall back to a stray `rustc` from `PATH`.
///
/// Without this, an environment that prepends a non-rustup directory
/// containing a plain `rustc.exe` ahead of `~/.cargo/bin` would cause cargo
/// (even when *cargo* itself was correctly resolved to the rustup proxy) to
/// invoke that stray `rustc`, silently bypassing `rust-toolchain.toml`. See
/// the module-level "Environment" docs.
///
/// Behaviour:
/// - If the user has already set `RUSTC` in the inherited environment, it is
///   left alone (their explicit choice wins).
/// - If [`resolve_rustc_binary`] returned [`ResolutionSource::PathLookup`],
///   no override is set: there is no concrete path to pin, and forcing
///   `RUSTC=rustc` would just re-run the same `PATH` lookup cargo would do
///   anyway.
/// - Otherwise (`RustupProxy` or `RustupProxyNoSibling`), set `RUSTC` to
///   the resolved path. For the rustup proxy this defers toolchain
///   selection to the proxy, which honours `rust-toolchain.toml`.
fn apply_rustc_env_to_map(env: &mut BTreeMap<OsString, OsString>) {
    if env.get(OsStr::new("RUSTC")).is_some_and(|v| !v.is_empty()) {
        return;
    }
    let (rustc_path, source) = resolve_rustc_binary();
    if matches!(source, ResolutionSource::PathLookup) {
        return;
    }
    env.insert(OsString::from("RUSTC"), rustc_path.into_os_string());
}

/// Build the **complete** environment block to hand to the cargo subprocess
/// at spawn time.
///
/// On Windows this becomes the explicit `lpEnvironment` argument to
/// `CreateProcess`; on Unix it is the `envp` passed to `execvpe`. Callers
/// install the block with `cmd.env_clear().envs(build_subprocess_env())` so
/// the child sees exactly what we computed — no implicit merge between
/// per-`Command` overrides and the parent's inherited block at spawn time.
///
/// Layering, in order (later entries win):
/// 1. The parent process's current environment ([`std::env::vars_os`]).
/// 2. cargo-mcp's built-in defaults (`CARGO_TERM_COLOR=never`, `NO_COLOR=1`).
/// 3. The `RUSTC` pin from [`apply_rustc_env_to_map`].
/// 4. Per-call overrides from [`set_extra_env`] (set or unset).
fn build_subprocess_env() -> BTreeMap<OsString, OsString> {
    let mut env: BTreeMap<OsString, OsString> = std::env::vars_os().collect();
    env.insert(OsString::from("CARGO_TERM_COLOR"), OsString::from("never"));
    env.insert(OsString::from("NO_COLOR"), OsString::from("1"));
    apply_rustc_env_to_map(&mut env);
    apply_extra_env_to_map(&mut env);
    env
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

/// Emit a one-line diagnostic describing a cargo invocation.
///
/// Routed through the MCP `notifications/message` channel so the client
/// displays it at the intended level (`info` for the invocation record,
/// `warning` for the no-sibling-rustup advisory) in its MCP output pane,
/// rather than as untyped stderr output.
fn log_invocation(path: &Path, source: ResolutionSource, working_dir: Option<&str>, args: &[&str]) {
    emit_mcp_log(
        "info",
        &format!(
            "invoking {} (source={:?}, step={}) cwd={:?} args={:?}",
            path.display(),
            source,
            source.step(),
            working_dir.unwrap_or("."),
            args,
        ),
    );
    if matches!(source, ResolutionSource::RustupProxyNoSibling) {
        emit_mcp_log(
            "warning",
            &format!(
                "{} exists but no sibling rustup found — rust-toolchain.toml may not be honoured",
                path.display(),
            ),
        );
    }
}

/// Send a `notifications/message` JSON-RPC frame to stdout.
///
/// `io::Stdout` uses a reentrant mutex, so locking here is safe even when
/// the main loop is already holding the stdout lock between message reads.
fn emit_mcp_log(level: &str, message: &str) {
    let frame = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "notifications/message",
        "params": {
            "level": level,
            "logger": "cargo-mcp",
            "data": format!("cargo-mcp: {message}"),
        },
    });
    if let Ok(mut s) = serde_json::to_string(&frame) {
        s.push('\n');
        let stdout = std::io::stdout();
        let mut guard = stdout.lock();
        let _ = guard.write_all(s.as_bytes());
        let _ = guard.flush();
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

/// Error returned when a cargo operation exceeds its `timeout_secs` budget.
#[derive(Debug)]
pub struct TimeoutError {
    /// Actual wall-clock duration measured from the start of the overall
    /// operation (before the first subprocess spawn) until the timeout
    /// trigger fired. This spans every attempt including any retry
    /// backoff sleeps, not just the currently-running subprocess, so the
    /// value will be slightly greater than the configured budget due to
    /// the polling interval used to detect the deadline.
    pub elapsed: Duration,
}

impl std::fmt::Display for TimeoutError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Operation timed out after {:.3} seconds; cargo subprocess and all \
             of its descendants were terminated.",
            self.elapsed.as_secs_f64(),
        )
    }
}

impl std::error::Error for TimeoutError {}

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

// ── extra environment ─────────────────────────────────────────────────────────

/// A list of `(name, value)` env operations to apply to every cargo
/// subprocess spawned on this thread. `None` removes the variable from the
/// child's environment; `Some(value)` sets it.
pub type ExtraEnv = Vec<(String, Option<String>)>;

thread_local! {
    /// Extra env operations for the cargo operation currently running on
    /// this thread. Installed by [`set_extra_env`] and consumed by
    /// [`apply_extra_env_to_map`] inside [`build_subprocess_env`].
    static EXTRA_ENV: RefCell<ExtraEnv> = const { RefCell::new(Vec::new()) };
}

/// Install (or clear) the extra env operations for the current thread.
///
/// Pass the parsed `env` map before spawning a cargo subprocess and an
/// empty `Vec` after it returns. The subprocess runners merge these
/// operations into the explicit env block built by
/// [`build_subprocess_env`] **after** the built-in defaults
/// (`CARGO_TERM_COLOR`, `NO_COLOR`, `RUSTC`), so a caller-supplied
/// value wins over the default.
pub fn set_extra_env(env: ExtraEnv) {
    EXTRA_ENV.with(|e| *e.borrow_mut() = env);
}

/// Merge the current thread's extra env operations into `env`.
///
/// Called by [`build_subprocess_env`] last so caller-supplied values
/// override every prior layer (inherited env, built-in defaults, RUSTC
/// pin). `None` values map to a remove from the map.
fn apply_extra_env_to_map(env: &mut BTreeMap<OsString, OsString>) {
    EXTRA_ENV.with(|e| {
        for (k, v) in e.borrow().iter() {
            match v {
                Some(val) => {
                    env.insert(OsString::from(k), OsString::from(val));
                }
                None => {
                    env.remove(OsStr::new(k));
                }
            }
        }
    });
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

/// Extract busy file paths from cargo output and ask the OS which processes
/// hold them. Returns an empty vector when no busy paths could be parsed
/// out of the combined stderr/stdout (in which case there is nothing
/// useful to report to the agent).
///
/// Combines stderr and stdout because cargo writes its diagnostic blocks
/// to stderr but some downstream tools (notably the MSVC linker invoked
/// via `link.exe`) emit "file in use" errors on stdout instead.
fn collect_busy_holders(stderr: &str, stdout: &str) -> Vec<crate::busy_files::FileHolders> {
    if !RM_LOOKUP_ENABLED.load(Ordering::Relaxed) {
        return Vec::new();
    }
    let mut paths = crate::busy_files::extract_busy_paths(stderr);
    paths.extend(crate::busy_files::extract_busy_paths(stdout));
    if paths.is_empty() {
        return Vec::new();
    }
    // Dedupe across the two streams while keeping order.
    let mut seen = std::collections::HashSet::new();
    paths.retain(|p| seen.insert(p.clone()));
    let refs: Vec<&Path> = paths.iter().map(|p| p.as_path()).collect();
    crate::busy_files::query_holders(&refs)
}

/// Append the formatted holder report to `stderr`, ensuring a leading
/// blank line so it doesn't run into cargo's own message text.
fn append_holder_report(stderr: &mut String, report: &[crate::busy_files::FileHolders]) {
    let formatted = crate::busy_files::format_full_report(report);
    if formatted.is_empty() {
        return;
    }
    if !stderr.ends_with('\n') {
        stderr.push('\n');
    }
    stderr.push('\n');
    stderr.push_str(&formatted);
}

// ── subprocess runners ────────────────────────────────────────────────────────

/// Cross-platform wrapper around [`std::process::Child`] that owns the OS
/// objects needed to terminate the **entire descendant tree** of the spawned
/// process — not just the immediate child.
///
/// This matters for `cargo test` (and `cargo run`, `cargo build` via build
/// scripts, etc.): cargo spawns `rustc` and the compiled test binaries as
/// its own children, and a plain `Child::kill()` only stops `cargo` itself,
/// leaving rustc and any running tests behind to consume CPU until they
/// finish on their own.
///
/// ## Platform mechanics
///
/// - **Windows.** The child is assigned to a Job Object configured with
///   `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`. Dropping the job handle causes
///   the kernel to terminate every process in the job, including
///   grandchildren that inherit the job assignment automatically.
/// - **Unix.** The child is spawned into its own process group (PGID =
///   child PID) via [`CommandExt::process_group`]. On cancel we send
///   `SIGKILL` to the negated PGID, which delivers to every process whose
///   group leader is the cargo we spawned.
///
/// There is a microscopic window between `spawn` and "assigned to job" on
/// Windows where a grandchild could escape; in practice cargo does not
/// fork that fast, and assigning immediately after spawn is the standard
/// production pattern.
///
/// [`CommandExt::process_group`]: std::os::unix::process::CommandExt::process_group
struct ManagedChild {
    child: std::process::Child,
    #[cfg(windows)]
    job: Option<job_object::Job>,
}

impl ManagedChild {
    fn spawn(cmd: &mut Command) -> std::io::Result<Self> {
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            cmd.process_group(0);
        }
        let child = cmd.spawn()?;
        #[cfg(windows)]
        let job =
            match job_object::Job::new_kill_on_close().and_then(|j| j.assign(&child).map(|_| j)) {
                Ok(j) => Some(j),
                Err(e) => {
                    emit_mcp_log(
                        "warning",
                        &format!(
                            "failed to assign cargo subprocess (pid={}) to a Job Object: {e}. \
                         Cancellation/timeout will fall back to taskkill /T /F, which is \
                         best-effort and may leave grandchildren running if they detach \
                         from the parent before kill.",
                            child.id(),
                        ),
                    );
                    None
                }
            };
        Ok(Self {
            child,
            #[cfg(windows)]
            job,
        })
    }

    fn take_stdout(&mut self) -> Option<std::process::ChildStdout> {
        self.child.stdout.take()
    }

    fn take_stderr(&mut self) -> Option<std::process::ChildStderr> {
        self.child.stderr.take()
    }

    fn wait(&mut self) -> std::io::Result<std::process::ExitStatus> {
        self.child.wait()
    }

    fn try_wait(&mut self) -> std::io::Result<Option<std::process::ExitStatus>> {
        self.child.try_wait()
    }

    /// Terminate the child **and all of its descendants**, then reap.
    fn kill_tree(&mut self) {
        #[cfg(unix)]
        {
            // SAFETY: kill() with a process-group target (negative PID) is
            // async-signal-safe and operates on the kernel's PID table.
            let pgid = self.child.id() as i32;
            unsafe {
                libc::kill(-pgid, libc::SIGKILL);
            }
        }
        #[cfg(windows)]
        {
            // Closing the job (drop) triggers KILL_ON_JOB_CLOSE. If we
            // never managed to assign the child to a job (see the warning
            // emitted in `spawn`), fall back to `taskkill /T /F /PID`,
            // which walks the parent/child tree the Windows scheduler
            // tracks and is the standard escape hatch when Job Objects
            // are unavailable (e.g. running inside another job that
            // disallows breakaway).
            if self.job.is_some() {
                self.job = None;
            } else {
                let pid = self.child.id();
                let _ = Command::new("taskkill")
                    .args(["/T", "/F", "/PID", &pid.to_string()])
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status();
            }
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[cfg(windows)]
mod job_object {
    //! Minimal Job Object wrapper for tree-killing cargo subprocesses.

    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
        SetInformationJobObject,
    };

    pub(super) struct Job(HANDLE);

    // HANDLE is just a pointer; Send/Sync are fine because the kernel
    // object is reference-counted internally.
    unsafe impl Send for Job {}
    unsafe impl Sync for Job {}

    impl Job {
        pub(super) fn new_kill_on_close() -> std::io::Result<Self> {
            // SAFETY: NULL attributes/name are documented as valid;
            // a NULL return indicates failure and sets GetLastError().
            unsafe {
                let h = CreateJobObjectW(std::ptr::null(), std::ptr::null());
                if h.is_null() {
                    return Err(std::io::Error::last_os_error());
                }
                let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
                info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
                let ok = SetInformationJobObject(
                    h,
                    JobObjectExtendedLimitInformation,
                    &info as *const _ as *const _,
                    std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                );
                if ok == 0 {
                    let e = std::io::Error::last_os_error();
                    CloseHandle(h);
                    return Err(e);
                }
                Ok(Job(h))
            }
        }

        pub(super) fn assign(&self, child: &std::process::Child) -> std::io::Result<()> {
            // SAFETY: `child`'s process handle is valid for the lifetime
            // of the Child value, which the caller continues to own.
            unsafe {
                if AssignProcessToJobObject(self.0, child.as_raw_handle() as HANDLE) == 0 {
                    return Err(std::io::Error::last_os_error());
                }
            }
            Ok(())
        }
    }

    impl Drop for Job {
        fn drop(&mut self) {
            // SAFETY: handle came from CreateJobObjectW and is closed at
            // most once (Drop runs once). Closing the last handle on a
            // KILL_ON_JOB_CLOSE job kills every assigned process.
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
}

/// Outcome of polling [`ManagedChild`] for completion while watching the
/// thread-local cancel token and an optional wall-clock deadline.
enum WaitOutcome {
    /// Child exited cleanly; treat its output normally.
    Exited,
    /// Cancel token was set; child has been killed.
    Cancelled,
    /// Wall-clock deadline elapsed before the child finished; child has
    /// been killed. Carries the real elapsed duration measured at the
    /// point the deadline was detected.
    TimedOut(Duration),
}

/// Run `cargo <args>`, calling `on_stdout_line` for each stdout line as it
/// arrives, and return the complete output after the process exits.
///
/// Convenience wrapper around [`run_cargo_streaming_with_timeout`] for call
/// sites that don't need a wall-clock cap. Equivalent to passing
/// `timeout = None`.
#[allow(dead_code)] // used by `#[path = "../src/invoke.rs"]` integration tests
pub fn run_cargo_streaming(
    args: &[&str],
    working_dir: Option<&str>,
    on_stdout_line: &mut dyn FnMut(&str),
) -> Result<CargoOutput, Box<dyn std::error::Error>> {
    run_cargo_streaming_with_timeout(args, working_dir, None, None, on_stdout_line)
}

/// Predicate that, when it returns `true` for a streamed stdout line, **arms**
/// the wall-clock deadline at that instant instead of at process spawn.
///
/// Used by `cargo_test` so the timeout bounds only test *execution*: the
/// deadline starts when cargo emits the `build-finished` record (compilation
/// and linking are complete), not while the project is still building.
pub type ArmDeadline<'a> = &'a dyn Fn(&str) -> bool;

/// Run `cargo <args>` with a wall-clock budget, streaming stdout.
///
/// Stderr is drained in a background thread to prevent pipe-buffer deadlock
/// when the process produces large amounts of output on both streams.
/// Stdout is drained on a second background thread so the main thread can
/// poll the cancel token and the optional deadline on a short tick — a
/// blocking line-read could otherwise wedge the runner while `cargo test`
/// silently executes a slow test.
///
/// If the thread-local cancel token is set or the deadline elapses mid-run,
/// the **entire process tree** (cargo, rustc, spawned test binaries, …) is
/// killed via [`ManagedChild::kill_tree`] and [`CancelledError`] or
/// [`TimeoutError`] is returned respectively.
///
/// When `arm_deadline` is `Some`, the wall-clock budget does **not** start at
/// spawn; instead it begins the moment a streamed stdout line satisfies the
/// predicate (e.g. cargo's `build-finished` record). This lets `cargo_test`
/// time only test execution and never the compile/link phase. In that mode
/// retries are not bounded by the budget (a slow build can take as long as it
/// needs); each attempt arms its own deadline once execution begins.
pub fn run_cargo_streaming_with_timeout(
    args: &[&str],
    working_dir: Option<&str>,
    timeout: Option<Duration>,
    arm_deadline: Option<ArmDeadline<'_>>,
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

    // In deferred-arming mode the budget only covers test execution inside a
    // single attempt, so there is no overall wall-clock cap across retries +
    // backoff. In the normal mode, compute a single overall deadline so
    // retries + backoff cannot exceed the caller's budget; each attempt gets
    // only the remaining slice and sleeps between attempts are bounded by it.
    let arming = arm_deadline.is_some();
    let overall_start = Instant::now();
    let overall_deadline = if arming {
        None
    } else {
        timeout.and_then(|t| overall_start.checked_add(t))
    };
    let remaining = |now: Instant| -> Option<Duration> {
        overall_deadline.map(|d| d.saturating_duration_since(now))
    };

    let mut last: Option<CargoOutput> = None;
    for attempt in 1..=max_attempts {
        // In arming mode each attempt gets the full budget (armed lazily on
        // the marker); otherwise it gets the remaining slice of the overall
        // deadline.
        let attempt_budget = if arming {
            timeout
        } else {
            remaining(Instant::now())
        };
        if let Some(r) = attempt_budget
            && r.is_zero()
        {
            return Err(Box::new(TimeoutError {
                elapsed: overall_start.elapsed(),
            }));
        }
        let mut out = run_cargo_streaming_once(
            args,
            working_dir,
            attempt_budget,
            None, // retry-aware path: no shared absolute deadline (each attempt arms its own)
            None, // retry-aware path uses overall budget only; no per-test reset
            arm_deadline,
            None,
            on_stdout_line,
        )
        .map_err(|e| -> Box<dyn std::error::Error> {
            // In arming mode preserve the inner, execution-relative elapsed
            // (the build phase is excluded). Otherwise normalize any timeout
            // from the inner attempt to the
            // overall wall-clock elapsed across all attempts and backoff
            // sleeps, so callers see a consistent value regardless of
            // which branch detected the deadline.
            if e.is::<TimeoutError>() {
                if arming {
                    // Keep the execution-relative elapsed from the inner
                    // attempt; the build phase is intentionally excluded.
                    e
                } else {
                    Box::new(TimeoutError {
                        elapsed: overall_start.elapsed(),
                    })
                }
            } else {
                e
            }
        })?;
        let busy = out.exit_code != 0
            && (is_transient_busy_error(&out.stderr) || is_transient_busy_error(&out.stdout));
        if !busy {
            return Ok(out);
        }
        // Best-effort: identify the processes holding the busy files and
        // surface them. Done on every busy attempt because the offender
        // can change between retries (e.g. AV releases its handle but the
        // freshly-launched binary itself takes one).
        let holder_report = collect_busy_holders(&out.stderr, &out.stdout);
        if !holder_report.is_empty() {
            if let Some(line) = crate::busy_files::format_short_summary(&holder_report) {
                on_stdout_line(&line);
            }
            // Also append the full per-process breakdown to the captured
            // stderr so the agent sees it even if it never reads progress.
            append_holder_report(&mut out.stderr, &holder_report);
        }
        if attempt == max_attempts {
            // Emit an explicit "gave up" record only when we actually had a
            // retry budget to exhaust (max_attempts > 1). With max_attempts
            // == 1 — retries disabled, or the subcommand isn't on the
            // idempotent allowlist — there was no retry loop to give up
            // on, so calling it "give up" would mislabel a single-shot
            // busy failure. The cargo exit code already signals the
            // failure; the holder report above provides the diagnostic.
            if max_attempts > 1 {
                let give_up = format!(
                    "cargo-mcp: gave up after {total} attempts on transient file-busy error; cargo last exited with code {code}",
                    total = max_attempts,
                    code = out.exit_code,
                );
                on_stdout_line(&give_up);
                if !out.stderr.is_empty() && !out.stderr.ends_with('\n') {
                    out.stderr.push('\n');
                }
                out.stderr.push_str(&give_up);
                out.stderr.push('\n');
            }
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
        // Honour cancellation and the overall deadline while sleeping.
        let step = Duration::from_millis(50);
        let mut sleep_left = delay;
        while sleep_left > Duration::ZERO {
            if is_cancelled() {
                return Err(Box::new(CancelledError));
            }
            if let Some(r) = remaining(Instant::now())
                && r.is_zero()
            {
                return Err(Box::new(TimeoutError {
                    elapsed: overall_start.elapsed(),
                }));
            }
            let mut s = std::cmp::min(step, sleep_left);
            if let Some(r) = remaining(Instant::now()) {
                s = std::cmp::min(s, r);
            }
            if s.is_zero() {
                s = Duration::from_millis(1);
            }
            thread::sleep(s);
            sleep_left = sleep_left.saturating_sub(s);
        }
    }
    // Unreachable (loop returns inside), but keep a safe fallback.
    Ok(last.unwrap_or(CargoOutput {
        stdout: String::new(),
        stderr: String::new(),
        exit_code: -1,
    }))
}

/// Run `cargo <args>` with a streaming callback and up to **two independent
/// watchdogs** — a hard overall cap and a per-test idle cap.
///
/// Differs from [`run_cargo_streaming_with_timeout`] in two ways:
///
/// 1. **Two deadlines, not one.**
///    - `overall_timeout`: a single wall-clock cap that arms once (on the
///      first stdout line satisfying `arm_deadline`, or at spawn if no arm
///      predicate is given) and **never** resets. This is the
///      "keep-throughput-going" cap on the whole execution phase.
///    - `overall_deadline_abs`: an **absolute** wall-clock deadline that,
///      when `Some`, takes precedence over `overall_timeout` and is used
///      as-is (no arming behaviour, no `armed_at` offset). This is the
///      hook the `cargo_test` `test_filter` orchestrator uses to share a
///      single execution-phase deadline across every per-binary launch:
///      the orchestrator captures the *first* per-binary `build-finished`
///      instant and computes `arm_t + overall_timeout` once, then passes
///      that same absolute deadline into every subsequent launch so per-
///      launch build/startup time inside L2+ cannot extend the cap.
///      When both are `Some`, `overall_deadline_abs` wins.
///    - `per_test_timeout`: armed on the same trigger as the Duration-
///      based `overall_timeout` and then **reset to `now + per_test_timeout`**
///      every time a streamed stdout line satisfies `reset_deadline`. This
///      is the "hung-test" cap used by `cargo_test`'s `test_filter` mode —
///      each `test ... ok|FAILED|ignored` boundary line refreshes it.
///
///    Any of the three may be `None`. If multiple are `Some`, whichever
///    expires first terminates the child. `TimeoutError::elapsed` is
///    measured from the local arming instant in the per-test case; for the
///    overall deadline it is measured from the shared cross-launch anchor
///    (`overall_deadline_abs - overall_timeout`) when both are supplied,
///    so it reflects the configured budget rather than this launch's local
///    clock.
///
/// 2. **No retry-on-busy.** A `cargo test` invocation is not safe to silently
///    re-run partway through execution: a flaky test that happens to print a
///    busy-file marker into its own stdout could trigger a redundant
///    execution of the entire (filtered) test set. `test_filter`'s build
///    phase already ran under the retry-on-busy regime, so by the time we
///    reach this function we are only executing pre-built binaries.
#[allow(clippy::too_many_arguments)] // each input is independent; bundling them would obscure intent
pub fn run_cargo_streaming_with_watchdog(
    args: &[&str],
    working_dir: Option<&str>,
    overall_timeout: Option<Duration>,
    overall_deadline_abs: Option<Instant>,
    per_test_timeout: Option<Duration>,
    arm_deadline: Option<ArmDeadline<'_>>,
    reset_deadline: Option<ArmDeadline<'_>>,
    on_stdout_line: &mut dyn FnMut(&str),
) -> Result<CargoOutput, Box<dyn std::error::Error>> {
    run_cargo_streaming_once(
        args,
        working_dir,
        overall_timeout,
        overall_deadline_abs,
        per_test_timeout,
        arm_deadline,
        reset_deadline,
        on_stdout_line,
    )
}

/// Single-attempt body of [`run_cargo_streaming`]; see that function for the
/// retry policy and contract.
///
/// Two independent deadlines may be active simultaneously:
/// - `overall_timeout` arms on the first stdout line that satisfies
///   `arm_deadline` (or at spawn if no arm predicate is given) and **never**
///   resets — it bounds the entire execution phase as a hard wall clock.
///   `overall_deadline_abs`, when `Some`, takes precedence: the overall
///   deadline is set from it immediately (at spawn) and is never extended
///   by per-launch arming. The orchestrator uses this to share a single
///   deadline across multiple launches.
/// - `per_test_timeout` arms on the same trigger and additionally resets
///   back to `now + per_test_timeout` on every subsequent stdout line that
///   satisfies `reset_deadline` — the per-test watchdog used by the
///   `cargo_test` `test_filter` feature, where each
///   `test ... ok|FAILED|ignored` boundary line refreshes the budget.
///
/// If both are `Some`, whichever elapses first terminates the child.
/// `armed_at` is captured only at the initial arming, so when the per-test
/// watchdog fires the `TimeoutError::elapsed` value reports
/// execution-relative time regardless of how many resets occurred. When the
/// overall deadline fires and the caller supplied both
/// `overall_deadline_abs` and `overall_timeout`, the reported elapsed is
/// derived from `overall_deadline_abs - overall_timeout` instead so it
/// reflects the shared cross-launch anchor (the first per-binary
/// `build-finished` captured by the `test_filter` orchestrator) rather
/// than this single launch's local clock.
#[allow(clippy::too_many_arguments)] // each input is independent; bundling them would obscure intent
fn run_cargo_streaming_once(
    args: &[&str],
    working_dir: Option<&str>,
    overall_timeout: Option<Duration>,
    overall_deadline_abs: Option<Instant>,
    per_test_timeout: Option<Duration>,
    arm_deadline: Option<ArmDeadline<'_>>,
    reset_deadline: Option<ArmDeadline<'_>>,
    on_stdout_line: &mut dyn FnMut(&str),
) -> Result<CargoOutput, Box<dyn std::error::Error>> {
    let (cargo_path, source) = resolve_cargo_binary();
    log_invocation(&cargo_path, source, working_dir, args);
    let mut cmd = Command::new(&cargo_path);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env_clear()
        .envs(build_subprocess_env());

    if let Some(dir) = working_dir {
        cmd.current_dir(dir);
    }

    let mut child = ManagedChild::spawn(&mut cmd)?;
    let stdout_pipe = child.take_stdout().expect("stdout is piped");
    let stderr_pipe = child.take_stderr().expect("stderr is piped");

    // Drain stderr on a background thread to avoid deadlock when the stdout
    // pipe buffer fills while stderr is also accumulating.
    let stderr_thread = thread::spawn(move || -> String {
        let mut buf = String::new();
        let _ = BufReader::new(stderr_pipe).read_to_string(&mut buf);
        buf
    });

    // Drain stdout on its own background thread, forwarding each line
    // through a bounded mpsc channel. The main thread polls the channel
    // with a short timeout so cancel / wall-clock checks happen even when
    // cargo is silent (e.g. a slow test running with no progress output).
    // The channel is bounded so a slow `on_stdout_line` consumer applies
    // backpressure to the reader thread instead of letting an unbounded
    // backlog duplicate cargo's stdout in memory on top of `stdout_buf`.
    let (tx, rx) = mpsc::sync_channel::<Option<String>>(256);
    let stdout_thread = thread::spawn(move || {
        let reader = BufReader::new(stdout_pipe);
        for line in reader.lines() {
            match line {
                Ok(l) => {
                    if tx.send(Some(l)).is_err() {
                        return;
                    }
                }
                Err(_) => break,
            }
        }
        let _ = tx.send(None); // EOF sentinel
    });

    let start = Instant::now();
    // checked_add: a caller-supplied timeout near Duration::MAX would panic
    // on `start + t`. Treat overflow as "no deadline" rather than crashing.
    //
    // Normal mode: arm both deadlines at spawn. Deferred-arming mode
    // (`arm_deadline` is `Some`): leave them unset and arm them the first
    // time a streamed stdout line satisfies the predicate (e.g. cargo's
    // `build-finished` record), so only test execution is timed. `armed_at`
    // records that instant so the timeout's reported `elapsed` is relative
    // to execution start, not process spawn.
    let arming_required = arm_deadline.is_some();
    let mut armed = !arming_required;
    // overall_deadline_abs takes precedence over the Duration-based arming:
    // when Some, the overall deadline is set from it at spawn and is never
    // touched by the per-launch arming branch below. This is how the
    // `test_filter` orchestrator shares a single execution-phase deadline
    // across every per-binary launch (captured from L1's first build-
    // finished and reused for L2+ without restarting on each launch's
    // local arming).
    let mut overall_deadline = match overall_deadline_abs {
        Some(d) => Some(d),
        None if armed => overall_timeout.and_then(|t| start.checked_add(t)),
        None => None,
    };
    let mut per_test_deadline = if armed {
        per_test_timeout.and_then(|t| start.checked_add(t))
    } else {
        None
    };
    let mut armed_at: Option<Instant> = None;
    let mut stdout_buf = String::new();
    let mut outcome = WaitOutcome::Exited;
    let tick = Duration::from_millis(50);
    loop {
        if is_cancelled() {
            outcome = WaitOutcome::Cancelled;
            break;
        }
        // Whichever deadline elapses first wins; both checked together so
        // the overall cap can fire even when the per-test watchdog is
        // being reset on a busy stream.
        let now = Instant::now();
        let overall_fired = matches!(overall_deadline, Some(d) if now >= d);
        let per_test_fired = matches!(per_test_deadline, Some(d) if now >= d);
        if overall_fired || per_test_fired {
            // For per-test timeouts, report elapsed since this launch's
            // local arming. For the overall deadline, when the caller
            // supplied both `overall_deadline_abs` and `overall_timeout`
            // (the `test_filter` L2+ case) the abs deadline was anchored
            // by the orchestrator on the *first* per-binary
            // `build-finished` across all launches; recover that anchor
            // as `overall_deadline_abs - overall_timeout` so the reported
            // elapsed matches the configured overall budget rather than
            // this single launch's local clock.
            let elapsed = if overall_fired
                && !per_test_fired
                && let (Some(d), Some(t)) = (overall_deadline_abs, overall_timeout)
            {
                let anchor = d.checked_sub(t).unwrap_or(start);
                now.saturating_duration_since(anchor)
            } else {
                armed_at.unwrap_or(start).elapsed()
            };
            outcome = WaitOutcome::TimedOut(elapsed);
            break;
        }
        match rx.recv_timeout(tick) {
            Ok(Some(l)) => {
                // Arm both deadlines on the marker line (deferred-arming mode).
                let mut just_armed = false;
                if !armed
                    && let Some(pred) = arm_deadline
                    && pred(&l)
                {
                    let now = Instant::now();
                    armed_at = Some(now);
                    armed = true;
                    // Only set overall_deadline from the Duration here if
                    // the caller did NOT supply an absolute one. When
                    // overall_deadline_abs is Some it was already pinned
                    // at spawn and per-launch arming must not extend it.
                    if overall_deadline_abs.is_none() {
                        overall_deadline = overall_timeout.and_then(|t| now.checked_add(t));
                    }
                    per_test_deadline = per_test_timeout.and_then(|t| now.checked_add(t));
                    just_armed = true;
                }
                // After arming, an optional reset predicate refreshes ONLY the
                // per-test deadline on each matching line \u2014 the per-test
                // watchdog used by `cargo_test`'s `test_filter` mode, where
                // every test completion (`test ... ok|FAILED|ignored`)
                // should re-arm the clock so a long suite of fast tests
                // never trips the per-test budget. The overall deadline is
                // intentionally untouched: it bounds the whole run regardless
                // of how much per-test progress is being made.
                if armed
                    && !just_armed
                    && let (Some(t), Some(pred)) = (per_test_timeout, reset_deadline)
                    && pred(&l)
                {
                    per_test_deadline = Instant::now().checked_add(t);
                }
                on_stdout_line(&l);
                stdout_buf.push_str(&l);
                stdout_buf.push('\n');
            }
            Ok(None) => break, // stdout EOF
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }

    match outcome {
        WaitOutcome::Cancelled => {
            child.kill_tree();
            // Drop the receiver before joining so the stdout reader thread,
            // which may be parked in a bounded `tx.send`, observes a
            // disconnect and exits instead of blocking `join` forever.
            drop(rx);
            let _ = stdout_thread.join();
            let _ = stderr_thread.join();
            return Err(Box::new(CancelledError));
        }
        WaitOutcome::TimedOut(elapsed) => {
            child.kill_tree();
            // Drop the receiver before joining for the same reason as the
            // cancellation branch above: the stdout reader thread may be
            // parked in a bounded `tx.send`, and needs to observe a
            // disconnect to exit instead of blocking `join` forever.
            drop(rx);
            let _ = stdout_thread.join();
            let _ = stderr_thread.join();
            return Err(Box::new(TimeoutError { elapsed }));
        }
        WaitOutcome::Exited => {}
    }

    // Drain any lines the stdout thread buffered after we broke the loop.
    while let Ok(Some(l)) = rx.try_recv() {
        on_stdout_line(&l);
        stdout_buf.push_str(&l);
        stdout_buf.push('\n');
    }
    let _ = stdout_thread.join();

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
/// Convenience wrapper around [`run_cargo_with_timeout`] that passes
/// `timeout = None` ("wait forever"). Use [`run_cargo_with_timeout`] when
/// you need a wall-clock cap.
pub fn run_cargo(
    args: &[&str],
    working_dir: Option<&str>,
) -> Result<CargoOutput, Box<dyn std::error::Error>> {
    run_cargo_with_timeout(args, working_dir, None, None)
}

/// Run `cargo <args>` with a wall-clock budget; no streaming callback.
///
/// `arm_deadline` has the same meaning as in
/// [`run_cargo_streaming_with_timeout`]: when `Some`, the budget starts only
/// once a stdout line satisfies the predicate (used to time test execution
/// without the build phase).
pub fn run_cargo_with_timeout(
    args: &[&str],
    working_dir: Option<&str>,
    timeout: Option<Duration>,
    arm_deadline: Option<ArmDeadline<'_>>,
) -> Result<CargoOutput, Box<dyn std::error::Error>> {
    run_cargo_streaming_with_timeout(args, working_dir, timeout, arm_deadline, &mut |_| {})
}

/// Run `cargo <args>`, piping stdout **directly** into `dest_file` at the OS
/// level instead of buffering it in memory.
///
/// Use this for commands whose stdout can be very large (e.g. `cargo metadata`
/// in a workspace with thousands of transitive dependencies). Because the OS
/// plumbs the pipe straight to the file, the Rust process's heap is never
/// charged for the output.
///
/// If the thread-local cancel token is set mid-run, the **entire process
/// tree** is killed and [`CancelledError`] is returned. If a `timeout` is
/// supplied and elapses, the tree is killed and [`TimeoutError`] is
/// returned.
///
/// `CargoOutput::stdout` is always empty when this function is used; only
/// `stderr` and `exit_code` are meaningful in the returned value.
pub fn run_cargo_to_file(
    args: &[&str],
    working_dir: Option<&str>,
    dest_file: std::fs::File,
    timeout: Option<Duration>,
) -> Result<CargoOutput, Box<dyn std::error::Error>> {
    let (cargo_path, source) = resolve_cargo_binary();
    log_invocation(&cargo_path, source, working_dir, args);
    let mut cmd = Command::new(&cargo_path);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::from(dest_file)) // OS-level pipe → file, no heap buffer
        .stderr(Stdio::piped())
        .env_clear()
        .envs(build_subprocess_env());

    if let Some(dir) = working_dir {
        cmd.current_dir(dir);
    }

    let mut child = ManagedChild::spawn(&mut cmd)?;
    let stderr_pipe = child.take_stderr().expect("stderr is piped");

    // Drain stderr on a background thread to avoid deadlock when stdout
    // fills the pipe buffer while stderr is also accumulating.
    let stderr_thread = thread::spawn(move || -> String {
        let mut buf = String::new();
        let _ = BufReader::new(stderr_pipe).read_to_string(&mut buf);
        buf
    });

    let start = Instant::now();
    // checked_add: see run_cargo_streaming_with_timeout for rationale.
    let deadline = timeout.and_then(|t| start.checked_add(t));
    let status = loop {
        match child.try_wait()? {
            Some(s) => break s,
            None => {
                if is_cancelled() {
                    child.kill_tree();
                    let _ = stderr_thread.join();
                    return Err(Box::new(CancelledError));
                }
                if let Some(d) = deadline
                    && Instant::now() >= d
                {
                    child.kill_tree();
                    let _ = stderr_thread.join();
                    return Err(Box::new(TimeoutError {
                        elapsed: start.elapsed(),
                    }));
                }
                thread::sleep(Duration::from_millis(50));
            }
        }
    };

    let mut stderr_buf = stderr_thread.join().unwrap_or_default();
    let exit_code = status.code().unwrap_or(-1);
    if exit_code != 0 {
        maybe_append_working_dir_hint(&mut stderr_buf, working_dir);
        // No retry loop here (stdout is plumbed straight to a file), so
        // do the busy-file diagnostic in line. stdout is unavailable to
        // parse — pass an empty string and rely on stderr only.
        let report = collect_busy_holders(&stderr_buf, "");
        if !report.is_empty() {
            append_holder_report(&mut stderr_buf, &report);
        }
    }

    Ok(CargoOutput {
        stdout: String::new(), // nothing buffered; caller reads from dest_file
        stderr: stderr_buf,
        exit_code,
    })
}

/// Run an **arbitrary** subprocess (not necessarily cargo) to completion,
/// capturing its full stdout and stderr, under the same cancel-token and
/// optional wall-clock-deadline supervision used by the cargo runners.
///
/// The caller supplies a fully-prepared [`Command`] (program + args + any
/// process-specific env). This helper adds `Stdio::null()` on stdin and
/// `Stdio::piped()` on stdout/stderr, applies `working_dir` if given, then
/// spawns via [`ManagedChild`] so that cancellation and deadline expiry
/// kill the **entire process tree**. Stdout and stderr are drained on
/// background threads to prevent pipe-buffer deadlock if the child writes
/// substantial output on both streams.
///
/// Returns the captured output as a [`CargoOutput`] (its three fields —
/// `stdout`, `stderr`, `exit_code` — are generic enough to describe any
/// subprocess, so we reuse the type rather than introduce a parallel one).
///
/// Returns [`CancelledError`] if the cancel token fires, [`TimeoutError`]
/// if `timeout` is supplied and elapses. The child is tree-killed in both
/// cases.
pub fn run_subprocess_capture(
    mut cmd: Command,
    working_dir: Option<&str>,
    timeout: Option<Duration>,
) -> Result<CargoOutput, Box<dyn std::error::Error>> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(dir) = working_dir {
        cmd.current_dir(dir);
    }

    let mut child = ManagedChild::spawn(&mut cmd)?;
    let stdout_pipe = child.take_stdout().expect("stdout is piped");
    let stderr_pipe = child.take_stderr().expect("stderr is piped");

    let stdout_thread = thread::spawn(move || -> String {
        let mut buf = String::new();
        let _ = BufReader::new(stdout_pipe).read_to_string(&mut buf);
        buf
    });
    let stderr_thread = thread::spawn(move || -> String {
        let mut buf = String::new();
        let _ = BufReader::new(stderr_pipe).read_to_string(&mut buf);
        buf
    });

    let start = Instant::now();
    let deadline = timeout.and_then(|t| start.checked_add(t));
    let status = loop {
        match child.try_wait()? {
            Some(s) => break s,
            None => {
                if is_cancelled() {
                    child.kill_tree();
                    let _ = stdout_thread.join();
                    let _ = stderr_thread.join();
                    return Err(Box::new(CancelledError));
                }
                if let Some(d) = deadline
                    && Instant::now() >= d
                {
                    child.kill_tree();
                    let _ = stdout_thread.join();
                    let _ = stderr_thread.join();
                    return Err(Box::new(TimeoutError {
                        elapsed: start.elapsed(),
                    }));
                }
                thread::sleep(Duration::from_millis(50));
            }
        }
    };

    let stdout_buf = stdout_thread.join().unwrap_or_default();
    let stderr_buf = stderr_thread.join().unwrap_or_default();
    let exit_code = status.code().unwrap_or(-1);
    Ok(CargoOutput {
        stdout: stdout_buf,
        stderr: stderr_buf,
        exit_code,
    })
}

// ── tests ─────────────────────────────────────────────────────────────────────

/// Serializes every test (unit **or** integration) that reads or mutates the
/// process-global environment variables consulted by [`resolve_cargo_binary`]
/// and [`resolve_rustc_binary`]: `CARGO`, `RUSTC`, `CARGO_HOME`, `HOME`,
/// `USERPROFILE`.
///
/// Exposed `pub(crate)` so integration test binaries that mount `invoke.rs`
/// via `#[path]` can acquire the same lock, preventing a race where a unit
/// test temporarily sets `CARGO` to a fake path while an integration test is
/// mid-way through `run_cargo_streaming_with_timeout` calling
/// `resolve_cargo_binary`.
#[cfg(test)]
pub(crate) static TEST_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
mod tests {
    //! Tests for [`resolve_cargo_binary`] et al.
    //!
    //! These tests mutate process-global environment variables, so they
    //! serialize through [`super::TEST_ENV_LOCK`]. Each test snapshots the
    //! relevant vars up front and restores them on drop via [`EnvGuard`].
    use super::*;
    use std::ffi::{OsStr, OsString};
    use std::sync::Mutex;

    // Convenience alias so test bodies can still write `ENV_LOCK`.
    use super::TEST_ENV_LOCK as ENV_LOCK;

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

    /// Build the same env block a spawn site would, then read the `RUSTC`
    /// override that `apply_rustc_env_to_map` placed in it. Returns `None`
    /// if no override is present — distinguished from inherited values via
    /// the snapshot-then-mutate pattern in the call sites below.
    fn rustc_in_env(env: &BTreeMap<OsString, OsString>) -> Option<PathBuf> {
        env.get(OsStr::new("RUSTC")).map(PathBuf::from)
    }

    #[test]
    fn apply_rustc_env_pins_rustup_proxy_path() {
        let _g = ENV_LOCK.lock().unwrap();
        let guard = EnvGuard::snapshot(&["RUSTC", "CARGO_HOME", "HOME", "USERPROFILE"]);
        let cargo_home = unique_tempdir("apply_rustc_proxy");
        let bin_dir = cargo_home.join("bin");
        let rustc_path = write_fake_bin(&bin_dir, "rustc");
        write_fake_bin(&bin_dir, "rustup");
        guard.unset("RUSTC");
        guard.set("CARGO_HOME", &cargo_home);

        // Start from an empty map so we test the helper's pin behaviour in
        // isolation rather than the snapshot of the test process's env.
        let mut env: BTreeMap<OsString, OsString> = BTreeMap::new();
        apply_rustc_env_to_map(&mut env);
        assert_eq!(rustc_in_env(&env), Some(rustc_path));
    }

    #[test]
    fn apply_rustc_env_skips_when_rustc_already_set_by_user() {
        let _g = ENV_LOCK.lock().unwrap();
        let guard = EnvGuard::snapshot(&["RUSTC", "CARGO_HOME", "HOME", "USERPROFILE"]);
        // User-provided RUSTC takes precedence — we must not override it
        // (even if it points at a non-existent file; that's the user's call).
        guard.set("RUSTC", "/explicit/user/rustc");
        let cargo_home = unique_tempdir("apply_rustc_userset");
        let bin_dir = cargo_home.join("bin");
        write_fake_bin(&bin_dir, "rustc");
        write_fake_bin(&bin_dir, "rustup");
        guard.set("CARGO_HOME", &cargo_home);

        // Seed the map with the user's RUSTC the way build_subprocess_env
        // would (via std::env::vars_os).
        let mut env: BTreeMap<OsString, OsString> = BTreeMap::new();
        env.insert(
            OsString::from("RUSTC"),
            OsString::from("/explicit/user/rustc"),
        );
        apply_rustc_env_to_map(&mut env);
        assert_eq!(
            rustc_in_env(&env),
            Some(PathBuf::from("/explicit/user/rustc")),
            "must not override user-set RUSTC"
        );
    }

    #[test]
    fn apply_rustc_env_skips_for_path_lookup() {
        let _g = ENV_LOCK.lock().unwrap();
        let guard = EnvGuard::snapshot(&["RUSTC", "CARGO_HOME", "HOME", "USERPROFILE"]);
        let empty_home = unique_tempdir("apply_rustc_path");
        guard.unset("RUSTC");
        guard.set("CARGO_HOME", &empty_home);
        // No bin/rustc under this home → resolver returns PathLookup.

        let mut env: BTreeMap<OsString, OsString> = BTreeMap::new();
        apply_rustc_env_to_map(&mut env);
        assert!(
            rustc_in_env(&env).is_none(),
            "PathLookup must not pin a concrete RUSTC path"
        );
    }

    #[test]
    fn build_subprocess_env_includes_defaults_and_extra_env() {
        let _g = ENV_LOCK.lock().unwrap();
        let guard = EnvGuard::snapshot(&[
            "RUSTC",
            "CARGO_HOME",
            "HOME",
            "USERPROFILE",
            "CARGO_TERM_COLOR",
            "NO_COLOR",
            "RUSTFLAGS",
        ]);
        // Park the resolver in PathLookup so we don't pin RUSTC and complicate
        // the assertion — we only care about the defaults + extra-env wiring.
        let empty_home = unique_tempdir("build_env");
        guard.unset("RUSTC");
        guard.set("CARGO_HOME", &empty_home);
        guard.unset("CARGO_TERM_COLOR");
        guard.unset("NO_COLOR");
        guard.set("RUSTFLAGS", "-C overflow-checks=on");

        // Layered scenario: set a new var, override an inherited one, and
        // remove a built-in default. The block we hand to CreateProcess must
        // reflect the final state of all three operations.
        set_extra_env(vec![
            ("FIREBIRD_DUMP_MIR".into(), Some("1".into())),
            ("RUSTFLAGS".into(), Some("-C debuginfo=2".into())),
            ("NO_COLOR".into(), None),
        ]);
        let env = build_subprocess_env();
        set_extra_env(Vec::new());

        assert_eq!(
            env.get(OsStr::new("CARGO_TERM_COLOR")).cloned(),
            Some(OsString::from("never")),
            "built-in default missing"
        );
        assert_eq!(
            env.get(OsStr::new("FIREBIRD_DUMP_MIR")).cloned(),
            Some(OsString::from("1")),
            "extra env (new variable) missing"
        );
        assert_eq!(
            env.get(OsStr::new("RUSTFLAGS")).cloned(),
            Some(OsString::from("-C debuginfo=2")),
            "extra env must override inherited value"
        );
        assert!(
            !env.contains_key(OsStr::new("NO_COLOR")),
            "extra env null must remove the built-in default"
        );
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
    fn is_retry_safe_skips_leading_toolchain_override() {
        // A `+toolchain` token precedes the subcommand; retry-safety must be
        // judged on the subcommand, not the override.
        assert!(is_retry_safe(&["+nightly", "test"]));
        assert!(is_retry_safe(&["+ms-prod", "check", "--all-targets"]));
        assert!(!is_retry_safe(&["+nightly", "publish"]));
        // A lone toolchain token with no subcommand is not retry-safe.
        assert!(!is_retry_safe(&["+nightly"]));
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
