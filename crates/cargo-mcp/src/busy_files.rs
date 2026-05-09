// Copyright (c) Michael Grier. All rights reserved.

//! Diagnostics for "file in use" / "sharing violation" errors emitted by
//! `cargo` (and underlying tools like `rustc` and the Windows linker) when
//! another process holds an open handle on a file in `target/`.
//!
//! Two responsibilities:
//!
//! 1. **Path extraction** ([`extract_busy_paths`]) — cross-platform parser
//!    that scans cargo's stderr for backtick-quoted file paths reported as
//!    busy. Cargo formats these as e.g.:
//!
//!    ```text
//!    error: failed to remove file `target\debug\foo.exe`:
//!      The process cannot access the file because it is being used by
//!      another process. (os error 32)
//!    ```
//!
//! 2. **Holder query** ([`query_holders`]) — Windows-only call into the
//!    Restart Manager APIs (`rstrtmgr.dll`) that reports every process
//!    holding a handle on each given file (PID, executable name,
//!    application kind). On non-Windows hosts it returns one
//!    [`FileHolders`] entry per input path with `error = Some(...)`
//!    explaining that RM is Windows-only, so callers can render a
//!    uniform diagnostic without their own `cfg` gates.
//!
//! Combined, these power the file-busy diagnostic that cargo-mcp appends
//! to failed cargo output and surfaces as progress notifications between
//! retry attempts. Without this, the agent only sees Windows' generic
//! "being used by another process" error and has no signal pointing at the
//! actual culprit (rust-analyzer, a debugger, antivirus, the just-launched
//! `target\debug\foo.exe` itself, etc.).
//!
//! All Restart Manager calls are best-effort: any failure (RM not
//! available, access denied, file no longer exists) is reported per-file
//! in [`FileHolders::error`] rather than propagated, so a partial answer
//! is always returned.

use std::path::{Path, PathBuf};

// ── public types ─────────────────────────────────────────────────────────────

/// Coarse classification of a process holding a busy file, mirroring the
/// `RM_APP_TYPE` enum from the Windows Restart Manager API.
///
/// Kept as a stable Rust enum (rather than a raw integer) so non-Windows
/// callers and tests can match on it without `cfg` gates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppKind {
    /// Could not be classified.
    Unknown,
    /// GUI application with a top-level window.
    MainWindow,
    /// GUI application with no top-level window (background dialog, etc.).
    OtherWindow,
    /// Windows service.
    Service,
    /// Windows Explorer (`explorer.exe`).
    Explorer,
    /// Console (CLI) application.
    Console,
    /// Critical system process — Restart Manager will not attempt to stop it.
    Critical,
}

impl AppKind {
    /// Human-readable label for log/report output.
    pub fn label(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::MainWindow => "GUI app",
            Self::OtherWindow => "GUI app (background)",
            Self::Service => "service",
            Self::Explorer => "Explorer",
            Self::Console => "console",
            Self::Critical => "critical system process",
        }
    }
}

/// One process reported by the Restart Manager as holding a handle on the
/// queried file.
#[derive(Debug, Clone)]
pub struct ProcessHolder {
    /// Operating-system process ID.
    pub pid: u32,
    /// Application name as Restart Manager reports it. Typically the
    /// executable basename (e.g. `foo.exe`) for normal processes, or the
    /// service short name for services.
    pub app_name: String,
    /// Coarse application kind (used to tailor the suggested remediation —
    /// e.g. "stop the debugger" vs "stop the service").
    pub app_kind: AppKind,
}

/// Restart Manager result for a single busy file path.
#[derive(Debug, Clone)]
pub struct FileHolders {
    /// The file path that was queried.
    pub path: PathBuf,
    /// Processes holding open handles on `path` at query time. May be empty
    /// even when [`FileHolders::error`] is `None` (e.g. the offending
    /// process released the handle between the cargo error and our query).
    pub holders: Vec<ProcessHolder>,
    /// Set when Restart Manager returned an error or could not be invoked
    /// (e.g. on non-Windows hosts, or when the path no longer exists).
    /// Diagnostic-only — `holders` is still authoritative.
    pub error: Option<String>,
}

// ── path extraction (cross-platform) ─────────────────────────────────────────

/// Phrases that indicate a file-busy / sharing-violation condition in cargo
/// or rustc output. Kept in sync with [`crate::invoke::is_transient_busy_error`]
/// so the two detectors agree on what counts as "busy".
///
/// The two `(os error N)` markers are Win32-specific error numbers and are
/// gated to Windows in [`line_is_busy_indicator`] (mirroring the same
/// `cfg!(windows)` gate in `is_transient_busy_error`); the textual phrases
/// match on every platform because cargo prints them verbatim regardless of
/// host (e.g. when relaying the OS message from a remote build target).
const BUSY_INDICATORS: &[&str] = &[
    "being used by another process",
    "Access is denied",
    "access is denied",
    "sharing violation",
    "Sharing violation",
];

/// Win32-specific OS-error tags. Only treated as busy indicators on Windows
/// hosts; on other platforms `(os error 32)` could legitimately mean
/// `EPIPE`, etc.
const BUSY_INDICATORS_WINDOWS_ONLY: &[&str] = &["(os error 32)", "(os error 5)"];

/// Returns `true` if `line` contains any [`BUSY_INDICATORS`] phrase, or (on
/// Windows only) any [`BUSY_INDICATORS_WINDOWS_ONLY`] phrase.
fn line_is_busy_indicator(line: &str) -> bool {
    if BUSY_INDICATORS.iter().any(|p| line.contains(p)) {
        return true;
    }
    cfg!(windows)
        && BUSY_INDICATORS_WINDOWS_ONLY
            .iter()
            .any(|p| line.contains(p))
}

/// Extract every file path that cargo reported as busy in `stderr_or_stdout`.
///
/// Strategy: split the input into "blocks" delimited by `error:` /
/// `warning:` lines, then for each block that contains a busy indicator,
/// pull out every backtick-quoted substring. Cargo always wraps file paths
/// in ASCII backticks, so this is a tight signal with very few false
/// positives.
///
/// The returned vector preserves first-seen order and is deduplicated:
/// multiple cargo errors against the same file (common when several
/// targets in the workspace try to write the same artefact) collapse to a
/// single entry, so downstream Restart Manager queries don't repeat work.
///
/// Returns an empty vector when no busy phrase is present, even if there
/// are backtick-quoted strings — we never harvest paths from unrelated
/// errors like "unresolved import \`foo\`".
pub fn extract_busy_paths(stderr_or_stdout: &str) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    let lines: Vec<&str> = stderr_or_stdout.lines().collect();

    // Identify block boundaries: any line whose first non-whitespace token
    // is `error:` or `warning:` starts a new diagnostic block. Everything
    // up to the next such line belongs to that block.
    let mut block_starts: Vec<usize> = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("error:")
            || trimmed.starts_with("warning:")
            || trimmed.starts_with("error[")
        {
            block_starts.push(i);
        }
    }
    // Treat the whole input as a single block if no diagnostic header was
    // found — cargo sometimes emits raw OS errors without an `error:` prefix
    // (e.g. when the linker fails directly).
    if block_starts.is_empty() {
        block_starts.push(0);
    }

    for (idx, &start) in block_starts.iter().enumerate() {
        let end = block_starts.get(idx + 1).copied().unwrap_or(lines.len());
        let block = &lines[start..end];
        if !block.iter().any(|l| line_is_busy_indicator(l)) {
            continue;
        }
        for line in block {
            harvest_backtick_paths(line, &mut out);
        }
    }

    // Dedupe while preserving first-seen order.
    let mut seen = std::collections::HashSet::new();
    out.retain(|p| seen.insert(p.clone()));
    out
}

/// Push every backtick-quoted substring on `line` into `out` as a `PathBuf`.
///
/// Skips empty backtick pairs (`` `` ``) and entries that don't look like
/// file paths (e.g. a single bare identifier from a Rust diagnostic like
/// `` `foo` ``). Heuristic for "looks like a path": contains `/`, `\`, or
/// `.` — file paths in cargo's "failed to ..." messages always carry at
/// least one of those.
fn harvest_backtick_paths(line: &str, out: &mut Vec<PathBuf>) {
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'`' {
            i += 1;
            continue;
        }
        let start = i + 1;
        let mut j = start;
        while j < bytes.len() && bytes[j] != b'`' {
            j += 1;
        }
        if j >= bytes.len() {
            break;
        }
        let inner = &line[start..j];
        if !inner.is_empty()
            && (inner.contains('/') || inner.contains('\\') || inner.contains('.'))
        {
            out.push(PathBuf::from(inner));
        }
        i = j + 1;
    }
}

// ── Restart Manager query ────────────────────────────────────────────────────

/// Ask the OS which processes hold open handles on each path in `paths`.
///
/// On Windows this calls into the Restart Manager (`rstrtmgr.dll`). On
/// every other platform this returns a vector of [`FileHolders`] entries
/// each carrying `error = Some("Restart Manager is Windows-only")` so
/// callers can render a uniform diagnostic instead of branching.
///
/// Best-effort: a missing or inaccessible `rstrtmgr.dll`, an access-denied
/// session start, or a path that has already been deleted is reported per
/// entry rather than propagated. The function never panics.
pub fn query_holders(paths: &[&Path]) -> Vec<FileHolders> {
    #[cfg(windows)]
    {
        windows_impl::query_holders(paths)
    }
    #[cfg(not(windows))]
    {
        paths
            .iter()
            .map(|p| FileHolders {
                path: p.to_path_buf(),
                holders: Vec::new(),
                error: Some("Restart Manager diagnostics are Windows-only".into()),
            })
            .collect()
    }
}

// ── formatting ───────────────────────────────────────────────────────────────

/// Render `report` as a multi-line block suitable for appending to cargo's
/// captured stderr. Returns an empty string when `report` is empty.
///
/// Format (one entry per file):
///
/// ```text
/// cargo-mcp: 2 file(s) reported in use by other processes:
///   target\debug\foo.exe
///     PID 12345 - foo.exe (console)
///     PID  6789 - rust-analyzer-proc-macro-srv.exe (console)
///   target\debug\bar.dll
///     (no current holders - likely a transient AV / indexer scan)
/// ```
pub fn format_full_report(report: &[FileHolders]) -> String {
    if report.is_empty() {
        return String::new();
    }
    let mut s = String::new();
    s.push_str(&format!(
        "cargo-mcp: {} file(s) reported in use by other processes:\n",
        report.len()
    ));
    for entry in report {
        s.push_str(&format!("  {}\n", entry.path.display()));
        if let Some(err) = &entry.error {
            s.push_str(&format!("    (Restart Manager: {err})\n"));
            continue;
        }
        if entry.holders.is_empty() {
            s.push_str(
                "    (no current holders - likely a transient AV / indexer scan)\n",
            );
            continue;
        }
        for h in &entry.holders {
            s.push_str(&format!(
                "    PID {pid} - {name} ({kind})\n",
                pid = h.pid,
                name = h.app_name,
                kind = h.app_kind.label(),
            ));
        }
    }
    s
}

/// One-line summary of `report` suitable for a streaming progress line
/// emitted between retry attempts. Returns `None` when no holders were
/// identified across any file (i.e. the report adds no information beyond
/// the existing "transient file-busy error; retrying ..." line).
pub fn format_short_summary(report: &[FileHolders]) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    for entry in report {
        for h in &entry.holders {
            parts.push(format!("{} (PID {})", h.app_name, h.pid));
        }
    }
    if parts.is_empty() {
        return None;
    }
    parts.sort();
    parts.dedup();
    // Cap to keep the progress line readable.
    const MAX: usize = 4;
    let extra = parts.len().saturating_sub(MAX);
    let shown: Vec<_> = parts.into_iter().take(MAX).collect();
    let mut joined = shown.join(", ");
    if extra > 0 {
        joined.push_str(&format!(" (+{extra} more)"));
    }
    Some(format!("cargo-mcp: file held by: {joined}"))
}

// ── Windows Restart Manager FFI ──────────────────────────────────────────────

#[cfg(windows)]
mod windows_impl {
    //! Minimal hand-rolled FFI bindings for the Restart Manager surface we
    //! need. Kept here (rather than pulling in `windows-sys`) because the
    //! API is tiny and adding a heavyweight dependency for one diagnostic
    //! is not worth the build-time/binary-size cost.
    //!
    //! All functions are documented at:
    //! <https://learn.microsoft.com/windows/win32/api/restartmanager/>

    use super::{AppKind, FileHolders, ProcessHolder};
    use std::os::windows::ffi::OsStrExt;
    use std::path::{Path, PathBuf};
    use std::ptr;

    // ── Win32 constants ──────────────────────────────────────────────────

    const ERROR_SUCCESS: u32 = 0;
    const ERROR_MORE_DATA: u32 = 234;
    const CCH_RM_SESSION_KEY: usize = 32;
    const CCH_RM_MAX_APP_NAME: usize = 255;
    const CCH_RM_MAX_SVC_NAME: usize = 63;

    // RM_APP_TYPE values (see RestartManager.h). RmUnknownApp (0) is
    // handled implicitly by the wildcard arm in `classify`.
    const RM_MAIN_WINDOW: u32 = 1;
    const RM_OTHER_WINDOW: u32 = 2;
    const RM_SERVICE: u32 = 3;
    const RM_EXPLORER: u32 = 4;
    const RM_CONSOLE: u32 = 5;
    const RM_CRITICAL: u32 = 1000;

    fn classify(app_type: u32) -> AppKind {
        match app_type {
            RM_MAIN_WINDOW => AppKind::MainWindow,
            RM_OTHER_WINDOW => AppKind::OtherWindow,
            RM_SERVICE => AppKind::Service,
            RM_EXPLORER => AppKind::Explorer,
            RM_CONSOLE => AppKind::Console,
            RM_CRITICAL => AppKind::Critical,
            // Covers RM_UNKNOWN_APP (0) and any future RM_APP_TYPE values
            // Microsoft might add. Treat as `Unknown` rather than failing.
            _ => AppKind::Unknown,
        }
    }

    // ── Win32 structs ────────────────────────────────────────────────────

    #[repr(C)]
    #[derive(Clone, Copy)]
    #[allow(non_snake_case)]
    struct Filetime {
        dw_low_date_time: u32,
        dw_high_date_time: u32,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct RmUniqueProcess {
        dw_process_id: u32,
        process_start_time: Filetime,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct RmProcessInfo {
        process: RmUniqueProcess,
        str_app_name: [u16; CCH_RM_MAX_APP_NAME + 1],
        str_service_short_name: [u16; CCH_RM_MAX_SVC_NAME + 1],
        application_type: u32,
        app_status: u32,
        ts_session_id: u32,
        b_restartable: i32, // BOOL
    }

    // ── Win32 imports ────────────────────────────────────────────────────

    #[link(name = "rstrtmgr")]
    unsafe extern "system" {
        fn RmStartSession(
            p_session_handle: *mut u32,
            dw_session_flags: u32,
            str_session_key: *mut u16,
        ) -> u32;

        fn RmEndSession(dw_session_handle: u32) -> u32;

        fn RmRegisterResources(
            dw_session_handle: u32,
            n_files: u32,
            rgs_filenames: *const *const u16,
            n_applications: u32,
            rg_applications: *const RmUniqueProcess,
            n_services: u32,
            rgs_service_names: *const *const u16,
        ) -> u32;

        fn RmGetList(
            dw_session_handle: u32,
            pn_proc_info_needed: *mut u32,
            pn_proc_info: *mut u32,
            rg_affected_apps: *mut RmProcessInfo,
            lpdw_reboot_reasons: *mut u32,
        ) -> u32;
    }

    // ── error formatting ─────────────────────────────────────────────────

    // FormatMessageW flags / source.
    const FORMAT_MESSAGE_ALLOCATE_BUFFER: u32 = 0x0000_0100;
    const FORMAT_MESSAGE_FROM_SYSTEM: u32 = 0x0000_1000;
    const FORMAT_MESSAGE_IGNORE_INSERTS: u32 = 0x0000_0200;
    // MAKELANGID(LANG_ENGLISH=0x09, SUBLANG_ENGLISH_US=0x01) -> 0x0409.
    // We try en-US first because the rest of cargo-mcp parses
    // English error text elsewhere; if en-US isn't installed on this
    // host `format_win32_error` falls back to the system default
    // language (`LANG_NEUTRAL_SUBLANG_DEFAULT`, 0).
    const LANG_ENGLISH_US: u32 = 0x0409;
    const LANG_SYSTEM_DEFAULT: u32 = 0;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn FormatMessageW(
            dw_flags: u32,
            lp_source: *const std::ffi::c_void,
            dw_message_id: u32,
            dw_language_id: u32,
            lp_buffer: *mut u16, // when ALLOCATE_BUFFER: *mut *mut u16 cast
            n_size: u32,
            arguments: *const std::ffi::c_void,
        ) -> u32;

        fn LocalFree(h_mem: *mut std::ffi::c_void) -> *mut std::ffi::c_void;
    }

    /// Translate a Win32 error code (as returned by Restart Manager) into
    /// a localized message via `FormatMessageW`. Returns `None` if
    /// `FormatMessageW` itself fails (e.g. unknown code).
    ///
    /// Tries U.S. English first (`MAKELANGID(LANG_ENGLISH,
    /// SUBLANG_ENGLISH_US)`) so the text is predictable across hosts and
    /// matches the English error patterns that the rest of cargo-mcp
    /// parses; falls back to the system default language if en-US is not
    /// installed on this machine.
    ///
    /// Uses `FORMAT_MESSAGE_ALLOCATE_BUFFER` so the OS picks the buffer
    /// size; this avoids the trim/retry dance with a fixed-size stack
    /// buffer for messages that exceed it. The buffer is freed via
    /// `LocalFree` per the API contract.
    fn format_win32_error(code: u32) -> Option<String> {
        format_win32_error_in(code, LANG_ENGLISH_US)
            .or_else(|| format_win32_error_in(code, LANG_SYSTEM_DEFAULT))
    }

    fn format_win32_error_in(code: u32, lang_id: u32) -> Option<String> {
        let mut buf_ptr: *mut u16 = ptr::null_mut();
        let n = unsafe {
            FormatMessageW(
                FORMAT_MESSAGE_ALLOCATE_BUFFER
                    | FORMAT_MESSAGE_FROM_SYSTEM
                    | FORMAT_MESSAGE_IGNORE_INSERTS,
                ptr::null(),
                code,
                lang_id,
                // With ALLOCATE_BUFFER, the API expects a *pointer to* a
                // pointer (it writes the allocated address back). Cast
                // accordingly.
                (&mut buf_ptr as *mut *mut u16) as *mut u16,
                0,
                ptr::null(),
            )
        };
        if n == 0 || buf_ptr.is_null() {
            return None;
        }
        // SAFETY: FormatMessageW wrote `n` UTF-16 code units to `buf_ptr`.
        let slice = unsafe { std::slice::from_raw_parts(buf_ptr, n as usize) };
        let mut s = String::from_utf16_lossy(slice);
        // FormatMessage typically appends "\r\n" — trim trailing
        // whitespace so it doesn't break our single-line log output.
        let trimmed_len = s.trim_end().len();
        s.truncate(trimmed_len);
        unsafe { LocalFree(buf_ptr as *mut std::ffi::c_void) };
        if s.is_empty() { None } else { Some(s) }
    }

    /// Format a Restart Manager error code as a single human-readable
    /// string, e.g.
    /// `"RmStartSession failed: The handle is invalid. (code 6)"`.
    ///
    /// Always includes the numeric code as a stable token so support
    /// requests can search for it regardless of the host's locale.
    fn rm_err(api: &str, code: u32) -> String {
        match format_win32_error(code) {
            Some(msg) => format!("{api} failed: {msg} (code {code})"),
            None => format!("{api} failed: code {code} (no system message)"),
        }
    }

    // Test-only re-export so the outer `tests` module can exercise
    // `rm_err` without making it part of the module's normal API.
    #[cfg(test)]
    pub(super) fn rm_err_for_tests(api: &str, code: u32) -> String {
        rm_err(api, code)
    }

    #[cfg(test)]
    pub(super) fn strip_unc_prefix_for_tests(p: PathBuf) -> PathBuf {
        strip_unc_prefix(p)
    }

    // ── public entry point ───────────────────────────────────────────────

    /// One Restart Manager session per file so each holder list maps back
    /// to a specific path. RM's per-session output is a *union* across all
    /// registered resources, so multiplexing files through one session
    /// would lose the path → holders mapping that the diagnostic depends
    /// on.
    pub(super) fn query_holders(paths: &[&Path]) -> Vec<FileHolders> {
        paths.iter().map(|p| query_one(p)).collect()
    }

    fn query_one(path: &Path) -> FileHolders {
        match query_one_inner(path) {
            Ok(holders) => FileHolders {
                path: path.to_path_buf(),
                holders,
                error: None,
            },
            Err(msg) => FileHolders {
                path: path.to_path_buf(),
                holders: Vec::new(),
                error: Some(msg),
            },
        }
    }

    fn query_one_inner(path: &Path) -> Result<Vec<ProcessHolder>, String> {
        // Convert path to a NUL-terminated UTF-16 string. RM expects an
        // absolute path; relative paths sometimes work but are unreliable.
        let abs: PathBuf = match std::fs::canonicalize(path) {
            Ok(p) => strip_unc_prefix(p),
            // Fall back to the original path; if RM can't resolve it the
            // session will simply return an empty holder list.
            Err(_) => path.to_path_buf(),
        };
        let mut wide: Vec<u16> = abs.as_os_str().encode_wide().collect();
        wide.push(0);

        // Start a session.
        let mut handle: u32 = 0;
        let mut session_key = [0u16; CCH_RM_SESSION_KEY + 1];
        let rc = unsafe { RmStartSession(&mut handle, 0, session_key.as_mut_ptr()) };
        if rc != ERROR_SUCCESS {
            return Err(rm_err("RmStartSession", rc));
        }

        // RAII: ensure RmEndSession runs on every exit path below.
        struct SessionGuard(u32);
        impl Drop for SessionGuard {
            fn drop(&mut self) {
                let _ = unsafe { RmEndSession(self.0) };
            }
        }
        let _guard = SessionGuard(handle);

        // Register the file as a resource.
        let file_ptrs: [*const u16; 1] = [wide.as_ptr()];
        let rc = unsafe {
            RmRegisterResources(
                handle,
                1,
                file_ptrs.as_ptr(),
                0,
                ptr::null(),
                0,
                ptr::null(),
            )
        };
        if rc != ERROR_SUCCESS {
            return Err(rm_err("RmRegisterResources", rc));
        }

        // First call: probe required buffer size.
        let mut needed: u32 = 0;
        let mut count: u32 = 0;
        let mut reasons: u32 = 0;
        let rc = unsafe {
            RmGetList(handle, &mut needed, &mut count, ptr::null_mut(), &mut reasons)
        };
        // ERROR_SUCCESS with needed == 0 means no holders.
        // ERROR_MORE_DATA means we need to retry with a buffer of `needed`.
        match rc {
            ERROR_SUCCESS => return Ok(Vec::new()),
            ERROR_MORE_DATA => { /* fall through */ }
            other => return Err(rm_err("RmGetList probe", other)),
        }
        if needed == 0 {
            return Ok(Vec::new());
        }

        // Fetch the list. RmGetList is *not* a streaming API: each call
        // returns the entire current snapshot of affected processes, and
        // signals "the snapshot grew between calls" by returning
        // ERROR_MORE_DATA together with an updated `needed` count. We
        // therefore loop, growing the buffer to whatever RM currently
        // wants, until we either get ERROR_SUCCESS or detect that
        // looping further can't make progress.
        //
        // Termination conditions (in priority order):
        //   1. ERROR_SUCCESS  -> success path, take the buffer.
        //   2. Some non-MORE_DATA error  -> hard fail with rm_err.
        //   3. ERROR_MORE_DATA but `needed` did NOT strictly grow ->
        //      RM is misbehaving (it told us "bigger" without telling
        //      us *how much* bigger); bail rather than spin.
        //   4. `needed` exceeds a sanity ceiling -> a runaway system
        //      where literally every process opens this file; bail
        //      with a clear diagnostic.
        //
        // Note: there is no fixed cap on loop iterations. As long as RM
        // is honestly reporting genuine growth, we keep up with it.
        // Sanity ceiling on the per-file affected-process count. RM
        // returns one entry per holder on the system; even on a busy host
        // a single file is realistically held by single-digit numbers of
        // processes, and an answer in the tens of thousands almost
        // certainly indicates a runaway snapshot or RM bug. The exact
        // number is arbitrary -- it just needs to be far above any
        // plausible real answer and far below anything that would let us
        // burn unbounded memory.
        const MAX_HOLDERS: u32 = 65_536;
        let (buf, final_count) = loop {
            if needed > MAX_HOLDERS {
                return Err(format!(
                    "RmGetList reported {needed} affected processes \
                     (exceeds sanity limit {MAX_HOLDERS})"
                ));
            }
            let prev_needed = needed;
            let mut buf: Vec<RmProcessInfo> = Vec::with_capacity(needed as usize);
            count = needed;
            let rc = unsafe {
                RmGetList(
                    handle,
                    &mut needed,
                    &mut count,
                    buf.as_mut_ptr(),
                    &mut reasons,
                )
            };
            match rc {
                ERROR_SUCCESS => break (buf, count),
                ERROR_MORE_DATA => {
                    // RM updated `needed` to the now-required size.
                    // If it didn't actually grow, RM is telling us
                    // "bigger" without making progress; refuse to spin.
                    if needed <= prev_needed {
                        return Err(format!(
                            "RmGetList kept asking for more space \
                             without growing the request \
                             (stuck at {needed} entries)"
                        ));
                    }
                    // Otherwise loop and reallocate to the new size.
                }
                other => return Err(rm_err("RmGetList fetch", other)),
            }
        };
        let mut buf = buf;
        // SAFETY: RM populated `final_count` valid entries in `buf`.
        unsafe { buf.set_len(final_count as usize) };

        Ok(buf
            .into_iter()
            .map(|info| ProcessHolder {
                pid: info.process.dw_process_id,
                app_name: read_wide_string(&info.str_app_name),
                app_kind: classify(info.application_type),
            })
            .collect())
    }

    /// Strip the `\\?\` verbatim prefix that `canonicalize` returns on
    /// Windows so log output is human-readable, and (more importantly)
    /// so verbatim UNC paths get rewritten back to a real UNC form
    /// (`\\?\UNC\server\share\...` -> `\\server\share\...`) instead of
    /// the invalid `UNC\server\share\...` that a naive prefix strip
    /// would produce. Restart Manager accepts the verbatim form, so
    /// strictly cosmetic for local-disk paths; required for
    /// correctness for verbatim UNC paths if they ever reach us.
    ///
    /// Operates on path [`Component`]s rather than a lossy
    /// `to_string_lossy` round-trip so paths containing non-Unicode
    /// `OsStr` bytes are preserved exactly.
    fn strip_unc_prefix(p: PathBuf) -> PathBuf {
        use std::ffi::OsString;
        use std::path::{Component, Prefix};

        let mut comps = p.components();
        let Some(Component::Prefix(pc)) = comps.next() else {
            return p;
        };

        // Build the new prefix as an OsString so non-Unicode bytes in
        // the server/share names survive intact.
        let head: OsString = match pc.kind() {
            Prefix::VerbatimDisk(letter) => {
                // VerbatimDisk's letter is a guaranteed ASCII byte.
                let mut s = OsString::new();
                s.push(format!("{}:\\", letter as char));
                s
            }
            Prefix::VerbatimUNC(server, share) => {
                let mut s = OsString::from(r"\\");
                s.push(server);
                s.push(r"\");
                s.push(share);
                s.push(r"\");
                s
            }
            // Anything else (plain Disk, UNC, DeviceNS, or
            // Verbatim(<volume GUID>)) is already either non-verbatim
            // or unsafe to rewrite. Leave the path alone.
            _ => return p,
        };

        let mut rebuilt = PathBuf::from(head);
        for c in comps {
            // The component immediately after a Prefix on an absolute
            // path is RootDir; the head we just built already contains
            // the trailing separator, so skip it to avoid `\\?\` style
            // duplication.
            if matches!(c, Component::RootDir) {
                continue;
            }
            rebuilt.push(c.as_os_str());
        }
        rebuilt
    }

    /// Read a NUL-terminated UTF-16 string from a fixed-size buffer.
    fn read_wide_string(buf: &[u16]) -> String {
        let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        String::from_utf16_lossy(&buf[..len])
    }
}

// ── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_path_from_typical_cargo_busy_error() {
        let stderr = "\
error: failed to remove file `target\\debug\\foo.exe`:
  The process cannot access the file because it is being used by \
another process. (os error 32)
";
        let paths = extract_busy_paths(stderr);
        assert_eq!(paths, vec![PathBuf::from("target\\debug\\foo.exe")]);
    }

    #[test]
    fn extracts_path_from_access_denied_error() {
        let stderr = "\
error: failed to write `target\\debug\\foo.pdb`: Access is denied. (os error 5)
";
        let paths = extract_busy_paths(stderr);
        assert_eq!(paths, vec![PathBuf::from("target\\debug\\foo.pdb")]);
    }

    #[test]
    fn extracts_multiple_paths_from_separate_error_blocks() {
        let stderr = "\
error: failed to remove file `target/debug/a.exe`:
  being used by another process (os error 32)
error: failed to remove file `target/debug/b.exe`:
  being used by another process (os error 32)
";
        let paths = extract_busy_paths(stderr);
        assert_eq!(
            paths,
            vec![
                PathBuf::from("target/debug/a.exe"),
                PathBuf::from("target/debug/b.exe")
            ]
        );
    }

    #[test]
    fn dedupes_same_path_reported_twice() {
        let stderr = "\
error: failed to remove file `target/debug/dup.exe`:
  being used by another process (os error 32)
error: failed to remove file `target/debug/dup.exe`:
  being used by another process (os error 32)
";
        let paths = extract_busy_paths(stderr);
        assert_eq!(paths, vec![PathBuf::from("target/debug/dup.exe")]);
    }

    #[test]
    fn ignores_backtick_identifiers_in_unrelated_errors() {
        // A Rust diagnostic with `foo` is not a path and the block has no
        // busy indicator, so nothing should be extracted.
        let stderr = "error[E0432]: unresolved import `foo::bar`\n";
        assert!(extract_busy_paths(stderr).is_empty());
    }

    #[test]
    fn ignores_backticks_in_blocks_without_busy_indicator() {
        // Path-shaped backticks are present but no busy phrase — must not
        // harvest anything.
        let stderr = "\
error: cannot find type `crate::foo::Bar` in this scope
  --> src/lib.rs:10:5
";
        assert!(extract_busy_paths(stderr).is_empty());
    }

    #[test]
    fn handles_empty_input() {
        assert!(extract_busy_paths("").is_empty());
    }

    #[test]
    fn handles_input_with_no_diagnostic_header() {
        // Bare OS error from the linker without a leading `error:` prefix.
        let stderr = "LINK : fatal error LNK1104: cannot open file 'foo.exe' \
                      (os error 32) being used by another process";
        // `'foo.exe'` uses single quotes, not backticks, so we don't pick
        // it up — but we also must not crash. This documents that Cargo's
        // backtick convention is required for path harvesting.
        let paths = extract_busy_paths(stderr);
        assert!(paths.is_empty(), "expected no paths, got {paths:?}");
    }

    #[test]
    fn skips_empty_or_non_path_backtick_pairs() {
        let stderr = "\
error: failed to remove file ``:
  being used by another process (os error 32)
error: failed to remove file `foo`:
  being used by another process (os error 32)
";
        // Empty backticks → skip. Bare `foo` (no slash, dot, or backslash)
        // → skip (not a path).
        assert!(extract_busy_paths(stderr).is_empty());
    }

    #[test]
    fn format_short_summary_returns_none_when_no_holders() {
        let report = vec![FileHolders {
            path: PathBuf::from("a"),
            holders: Vec::new(),
            error: None,
        }];
        assert!(format_short_summary(&report).is_none());
    }

    #[test]
    fn format_short_summary_lists_holders() {
        let report = vec![FileHolders {
            path: PathBuf::from("a"),
            holders: vec![
                ProcessHolder {
                    pid: 1,
                    app_name: "foo.exe".into(),
                    app_kind: AppKind::Console,
                },
                ProcessHolder {
                    pid: 2,
                    app_name: "bar.exe".into(),
                    app_kind: AppKind::MainWindow,
                },
            ],
            error: None,
        }];
        let s = format_short_summary(&report).unwrap();
        assert!(s.contains("foo.exe (PID 1)"));
        assert!(s.contains("bar.exe (PID 2)"));
    }

    #[test]
    fn format_short_summary_caps_at_four_with_overflow_note() {
        let mut holders = Vec::new();
        for i in 1..=7u32 {
            holders.push(ProcessHolder {
                pid: i,
                app_name: format!("app{i}.exe"),
                app_kind: AppKind::Console,
            });
        }
        let report = vec![FileHolders {
            path: PathBuf::from("a"),
            holders,
            error: None,
        }];
        let s = format_short_summary(&report).unwrap();
        assert!(s.contains("(+3 more)"), "expected overflow tag in {s:?}");
    }

    #[test]
    fn format_full_report_includes_path_and_holders() {
        let report = vec![FileHolders {
            path: PathBuf::from("target/debug/foo.exe"),
            holders: vec![ProcessHolder {
                pid: 12345,
                app_name: "foo.exe".into(),
                app_kind: AppKind::Console,
            }],
            error: None,
        }];
        let s = format_full_report(&report);
        assert!(s.contains("1 file(s) reported in use"));
        assert!(s.contains("target/debug/foo.exe"));
        assert!(s.contains("PID 12345"));
        assert!(s.contains("foo.exe"));
        assert!(s.contains("console"));
    }

    #[test]
    fn format_full_report_handles_no_holders() {
        let report = vec![FileHolders {
            path: PathBuf::from("a"),
            holders: Vec::new(),
            error: None,
        }];
        let s = format_full_report(&report);
        assert!(s.contains("no current holders"));
    }

    #[test]
    fn format_full_report_handles_error() {
        let report = vec![FileHolders {
            path: PathBuf::from("a"),
            holders: Vec::new(),
            error: Some("something went wrong".into()),
        }];
        let s = format_full_report(&report);
        assert!(s.contains("Restart Manager: something went wrong"));
    }

    #[test]
    fn format_full_report_empty_returns_empty_string() {
        assert!(format_full_report(&[]).is_empty());
    }

    #[cfg(not(windows))]
    #[test]
    fn query_holders_on_non_windows_returns_error_per_path() {
        let p = PathBuf::from("/tmp/does-not-matter");
        let result = query_holders(&[p.as_path()]);
        assert_eq!(result.len(), 1);
        assert!(result[0].error.is_some());
        assert!(result[0].holders.is_empty());
    }

    #[cfg(windows)]
    #[test]
    fn query_holders_on_windows_handles_empty_input() {
        let result = query_holders(&[]);
        assert!(result.is_empty());
    }

    #[cfg(windows)]
    #[test]
    fn query_holders_on_windows_handles_unheld_file() {
        // A freshly-created temp file with no other openers must come back
        // with zero holders and no error. This exercises the full RM round
        // trip without depending on any specific running process.
        let tmp = std::env::temp_dir().join(format!(
            "cargo-mcp-busy-test-{}.tmp",
            std::process::id()
        ));
        std::fs::write(&tmp, b"x").unwrap();
        let result = query_holders(&[tmp.as_path()]);
        let _ = std::fs::remove_file(&tmp);
        assert_eq!(result.len(), 1);
        // Either: no error and no holders, OR an RM-side error (acceptable
        // on locked-down CI agents). The contract is "best effort".
        if result[0].error.is_none() {
            assert!(
                result[0].holders.is_empty(),
                "unexpected holders for temp file: {:?}",
                result[0].holders
            );
        }
    }

    #[cfg(windows)]
    #[test]
    fn rm_err_includes_localized_message_for_known_code() {
        // ERROR_FILE_NOT_FOUND (2) is universally defined and has a
        // localized FormatMessage text on every Windows install. We only
        // assert structural properties (the API name, the numeric code,
        // and that *some* descriptive text was found) so the test is not
        // sensitive to the host's display language.
        let s = super::windows_impl::rm_err_for_tests("RmTest", 2);
        assert!(s.starts_with("RmTest failed:"), "unexpected prefix: {s:?}");
        // Two valid shapes:
        //   success:  "RmTest failed: <localized message> (code 2)"
        //   fallback: "RmTest failed: code 2 (no system message)"
        // Either way assert there's a non-empty descriptive body --
        // not just the prefix and the trailing code.
        let success = s.strip_prefix("RmTest failed: ")
            .and_then(|rest| rest.strip_suffix(" (code 2)"))
            .map(|msg| !msg.trim().is_empty());
        let fallback = s == "RmTest failed: code 2 (no system message)";
        assert!(
            success == Some(true) || fallback,
            "expected localized body or explicit fallback marker: {s:?}"
        );
    }

    #[cfg(windows)]
    #[test]
    fn rm_err_falls_back_for_unknown_code() {
        // 0xDEAD_BEEF is not a real Win32 error; FormatMessage should
        // return 0 and rm_err should emit the fallback form.
        let s = super::windows_impl::rm_err_for_tests("RmTest", 0xDEAD_BEEF);
        assert!(
            s.contains("3735928559") && s.contains("no system message"),
            "missing fallback markers: {s:?}"
        );
    }

    #[cfg(windows)]
    #[test]
    fn strip_unc_prefix_rewrites_verbatim_disk() {
        let p = PathBuf::from(r"\\?\C:\Users\me\file.txt");
        let out = super::windows_impl::strip_unc_prefix_for_tests(p);
        assert_eq!(out, PathBuf::from(r"C:\Users\me\file.txt"));
    }

    #[cfg(windows)]
    #[test]
    fn strip_unc_prefix_rewrites_verbatim_unc() {
        // \\?\UNC\server\share\dir\file -> \\server\share\dir\file
        let p = PathBuf::from(r"\\?\UNC\server\share\dir\file.dll");
        let out = super::windows_impl::strip_unc_prefix_for_tests(p);
        assert_eq!(out, PathBuf::from(r"\\server\share\dir\file.dll"));
    }

    #[cfg(windows)]
    #[test]
    fn strip_unc_prefix_leaves_plain_disk_alone() {
        let p = PathBuf::from(r"D:\projects\thing.exe");
        let out = super::windows_impl::strip_unc_prefix_for_tests(p.clone());
        assert_eq!(out, p);
    }

    #[cfg(windows)]
    #[test]
    fn strip_unc_prefix_leaves_volume_guid_alone() {
        // Verbatim(<volume GUID>) is not safe to rewrite; pass through.
        let p = PathBuf::from(r"\\?\Volume{12345678-0000-0000-0000-000000000000}\file");
        let out = super::windows_impl::strip_unc_prefix_for_tests(p.clone());
        assert_eq!(out, p);
    }
}
