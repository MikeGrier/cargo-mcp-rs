// Copyright (c) Michael Grier. All rights reserved.

//! Safe wrapper over the Windows **Restart Manager** API — answers the
//! question *"which process is holding this file open?"*.
//!
//! This module is the **only** place in `cargo-mcp` that contains
//! `unsafe` code in the production binary. It is deliberately housed in
//! its own subdirectory so a casual reader can see the entire
//! unsafe-bearing surface area in one place.
//!
//! # Public API
//!
//! ```ignore
//! use cargo_mcp::rm::{who_holds, Holders};
//! let report: Vec<Holders> = who_holds(&[std::path::Path::new("target/debug/foo.exe")]);
//! ```
//!
//! [`who_holds`] is cross-platform: on Windows it performs a real
//! Restart Manager query (one session per file); on every other host it
//! returns one [`Holders`] entry per input path with `error = Some(...)`
//! explaining that RM is Windows-only, so callers can render a uniform
//! diagnostic without their own `cfg` gates.
//!
//! All Restart Manager calls are best-effort: any failure (RM not
//! available, access denied, file no longer exists) is reported per-file
//! in [`Holders::error`] rather than propagated, so a partial answer is
//! always returned.
//!
//! # Why a submodule rather than a crate?
//!
//! Today this is consumed by exactly one caller
//! ([`crate::busy_files`]). The shape, naming, and dependency footprint
//! are deliberately chosen so the directory can be lifted out as a
//! standalone crate (e.g. `who-holds-it`) later with no API redesign:
//!
//! - Types and entry point are defined here, not re-exported through
//!   the consumer.
//! - All Win32 FFI goes through `windows-sys`, not hand-rolled
//!   `extern "system"` blocks.
//! - There are no `crate::` references back into the rest of
//!   `cargo-mcp`.
//!
//! # Safety strategy
//!
//! Each `unsafe` block in this module:
//!
//! 1. Wraps a single Win32 call documented at
//!    <https://learn.microsoft.com/windows/win32/api/restartmanager/>
//!    (or the corresponding `kernel32` page for `FormatMessageW` /
//!    `LocalFree`).
//! 2. Has a `// SAFETY:` comment naming the precondition that makes the
//!    call sound (typically: pointers come from owned `Vec`s or stack
//!    arrays whose lifetime outlives the call, and the OS is responsible
//!    for any out-pointer it writes).
//! 3. Is paired with an RAII guard ([`SessionGuard`]) for any handle the
//!    OS hands back, so leaks and double-closes are not possible from
//!    safe code.

use std::path::{Path, PathBuf};

// ── public types ─────────────────────────────────────────────────────────────

/// Coarse classification of a process holding a busy file, mirroring the
/// `RM_APP_TYPE` enum from the Windows Restart Manager API.
///
/// Kept as a stable Rust enum (rather than a raw integer) so non-Windows
/// callers and tests can match on it without `cfg` gates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
// Most variants are only constructed by the Windows-only impl below;
// on non-Windows builds the dead-code lint would otherwise fire even
// though the variants are part of the cross-platform public API by
// design.
#[cfg_attr(not(windows), allow(dead_code))]
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
    /// Full path to the holder's executable image, when it could be
    /// resolved via `OpenProcess` + `QueryFullProcessImageNameW`. `None`
    /// when the process exited between the RM snapshot and the lookup,
    /// when our token lacks `PROCESS_QUERY_LIMITED_INFORMATION` rights
    /// for it (e.g. protected/system processes), or on non-Windows hosts.
    pub app_path: Option<PathBuf>,
    /// Coarse application kind (used to tailor the suggested remediation —
    /// e.g. "stop the debugger" vs "stop the service").
    pub app_kind: AppKind,
}

/// Restart Manager result for a single queried file path.
#[derive(Debug, Clone)]
pub struct Holders {
    /// The file path that was queried.
    pub path: PathBuf,
    /// Processes holding open handles on `path` at query time. May be empty
    /// even when [`Holders::error`] is `None` (e.g. the offending process
    /// released the handle between the cargo error and our query).
    pub holders: Vec<ProcessHolder>,
    /// Set when Restart Manager returned an error or could not be invoked
    /// (e.g. on non-Windows hosts, or when the path no longer exists).
    /// Diagnostic-only — `holders` is still authoritative.
    pub error: Option<String>,
}

// ── public entry point ───────────────────────────────────────────────────────

/// Ask the OS which process is holding each of `paths` open.
///
/// Returns one [`Holders`] entry per input path, in the same order. On
/// non-Windows hosts every entry has an `error` of "Restart Manager
/// diagnostics are Windows-only" and an empty `holders` list.
pub fn who_holds(paths: &[&Path]) -> Vec<Holders> {
    #[cfg(windows)]
    {
        windows_impl::query(paths)
    }
    #[cfg(not(windows))]
    {
        paths
            .iter()
            .map(|p| Holders {
                path: p.to_path_buf(),
                holders: Vec::new(),
                error: Some("Restart Manager diagnostics are Windows-only".into()),
            })
            .collect()
    }
}

// ── Windows implementation ───────────────────────────────────────────────────

#[cfg(windows)]
mod windows_impl {
    //! Restart Manager FFI, via the `windows-sys` typed bindings.
    //!
    //! API reference:
    //! <https://learn.microsoft.com/windows/win32/api/restartmanager/>

    use super::{AppKind, Holders, ProcessHolder};
    use std::os::windows::ffi::OsStrExt;
    use std::path::{Path, PathBuf};
    use std::ptr;

    use windows_sys::Win32::Foundation::{
        CloseHandle, ERROR_MORE_DATA, ERROR_SUCCESS, HLOCAL, LocalFree,
    };
    use windows_sys::Win32::System::Diagnostics::Debug::{
        FORMAT_MESSAGE_ALLOCATE_BUFFER, FORMAT_MESSAGE_FROM_SYSTEM, FORMAT_MESSAGE_IGNORE_INSERTS,
        FormatMessageW,
    };
    use windows_sys::Win32::System::RestartManager::{
        CCH_RM_SESSION_KEY, RM_APP_TYPE, RM_PROCESS_INFO, RmConsole, RmCritical, RmEndSession,
        RmExplorer, RmGetList, RmMainWindow, RmOtherWindow, RmRegisterResources, RmService,
        RmStartSession,
    };
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
    };

    fn classify(app_type: RM_APP_TYPE) -> AppKind {
        // RM_APP_TYPE is an i32 newtype in windows-sys; compare via the
        // typed constants rather than raw numeric literals.
        if app_type == RmMainWindow {
            AppKind::MainWindow
        } else if app_type == RmOtherWindow {
            AppKind::OtherWindow
        } else if app_type == RmService {
            AppKind::Service
        } else if app_type == RmExplorer {
            AppKind::Explorer
        } else if app_type == RmConsole {
            AppKind::Console
        } else if app_type == RmCritical {
            AppKind::Critical
        } else {
            // Covers RmUnknownApp (0) and any future RM_APP_TYPE values
            // Microsoft might add. Treat as `Unknown` rather than failing.
            AppKind::Unknown
        }
    }

    // ── error formatting ─────────────────────────────────────────────────

    // MAKELANGID(LANG_ENGLISH=0x09, SUBLANG_ENGLISH_US=0x01) -> 0x0409.
    // We try en-US first because the rest of cargo-mcp parses
    // English error text elsewhere; if en-US isn't installed on this
    // host `format_win32_error` falls back to the system default
    // language (`LANG_NEUTRAL_SUBLANG_DEFAULT`, 0).
    const LANG_ENGLISH_US: u32 = 0x0409;
    const LANG_SYSTEM_DEFAULT: u32 = 0;

    /// Translate a Win32 error code (as returned by Restart Manager) into
    /// a localized message via `FormatMessageW`. Returns `None` if
    /// `FormatMessageW` itself fails (e.g. unknown code).
    ///
    /// Tries U.S. English first so the text is predictable across hosts
    /// and matches the English error patterns that the rest of cargo-mcp
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
        // SAFETY: Documented `FormatMessageW` contract. With
        // FORMAT_MESSAGE_ALLOCATE_BUFFER set, the API treats `lpBuffer`
        // as `LPWSTR*` rather than `LPWSTR` and writes the allocated
        // address back through it; our `&mut buf_ptr` cast matches that
        // usage exactly. Source pointer is null because we are not using
        // FORMAT_MESSAGE_FROM_HMODULE / FROM_STRING. No insert arguments
        // because IGNORE_INSERTS is set.
        let n = unsafe {
            FormatMessageW(
                FORMAT_MESSAGE_ALLOCATE_BUFFER
                    | FORMAT_MESSAGE_FROM_SYSTEM
                    | FORMAT_MESSAGE_IGNORE_INSERTS,
                ptr::null(),
                code,
                lang_id,
                (&mut buf_ptr as *mut *mut u16) as *mut u16,
                0,
                ptr::null(),
            )
        };
        if n == 0 || buf_ptr.is_null() {
            return None;
        }
        // SAFETY: FormatMessageW returned `n > 0` and a non-null buffer,
        // and per its contract that buffer points to `n` UTF-16 code
        // units of message text owned by the OS until we LocalFree it.
        let slice = unsafe { std::slice::from_raw_parts(buf_ptr, n as usize) };
        let mut s = String::from_utf16_lossy(slice);
        // FormatMessage typically appends "\r\n" — trim trailing
        // whitespace so it doesn't break our single-line log output.
        let trimmed_len = s.trim_end().len();
        s.truncate(trimmed_len);
        // SAFETY: `buf_ptr` was produced by FormatMessageW with
        // FORMAT_MESSAGE_ALLOCATE_BUFFER, which the docs explicitly say
        // must be released with LocalFree.
        unsafe { LocalFree(buf_ptr as HLOCAL) };
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

    // Test-only re-exports so the outer `tests` module can exercise
    // helpers without making them part of the module's normal API.
    #[cfg(test)]
    pub(super) fn rm_err_for_tests(api: &str, code: u32) -> String {
        rm_err(api, code)
    }

    #[cfg(test)]
    pub(super) fn strip_unc_prefix_for_tests(p: PathBuf) -> PathBuf {
        strip_unc_prefix(p)
    }

    // ── entry point ──────────────────────────────────────────────────────

    /// One Restart Manager session per file so each holder list maps back
    /// to a specific path. RM's per-session output is a *union* across all
    /// registered resources, so multiplexing files through one session
    /// would lose the path → holders mapping that the diagnostic depends
    /// on.
    pub(super) fn query(paths: &[&Path]) -> Vec<Holders> {
        paths.iter().map(|p| query_one(p)).collect()
    }

    fn query_one(path: &Path) -> Holders {
        match query_one_inner(path) {
            Ok(holders) => Holders {
                path: path.to_path_buf(),
                holders,
                error: None,
            },
            Err(msg) => Holders {
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
        let mut session_key = [0u16; CCH_RM_SESSION_KEY as usize + 1];
        // SAFETY: `handle` is a stack `u32` we own; `session_key` is a
        // stack array sized per CCH_RM_SESSION_KEY + 1 as the API
        // requires; flags=0 is the documented default.
        let rc = unsafe { RmStartSession(&mut handle, 0, session_key.as_mut_ptr()) };
        if rc != ERROR_SUCCESS {
            return Err(rm_err("RmStartSession", rc));
        }

        // RAII: ensure RmEndSession runs on every exit path below.
        struct SessionGuard(u32);
        impl Drop for SessionGuard {
            fn drop(&mut self) {
                // SAFETY: `self.0` was returned successfully by
                // RmStartSession above and has not been ended yet.
                let _ = unsafe { RmEndSession(self.0) };
            }
        }
        let _guard = SessionGuard(handle);

        // Register the file as a resource.
        let file_ptrs: [*const u16; 1] = [wide.as_ptr()];
        // SAFETY: `handle` is a live RM session; `file_ptrs` borrows from
        // `wide`, which outlives this call; the application/service
        // arrays are empty (count 0, null pointer is permitted).
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
        // SAFETY: All out pointers reference live stack locals; passing a
        // null `rg_affected_apps` together with `count = 0` is the
        // documented probe-mode invocation.
        let rc = unsafe {
            RmGetList(
                handle,
                &mut needed,
                &mut count,
                ptr::null_mut(),
                &mut reasons,
            )
        };
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
            let mut buf: Vec<RM_PROCESS_INFO> = Vec::with_capacity(needed as usize);
            count = needed;
            // SAFETY: `buf` has capacity for `count` `RM_PROCESS_INFO`
            // entries; on success RM populates exactly `count` of them,
            // which we record below via `set_len`. All other out
            // pointers reference live stack locals.
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
        // SAFETY: RM populated `final_count <= buf.capacity()` valid
        // RM_PROCESS_INFO entries in `buf` (per the ERROR_SUCCESS branch
        // above); set_len makes them visible as initialized.
        unsafe { buf.set_len(final_count as usize) };

        Ok(buf
            .into_iter()
            .map(|info| {
                let pid = info.Process.dwProcessId;
                ProcessHolder {
                    pid,
                    app_name: read_wide_string(&info.strAppName),
                    app_path: process_image_path(pid),
                    app_kind: classify(info.ApplicationType),
                }
            })
            .collect())
    }

    /// Resolve the full image path of `pid` via `OpenProcess` +
    /// `QueryFullProcessImageNameW`. Returns `None` on any failure (the
    /// process exited, our token lacks rights, etc.) so the caller can
    /// degrade gracefully to displaying just the RM-reported name.
    fn process_image_path(pid: u32) -> Option<PathBuf> {
        // SAFETY: `pid` is a plain integer; `OpenProcess` returns a null
        // handle on failure and a real `HANDLE` we own on success.
        let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        if handle.is_null() {
            return None;
        }
        // Wide-char buffer sized for the documented Windows long-path
        // ceiling (32_768 UTF-16 code units, including the trailing NUL).
        let mut buf = vec![0u16; 32_768];
        let mut size: u32 = buf.len() as u32;
        // SAFETY: `handle` is a live process handle we just obtained;
        // `buf` has `size` UTF-16 slots that outlive the call; `size` is
        // an in/out parameter the API rewrites with the count of code
        // units actually written (excluding the trailing NUL).
        let ok = unsafe { QueryFullProcessImageNameW(handle, 0, buf.as_mut_ptr(), &mut size) };
        // SAFETY: `handle` was returned by `OpenProcess` above and has
        // not been closed yet; close exactly once.
        unsafe { CloseHandle(handle) };
        if ok == 0 || size == 0 || (size as usize) > buf.len() {
            return None;
        }
        let s = String::from_utf16_lossy(&buf[..size as usize]);
        if s.is_empty() {
            return None;
        }
        Some(strip_unc_prefix(PathBuf::from(s)))
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
    /// Operates on path [`std::path::Component`]s rather than a lossy
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
    use std::path::PathBuf;

    #[cfg(not(windows))]
    #[test]
    fn who_holds_on_non_windows_returns_error_per_path() {
        let p = PathBuf::from("/tmp/does-not-matter");
        let result = who_holds(&[p.as_path()]);
        assert_eq!(result.len(), 1);
        assert!(result[0].error.is_some());
        assert!(result[0].holders.is_empty());
    }

    #[cfg(windows)]
    #[test]
    fn who_holds_on_windows_handles_empty_input() {
        let result = who_holds(&[]);
        assert!(result.is_empty());
    }

    #[cfg(windows)]
    #[test]
    fn who_holds_on_windows_handles_unheld_file() {
        // A freshly-created temp file with no other openers must come
        // back with zero holders and no error. This exercises the full
        // RM round trip without depending on any specific running
        // process.
        let tmp =
            std::env::temp_dir().join(format!("cargo-mcp-rm-test-{}.tmp", std::process::id()));
        std::fs::write(&tmp, b"x").unwrap();
        let result = who_holds(&[tmp.as_path()]);
        let _ = std::fs::remove_file(&tmp);
        assert_eq!(result.len(), 1);
        // Either: no error and no holders, OR an RM-side error
        // (acceptable on locked-down CI agents). The contract is "best
        // effort".
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
        // localized FormatMessage text on every Windows install. We
        // only assert structural properties (the API name, the numeric
        // code, and that *some* descriptive text was found) so the test
        // is not sensitive to the host's display language.
        let s = super::windows_impl::rm_err_for_tests("RmTest", 2);
        assert!(s.starts_with("RmTest failed:"), "unexpected prefix: {s:?}");
        // Two valid shapes:
        //   success:  "RmTest failed: <localized message> (code 2)"
        //   fallback: "RmTest failed: code 2 (no system message)"
        let success = s
            .strip_prefix("RmTest failed: ")
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
