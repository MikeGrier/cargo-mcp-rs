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
//! 2. **Holder query** ([`query_holders`]) — thin re-export of
//!    [`crate::rm::who_holds`], which on Windows calls the Restart
//!    Manager APIs (`rstrtmgr.dll`) to report every process holding a
//!    handle on each given file (PID, executable name, application
//!    kind), and on non-Windows hosts returns one [`FileHolders`] entry
//!    per input path with `error = Some(...)` explaining that RM is
//!    Windows-only — so callers can render a uniform diagnostic without
//!    their own `cfg` gates.
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
//
// Re-exported from the `rm` submodule so the historical
// `busy_files::FileHolders` path keeps working while the FFI itself
// lives in one isolated place. Other types (`AppKind`, `ProcessHolder`)
// are accessed directly via `crate::rm::` by callers that need them.

pub use crate::rm::Holders as FileHolders;

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
    // Cargo's `--message-format=json` wraps each diagnostic in an NDJSON
    // object whose backslashes (and embedded backticks) are JSON-escaped,
    // so the raw stdout bytes contain `\\` between path segments and our
    // backtick scanner would harvest a malformed path that Restart Manager
    // can't open. Pre-expand JSON diagnostic lines into their rendered
    // text; non-JSON lines pass through unchanged. See
    // `normalize_json_lines` for the exact contract on JSON lines that
    // carry no diagnostic text (they are dropped, not passed through).
    let normalized = normalize_json_lines(stderr_or_stdout);
    let lines: Vec<&str> = normalized.lines().collect();

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
            harvest_at_path_paths(line, &mut out);
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
        if !inner.is_empty() && (inner.contains('/') || inner.contains('\\') || inner.contains('.'))
        {
            out.push(PathBuf::from(inner));
        }
        i = j + 1;
    }
}

/// Push every path that appears after the literal `at path "..."` (a form
/// rustc uses for the *underlying* file when reporting a wrapping error,
/// e.g. `failed to remove temporary directory: ... at path
/// "...\\.tmpD5sCLz.temp-archive"`). Cargo doesn't put this path in
/// backticks, so [`harvest_backtick_paths`] would miss it and the user
/// would get a Restart Manager report against the *outer* artefact (which
/// rustc never created) instead of the actual locked file.
///
/// The captured substring is `Debug`-formatted by rustc (`{:?}`), so on
/// Windows every backslash arrives doubled (`\\`) even after the JSON
/// layer has already been un-escaped by [`normalize_json_lines`]. We
/// collapse those doubled backslashes back to single ones before
/// returning the path; otherwise `std::fs::canonicalize` would have to
/// guess at the redundant separators and Restart Manager would receive
/// a syntactically odd path. Forward slashes are not affected.
fn harvest_at_path_paths(line: &str, out: &mut Vec<PathBuf>) {
    const NEEDLE: &str = "at path \"";
    let mut rest = line;
    while let Some(pos) = rest.find(NEEDLE) {
        let after = &rest[pos + NEEDLE.len()..];
        if let Some(end) = after.find('"') {
            let inner = &after[..end];
            if !inner.is_empty()
                && (inner.contains('/') || inner.contains('\\') || inner.contains('.'))
            {
                // Un-escape Debug-style doubled backslashes. Safe for
                // UNC paths because rustc Debug-prints `\\\\server\\share`,
                // which collapses to the correct `\\server\\share`.
                let unescaped = inner.replace(r"\\", r"\");
                out.push(PathBuf::from(unescaped));
            }
            rest = &after[end + 1..];
        } else {
            break;
        }
    }
}

/// Replace every NDJSON line emitted by `cargo --message-format=json` with
/// its rendered diagnostic text, so the rest of the extractor can run
/// against unescaped (`\` not `\\`) paths and human-readable phrases.
///
/// Non-JSON lines pass through unchanged. Lines that are JSON but lack a
/// `message.rendered` / `rendered` / `message` field are dropped because
/// they carry no diagnostic text relevant here (e.g. `compiler-artifact`,
/// `build-finished`).
fn normalize_json_lines(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for line in s.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with('{')
            && let Ok(v) = serde_json::from_str::<serde_json::Value>(trimmed)
        {
            let rendered = v
                .pointer("/message/rendered")
                .and_then(|x| x.as_str())
                .or_else(|| v.pointer("/rendered").and_then(|x| x.as_str()))
                .or_else(|| v.pointer("/message/message").and_then(|x| x.as_str()))
                .or_else(|| v.pointer("/message").and_then(|x| x.as_str()));
            if let Some(text) = rendered {
                out.push_str(text);
                if !text.ends_with('\n') {
                    out.push('\n');
                }
            }
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

// ── Restart Manager query ────────────────────────────────────────────────────

/// Ask the OS which processes hold open handles on each path in `paths`.
///
/// On Windows this calls into the Restart Manager (`rstrtmgr.dll`). On
/// every other platform this returns a vector of [`FileHolders`] entries
/// each carrying a non-empty `error` string explaining that Restart
/// Manager diagnostics are Windows-only, so callers can render a uniform
/// diagnostic instead of branching. The exact wording is owned by
/// [`crate::rm::who_holds`] — do not match on it.
///
/// Best-effort: a missing or inaccessible `rstrtmgr.dll`, an access-denied
/// session start, or a path that has already been deleted is reported per
/// entry rather than propagated. The function never panics.
pub fn query_holders(paths: &[&Path]) -> Vec<FileHolders> {
    crate::rm::who_holds(paths)
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
///     PID 12345 - foo.exe (C:\src\foo\target\debug\foo.exe) [console]
///     PID  6789 - rust-analyzer-proc-macro-srv.exe [console]
///   target\debug\bar.dll
///     (no current holders - likely a transient AV / indexer scan)
/// ```
///
/// The full image path is shown in parentheses when Restart Manager could
/// resolve it; the application kind is shown in `[brackets]` to keep it
/// visually distinct from the path.
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
            s.push_str("    (no current holders - likely a transient AV / indexer scan)\n");
            continue;
        }
        for h in &entry.holders {
            let path_part = match &h.app_path {
                Some(p) => format!(" ({})", p.display()),
                None => String::new(),
            };
            s.push_str(&format!(
                "    PID {pid} - {name}{path_part} [{kind}]\n",
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
            let path_part = match &h.app_path {
                Some(p) => format!(" ({})", p.display()),
                None => String::new(),
            };
            parts.push(format!(
                "{name}{path_part} (PID {pid})",
                name = h.app_name,
                pid = h.pid
            ));
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

// ── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rm::{AppKind, ProcessHolder};

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

    /// Verbatim shape captured from a real `cargo clean` failure where
    /// a directory in `target/debug/deps` had an open handle. Includes
    /// the `Caused by:` indentation and trailing blank line that cargo
    /// actually emits, so a future change to the extractor doesn't
    /// silently regress this case.
    #[test]
    fn extracts_directory_path_from_real_cargo_clean_error_block() {
        let stderr = "\
error: failed to remove directory `C:\\Users\\Me\\AppData\\Local\\Temp\\v\\target\\debug\\deps`

Caused by:
    The process cannot access the file because it is being used by another process. (os error 32)
";
        let paths = extract_busy_paths(stderr);
        assert_eq!(
            paths,
            vec![PathBuf::from(
                "C:\\Users\\Me\\AppData\\Local\\Temp\\v\\target\\debug\\deps"
            )]
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
    fn extracts_paths_from_cargo_json_message_format() {
        // Real-world line from `cargo build --message-format=json` when
        // rustc fails to clean up its temp archive on Windows. In the
        // NDJSON source:
        //   * the backtick-quoted artefact uses `\\` (one JSON-escaped
        //     backslash) because cargo writes the path verbatim into
        //     `rendered`, and
        //   * the locked file is reported as `at path \"...\"` with `\\\\`
        //     because rustc Debug-formats it (`{:?}`) before cargo wraps
        //     the whole thing in JSON, so each backslash is escaped twice.
        // After JSON decoding the two paths therefore look different
        // (single vs. double backslashes); both must end up extracted as
        // the same canonical Windows path.
        let stdout = r#"{"reason":"compiler-message","package_id":"path+file:///x#0.1.0","message":{"rendered":"error: failed to build archive at `Z:\\deps\\libfoo.rlib`: failed to remove temporary directory: The process cannot access the file because it is being used by another process. (os error 32) at path \"Z:\\\\deps\\\\.tmpD5sCLz.temp-archive\"\n\n","$message_type":"diagnostic","children":[],"level":"error","message":"failed","spans":[],"code":null}}
{"reason":"build-finished","success":false}"#;
        let paths = extract_busy_paths(stdout);
        assert_eq!(
            paths,
            vec![
                PathBuf::from(r"Z:\deps\libfoo.rlib"),
                PathBuf::from(r"Z:\deps\.tmpD5sCLz.temp-archive"),
            ],
            "expected both the rlib (backtick) and the temp-archive \
             (at path \"...\") form to be extracted with single \
             backslashes, got {paths:?}"
        );
    }

    #[test]
    fn extracts_at_path_quoted_paths_from_plain_text() {
        let stderr = "\
error: failed to remove temporary directory: The process cannot access the \
file because it is being used by another process. (os error 32) at path \
\"C:\\\\work\\\\.tmpAbc.temp-archive\"
";
        // The four-backslash sequences in the Rust source render as two
        // literal backslashes in the test input, matching how rustc
        // Debug-formats Windows paths (`{:?}`) inside its error chain.
        // `harvest_at_path_paths` is expected to un-escape those back to
        // single separators.
        let paths = extract_busy_paths(stderr);
        assert_eq!(paths, vec![PathBuf::from(r"C:\work\.tmpAbc.temp-archive")]);
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
                    app_path: None,
                    app_kind: AppKind::Console,
                },
                ProcessHolder {
                    pid: 2,
                    app_name: "bar.exe".into(),
                    app_path: Some(PathBuf::from(r"C:\bin\bar.exe")),
                    app_kind: AppKind::MainWindow,
                },
            ],
            error: None,
        }];
        let s = format_short_summary(&report).unwrap();
        assert!(s.contains("foo.exe (PID 1)"));
        assert!(
            s.contains(r"bar.exe (C:\bin\bar.exe) (PID 2)"),
            "missing path-decorated entry in {s:?}"
        );
    }

    #[test]
    fn format_short_summary_caps_at_four_with_overflow_note() {
        let mut holders = Vec::new();
        for i in 1..=7u32 {
            holders.push(ProcessHolder {
                pid: i,
                app_name: format!("app{i}.exe"),
                app_path: None,
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
                app_path: Some(PathBuf::from(r"C:\src\foo\target\debug\foo.exe")),
                app_kind: AppKind::Console,
            }],
            error: None,
        }];
        let s = format_full_report(&report);
        assert!(s.contains("1 file(s) reported in use"));
        assert!(s.contains("target/debug/foo.exe"));
        assert!(s.contains("PID 12345"));
        assert!(s.contains(r"foo.exe (C:\src\foo\target\debug\foo.exe)"));
        assert!(s.contains("[console]"));
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
}
