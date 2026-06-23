// Copyright (c) Michael Grier. All rights reserved.

//! Support for [`cargo-nextest`](https://nexte.st/), exposed via the
//! `cargo_nextest_run` and `cargo_nextest_list` MCP tools.
//!
//! Nextest is a third-party `cargo nextest` plugin (not bundled with cargo
//! or rustup). When the plugin binary is not on `PATH`, both tools return
//! an error result whose body is markdown containing the install commands
//! inside fenced shell code blocks — VS Code Copilot Chat renders those
//! with **Copy** and **Run in Terminal** affordances automatically.
//!
//! See `DESIGN-NOTES.md` ("cargo-nextest support") for the rationale
//! behind the output-wrapping choice, the timeout model, and the flag
//! remapping (`cargo_profile` vs `nextest_profile`, `build_jobs` vs
//! `test_threads`, etc.).

use std::process::{Command, Stdio};

use serde_json::Value;

use crate::invoke::{self, CargoOutput};
use crate::tools::{
    self, CommonOpts, STDERR_REASON, SummaryKind, ToolResult, invocation_header,
    is_build_finished_line, opt_bool, opt_int_str, opt_str, opt_timeout_explicit,
    push_feature_flags, push_manifest_options, push_package_selection, toolchain_arg,
    validate_relative_output_path, write_output_path_and_summarize,
};

/// Discriminator for the NDJSON record that wraps one line of nextest's
/// human reporter output (the test phase). Mirrors `TEST_OUTPUT_REASON` for
/// `cargo_test` — each non-JSON stdout line from nextest is wrapped
/// individually so the response stays a strict NDJSON stream parseable
/// line-by-line.
pub(crate) const NEXTEST_OUTPUT_REASON: &str = "x-cargo-mcp-nextest-output";

// ── installation detection ──────────────────────────────────────────────────

/// Outcome of probing whether `cargo nextest` is installed on this machine.
pub(crate) enum NextestProbe {
    /// `cargo nextest --version` succeeded; nextest is installed.
    Installed,
    /// Probe failed or returned non-zero (plugin not on PATH, or cargo itself
    /// could not be located).
    Missing,
}

/// Probe whether `cargo nextest` is available by running
/// `cargo nextest --version` with stdout/stderr suppressed.
///
/// Uses the same cargo binary cargo-mcp would invoke for any other tool
/// (via [`invoke::resolve_cargo_binary`]) and the same explicit
/// environment block (built-in defaults + RUSTC pin + the caller's
/// per-call `env` overrides installed by the dispatcher via
/// [`invoke::set_extra_env`], applied via
/// [`invoke::apply_subprocess_env`]). Without that env layering a
/// caller who passes `env.PATH` / `env.CARGO_HOME` to make the plugin
/// discoverable for the real run/list would still see the probe report
/// it as missing.
///
/// **Workspace-independent.** Plugin detection is PATH-based, so we do
/// NOT inherit the caller's `working_dir`. Spawning in an invalid path
/// would fail at the OS layer (treated as `Missing`) and we'd return
/// install instructions for what is actually a bad-path problem.
///
/// **Not cached.** A user who installs nextest mid-session should be able
/// to retry immediately without restarting the MCP server.
pub(crate) fn probe() -> NextestProbe {
    let (cargo_path, _src) = invoke::resolve_cargo_binary();
    let mut cmd = Command::new(&cargo_path);
    invoke::apply_subprocess_env(&mut cmd);
    cmd.args(["nextest", "--version"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    match cmd.status() {
        Ok(s) if s.success() => NextestProbe::Installed,
        _ => NextestProbe::Missing,
    }
}

/// Build the markdown body returned when nextest is missing.
///
/// The fenced shell blocks render in VS Code Copilot Chat with **Copy**
/// and **Run in Terminal** affordances, so the user can act on the
/// suggestion without re-typing anything.
pub(crate) fn missing_install_message() -> String {
    let mut s = String::new();
    s.push_str(
        "cargo-nextest is not installed (looked for the `cargo-nextest` plugin via \
         `cargo nextest --version`).\n\n",
    );
    s.push_str("Install with one of:\n\n");
    s.push_str("```pwsh\n");
    s.push_str("cargo install cargo-nextest --locked\n");
    s.push_str("```\n\n");
    s.push_str("Or, for a much faster install of a pre-built binary:\n\n");
    s.push_str("```pwsh\n");
    s.push_str("cargo binstall cargo-nextest\n");
    s.push_str("```\n\n");
    s.push_str(
        "See <https://nexte.st/docs/installation/> for platform-specific \
         pre-built binaries. Re-run this tool after installation.\n",
    );
    s
}

/// Build an `is_error: true` [`ToolResult`] carrying the install instructions.
pub(crate) fn missing_install_result() -> ToolResult {
    ToolResult::Text {
        text: missing_install_message(),
        is_error: true,
    }
}

/// True when the workspace at `working_dir` (or the cargo-mcp CWD when
/// `None`) contains a nextest config file at `.config/nextest.toml`.
///
/// Used by `cargo_setup` to escalate the "optional: cargo-nextest" hint
/// from optional to recommended.
pub(crate) fn workspace_has_nextest_config(working_dir: Option<&str>) -> bool {
    let base: std::path::PathBuf = match working_dir {
        Some(wd) => std::path::PathBuf::from(wd),
        None => std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from(".")),
    };
    base.join(".config").join("nextest.toml").is_file()
}

// ── output filtering / formatting ───────────────────────────────────────────

/// Filter a `cargo nextest run` stdout NDJSON stream:
/// - Keep `compiler-message` and `build-finished` records (forwarded by
///   nextest from cargo via `--cargo-message-format=json`).
/// - Drop blank lines and the known-noise cargo records
///   `compiler-artifact` / `build-script-executed` (already delivered
///   via streaming progress).
/// - Wrap every other line — non-JSON (nextest's human reporter output,
///   captured test stdout) **and** any JSON we don't explicitly
///   recognise (e.g. structured logs a test prints, or future
///   nextest/cargo record types) — in an [`NEXTEST_OUTPUT_REASON`]
///   NDJSON record so it is preserved rather than silently dropped.
fn filter_nextest_run_ndjson(stdout: &str) -> String {
    stdout
        .lines()
        .filter_map(|line| {
            if line.trim().is_empty() {
                return None;
            }
            if let Ok(v) = serde_json::from_str::<Value>(line) {
                match v.get("reason").and_then(|r| r.as_str()) {
                    Some("compiler-message") | Some("build-finished") => {
                        return Some(line.to_owned());
                    }
                    Some("compiler-artifact") | Some("build-script-executed") => {
                        return None;
                    }
                    _ => {}
                }
            }
            Some(
                serde_json::to_string(&serde_json::json!({
                    "reason": NEXTEST_OUTPUT_REASON,
                    "text": line,
                }))
                .unwrap_or_else(|_| "{}".into()),
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Format the output of `cargo nextest run`.
///
/// Mirrors [`tools::format_test_output`]: invocation header → filtered
/// records → status trailer → optional stderr record. Output is a strict
/// NDJSON stream.
fn format_nextest_run_output(out: &CargoOutput, argv: &[&str], wd: Option<&str>) -> String {
    let header = invocation_header(argv, wd);
    let filtered = filter_nextest_run_ndjson(&out.stdout);
    let filtered = filtered.trim_end();
    let trailer = if out.exit_code == 0 {
        r#"{"status":"success"}"#.to_owned()
    } else {
        format!(r#"{{"status":"error","exit_code":{}}}"#, out.exit_code)
    };
    let stderr_trimmed = out.stderr.trim();
    let mut parts: Vec<String> = Vec::with_capacity(3);
    if !filtered.is_empty() {
        parts.push(filtered.to_owned());
    }
    parts.push(trailer);
    if !stderr_trimmed.is_empty() {
        let stderr_record = serde_json::to_string(&serde_json::json!({
            "reason": STDERR_REASON,
            "text": stderr_trimmed,
        }))
        .unwrap_or_else(|_| "{}".into());
        parts.push(stderr_record);
    }
    let body = parts.join("\n");
    format!("{header}{body}")
}

/// Format the output of `cargo nextest list --message-format json`.
///
/// nextest's `list` emits JSON on stdout (plus, when
/// `--cargo-message-format=json` is in effect, cargo's build-phase
/// `compiler-message` / `build-finished` records ahead of it). We wrap
/// the whole stream in a one-line `x-cargo-mcp-invocation` header and a
/// JSON status trailer so the response is framed identically to every
/// other cargo-mcp tool.
///
/// To keep the response a strict one-JSON-object-per-line stream even if
/// upstream ever switches to pretty-printed output, every non-empty
/// stdout line that parses as JSON is re-serialised in compact form;
/// non-JSON lines pass through verbatim (we'd rather forward something
/// unrecognised than drop it on the floor).
fn format_nextest_list_output(out: &CargoOutput, argv: &[&str], wd: Option<&str>) -> String {
    let header = invocation_header(argv, wd);
    let stdout = out.stdout.trim_end_matches('\n');
    let trailer = if out.exit_code == 0 {
        r#"{"status":"success"}"#.to_owned()
    } else {
        format!(r#"{{"status":"error","exit_code":{}}}"#, out.exit_code)
    };
    let stderr_trimmed = out.stderr.trim();
    let mut parts: Vec<String> = Vec::with_capacity(3);
    if !stdout.is_empty() {
        // First try the whole stdout as a single JSON document — that
        // catches a future nextest that switches to pretty-printed
        // output (multi-line `{ ... }`), where per-line parsing would
        // fail on every brace line and we'd forward the pretty-print
        // verbatim. If the whole-blob parse fails, fall back to
        // line-by-line compaction so an NDJSON-style stream
        // (cargo build records ahead of the list payload) still works.
        let compacted_blob = serde_json::from_str::<Value>(stdout)
            .ok()
            .and_then(|v| serde_json::to_string(&v).ok());
        if let Some(line) = compacted_blob {
            parts.push(line);
        } else {
            let per_line: Vec<String> = stdout
                .lines()
                .filter(|l| !l.trim().is_empty())
                .map(|l| match serde_json::from_str::<Value>(l) {
                    Ok(v) => serde_json::to_string(&v).unwrap_or_else(|_| l.to_owned()),
                    Err(_) => l.to_owned(),
                })
                .collect();
            if !per_line.is_empty() {
                parts.push(per_line.join("\n"));
            }
        }
    }
    parts.push(trailer);
    if !stderr_trimmed.is_empty() {
        let stderr_record = serde_json::to_string(&serde_json::json!({
            "reason": STDERR_REASON,
            "text": stderr_trimmed,
        }))
        .unwrap_or_else(|_| "{}".into());
        parts.push(stderr_record);
    }
    let body = parts.join("\n");
    format!("{header}{body}")
}

// ── argv builders ───────────────────────────────────────────────────────────

/// Append nextest's target-selection flags. Same flags as cargo test:
/// `--lib`, `--bins`, `--bin`, `--examples`, `--example`, `--tests`,
/// `--test`, `--benches`, `--bench`, `--all-targets`.
fn push_nextest_target_selection<'a>(argv: &mut Vec<&'a str>, args: &Value, o: &'a CommonOpts) {
    if opt_bool(args, "lib") {
        argv.push("--lib");
    }
    if opt_bool(args, "bins") {
        argv.push("--bins");
    }
    if let Some(b) = &o.bin {
        argv.push("--bin");
        argv.push(b);
    }
    if opt_bool(args, "examples") {
        argv.push("--examples");
    }
    if let Some(e) = &o.example {
        argv.push("--example");
        argv.push(e);
    }
    if opt_bool(args, "tests") {
        argv.push("--tests");
    }
    if let Some(t) = &o.test {
        argv.push("--test");
        argv.push(t);
    }
    if opt_bool(args, "benches") {
        argv.push("--benches");
    }
    if let Some(b) = &o.bench {
        argv.push("--bench");
        argv.push(b);
    }
    if opt_bool(args, "all_targets") {
        argv.push("--all-targets");
    }
}

/// Append the nextest-specific compilation flags. Unlike cargo test:
/// - the build profile flag is `--cargo-profile` (not `--profile`, which
///   on nextest selects the *nextest* profile);
/// - `--build-jobs` is build parallelism (cargo test reuses `-j` for this
///   while we reuse the cargo test name verbatim — `build_jobs`).
fn push_nextest_compilation_options<'a>(
    argv: &mut Vec<&'a str>,
    args: &Value,
    cargo_profile: Option<&'a String>,
    build_jobs: Option<&'a String>,
    target: Option<&'a String>,
    target_dir: Option<&'a String>,
) {
    // `cargo_profile` and `release` are mutually exclusive; cargo_profile wins.
    if let Some(p) = cargo_profile {
        argv.push("--cargo-profile");
        argv.push(p);
    } else if opt_bool(args, "release") {
        argv.push("--release");
    }
    if let Some(j) = build_jobs {
        argv.push("--build-jobs");
        argv.push(j);
    }
    if let Some(t) = target {
        argv.push("--target");
        argv.push(t);
    }
    if let Some(d) = target_dir {
        argv.push("--target-dir");
        argv.push(d);
    }
}

/// Extracted from `args` up front so borrowed `&str`s in `argv` outlive it.
struct NextestOwnedOpts {
    cargo_profile: Option<String>,
    nextest_profile: Option<String>,
    build_jobs: Option<String>,
    test_threads: Option<String>,
    retries: Option<String>,
    filter_expr: Option<String>,
    filter: Option<String>,
    run_ignored: Option<String>,
    list_type: Option<String>,
}

impl NextestOwnedOpts {
    fn from_args(args: &Value) -> Self {
        Self {
            cargo_profile: opt_str(args, "cargo_profile").map(String::from),
            nextest_profile: opt_str(args, "nextest_profile").map(String::from),
            build_jobs: opt_int_str(args, "build_jobs"),
            test_threads: opt_int_str(args, "test_threads"),
            retries: opt_int_str(args, "retries"),
            filter_expr: opt_str(args, "filter_expr").map(String::from),
            filter: opt_str(args, "filter").map(String::from),
            run_ignored: opt_str(args, "run_ignored").map(String::from),
            list_type: opt_str(args, "list_type").map(String::from),
        }
    }
}

/// Validate `run_ignored` against nextest's enumerated values.
fn validate_run_ignored(v: Option<&str>) -> Result<(), Box<dyn std::error::Error>> {
    match v {
        None => Ok(()),
        Some("default") | Some("only") | Some("all") => Ok(()),
        Some(other) => Err(format!(
            "run_ignored must be one of \"default\", \"only\", or \"all\"; got {other:?}"
        )
        .into()),
    }
}

/// Validate `cargo_nextest_list`'s `list_type`.
fn validate_list_type(v: Option<&str>) -> Result<(), Box<dyn std::error::Error>> {
    match v {
        None => Ok(()),
        Some("full") | Some("binaries-only") => Ok(()),
        Some(other) => {
            Err(format!("list_type must be \"full\" or \"binaries-only\"; got {other:?}").into())
        }
    }
}

// ── tool entry points ───────────────────────────────────────────────────────

/// Implementation of the `cargo_nextest_run` tool.
pub(crate) fn call_run(
    args: &Value,
    on_progress: Option<&mut dyn FnMut(&str)>,
) -> Result<ToolResult, Box<dyn std::error::Error>> {
    let wd = opt_str(args, "working_dir");
    let output_path = opt_str(args, "output_path");
    if let Some(p) = output_path {
        validate_relative_output_path(p, wd)?;
    }
    let tc = toolchain_arg(args);
    let o = CommonOpts::from_args(args);
    let nx = NextestOwnedOpts::from_args(args);
    validate_run_ignored(nx.run_ignored.as_deref())?;

    // nextest's `run` subcommand. We always ask cargo to emit JSON build
    // messages so the existing compiler-message / build-finished pipeline
    // works unchanged for the build phase.
    let mut argv: Vec<&str> = vec!["nextest", "run", "--cargo-message-format=json"];

    // Nextest profile (selects per-test config from .config/nextest.toml).
    if let Some(p) = &nx.nextest_profile {
        argv.push("--profile");
        argv.push(p);
    }

    // Standard cargo selectors.
    push_package_selection(&mut argv, args, &o);
    push_nextest_target_selection(&mut argv, args, &o);
    push_feature_flags(&mut argv, args, &o);
    push_nextest_compilation_options(
        &mut argv,
        args,
        nx.cargo_profile.as_ref(),
        nx.build_jobs.as_ref(),
        o.target.as_ref(),
        o.target_dir.as_ref(),
    );
    // `ignore_rust_version` is supported by nextest (it forwards to cargo).
    push_manifest_options(&mut argv, args, &o, true);

    // Nextest-specific run flags.
    if opt_bool(args, "no_fail_fast") {
        argv.push("--no-fail-fast");
    }
    if opt_bool(args, "no_run") {
        argv.push("--no-run");
    }
    if opt_bool(args, "no_capture") {
        argv.push("--no-capture");
    }
    if let Some(n) = &nx.test_threads {
        argv.push("--test-threads");
        argv.push(n);
    }
    if let Some(n) = &nx.retries {
        argv.push("--retries");
        argv.push(n);
    }
    if let Some(r) = &nx.run_ignored {
        argv.push("--run-ignored");
        argv.push(r);
    }
    if let Some(e) = &nx.filter_expr {
        argv.push("-E");
        argv.push(e);
    }
    // The bare positional `filter` argument (nextest's libtest-compatible
    // substring filter). Goes last to avoid being mistaken for an option
    // value; safe alongside `-E` (both apply).
    if let Some(f) = &nx.filter {
        argv.push(f);
    }
    if let Some(ref t) = tc {
        argv.insert(0, t);
    }

    // Same three-state timeout selection as cargo_test: caller wins; missing
    // falls back to the server-wide default. Per-test enforcement is left to
    // nextest's profile (slow-timeout / terminate-after).
    let timeout = match opt_timeout_explicit(args)? {
        None => tools::default_test_timeout(),
        Some(explicit) => explicit,
    };

    let out = match on_progress {
        Some(cb) => invoke::run_cargo_streaming_with_timeout(
            &argv,
            wd,
            timeout,
            Some(&is_build_finished_line),
            cb,
        ),
        None => invoke::run_cargo_with_timeout(&argv, wd, timeout, Some(&is_build_finished_line)),
    }?;

    let is_error = out.exit_code != 0;
    let body = format_nextest_run_output(&out, &argv, wd);
    let text = write_output_path_and_summarize(body, output_path, wd, SummaryKind::Test)?;
    Ok(ToolResult::Text { text, is_error })
}

/// Implementation of the `cargo_nextest_list` tool.
pub(crate) fn call_list(args: &Value) -> Result<ToolResult, Box<dyn std::error::Error>> {
    let wd = opt_str(args, "working_dir");
    let tc = toolchain_arg(args);
    let o = CommonOpts::from_args(args);
    let nx = NextestOwnedOpts::from_args(args);
    validate_list_type(nx.list_type.as_deref())?;

    // Always emit nextest's stable JSON discovery format. The tool's
    // contract (and its NDJSON framing) depends on a single machine-
    // parseable payload line; exposing `--message-format human` or
    // `json-pretty` would break that, so we don't accept the knob at all.
    let mut argv: Vec<&str> = vec![
        "nextest",
        "list",
        "--message-format",
        "json",
        "--cargo-message-format=json",
    ];

    if let Some(p) = &nx.nextest_profile {
        argv.push("--profile");
        argv.push(p);
    }
    push_package_selection(&mut argv, args, &o);
    push_nextest_target_selection(&mut argv, args, &o);
    push_feature_flags(&mut argv, args, &o);
    push_nextest_compilation_options(
        &mut argv,
        args,
        nx.cargo_profile.as_ref(),
        nx.build_jobs.as_ref(),
        o.target.as_ref(),
        o.target_dir.as_ref(),
    );
    push_manifest_options(&mut argv, args, &o, true);

    if let Some(r) = &nx.run_ignored {
        argv.push("--run-ignored");
        argv.push(r);
    }
    if let Some(e) = &nx.filter_expr {
        argv.push("-E");
        argv.push(e);
    }
    if let Some(t) = &nx.list_type {
        argv.push("--list-type");
        argv.push(t);
    }
    if let Some(f) = &nx.filter {
        argv.push(f);
    }
    if let Some(ref t) = tc {
        argv.insert(0, t);
    }

    let out = invoke::run_cargo(&argv, wd)?;
    let is_error = out.exit_code != 0;
    let body = format_nextest_list_output(&out, &argv, wd);
    Ok(ToolResult::Text {
        text: body,
        is_error,
    })
}

// ── tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::INVOCATION_REASON;

    #[test]
    fn missing_install_message_contains_install_commands() {
        let s = missing_install_message();
        assert!(s.contains("cargo install cargo-nextest --locked"));
        assert!(s.contains("cargo binstall cargo-nextest"));
        // Fenced code blocks render as Copy / Run-in-Terminal affordances
        // in VS Code Copilot Chat; assert the fence is present so the UX
        // promise documented in DESIGN-NOTES does not silently regress.
        assert!(s.contains("```pwsh"));
    }

    #[test]
    fn filter_nextest_run_ndjson_keeps_compiler_messages_and_wraps_text() {
        let input = "\
{\"reason\":\"compiler-artifact\",\"target\":{\"name\":\"foo\"}}\n\
{\"reason\":\"compiler-message\",\"message\":{\"level\":\"warning\"}}\n\
{\"reason\":\"build-finished\",\"success\":true}\n\
\n\
    Starting 12 tests across 3 binaries\n\
        PASS [   0.001s] my-crate tests::it_works\n";
        let out = filter_nextest_run_ndjson(input);
        let lines: Vec<&str> = out.lines().collect();
        // compiler-artifact dropped; compiler-message kept; build-finished
        // kept; two non-JSON lines wrapped; blank line dropped.
        assert_eq!(lines.len(), 4);
        assert!(lines[0].contains("\"compiler-message\""));
        assert!(lines[1].contains("\"build-finished\""));
        assert!(lines[2].contains(NEXTEST_OUTPUT_REASON));
        assert!(lines[2].contains("Starting 12 tests"));
        assert!(lines[3].contains(NEXTEST_OUTPUT_REASON));
    }

    #[test]
    fn filter_nextest_run_ndjson_wraps_unrecognised_json_lines() {
        // A test printing a structured log line, or a future
        // nextest/cargo record we don't yet know about, must not be
        // silently dropped — wrap it as captured output so the caller
        // still sees it.
        let input = "\
{\"level\":\"info\",\"msg\":\"a test logged this\"}\n\
{\"reason\":\"build-script-executed\",\"package_id\":\"x\"}\n\
{\"reason\":\"some-future-record\",\"detail\":42}\n";
        let out = filter_nextest_run_ndjson(input);
        let lines: Vec<&str> = out.lines().collect();
        // structured log wrapped; build-script-executed dropped as
        // known noise; unknown reason wrapped.
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains(NEXTEST_OUTPUT_REASON));
        assert!(lines[0].contains("a test logged this"));
        assert!(lines[1].contains(NEXTEST_OUTPUT_REASON));
        assert!(lines[1].contains("some-future-record"));
        // The wrapped payload must itself be valid JSON (the original
        // line is carried verbatim inside the `text` field, escaped by
        // serde_json).
        let v: Value = serde_json::from_str(lines[0]).expect("wrapped record is JSON");
        assert_eq!(v["reason"], NEXTEST_OUTPUT_REASON);
        assert!(v["text"].as_str().unwrap().contains("a test logged this"));
    }

    #[test]
    fn format_nextest_run_output_includes_header_and_status_trailer() {
        let out = CargoOutput {
            stdout: "{\"reason\":\"build-finished\",\"success\":true}\n".into(),
            stderr: String::new(),
            exit_code: 0,
        };
        let s = format_nextest_run_output(&out, &["nextest", "run"], Some("/tmp"));
        assert!(s.contains(INVOCATION_REASON));
        assert!(s.contains("\"status\":\"success\""));
    }

    #[test]
    fn format_nextest_run_output_includes_stderr_record_on_failure() {
        let out = CargoOutput {
            stdout: String::new(),
            stderr: "boom\n".into(),
            exit_code: 2,
        };
        let s = format_nextest_run_output(&out, &["nextest", "run"], None);
        assert!(s.contains("\"status\":\"error\""));
        assert!(s.contains("\"exit_code\":2"));
        assert!(s.contains(STDERR_REASON));
        assert!(s.contains("boom"));
    }

    #[test]
    fn validate_run_ignored_accepts_valid_values_and_rejects_others() {
        assert!(validate_run_ignored(None).is_ok());
        assert!(validate_run_ignored(Some("default")).is_ok());
        assert!(validate_run_ignored(Some("only")).is_ok());
        assert!(validate_run_ignored(Some("all")).is_ok());
        assert!(validate_run_ignored(Some("nope")).is_err());
    }

    #[test]
    fn validate_list_type_accepts_valid_values_and_rejects_others() {
        assert!(validate_list_type(None).is_ok());
        assert!(validate_list_type(Some("full")).is_ok());
        assert!(validate_list_type(Some("binaries-only")).is_ok());
        assert!(validate_list_type(Some("nope")).is_err());
    }

    #[test]
    fn format_nextest_list_output_compacts_pretty_printed_json() {
        // Defence in depth: even though `cargo_nextest_list` always
        // requests `--message-format json`, a future nextest could
        // start pretty-printing or interleave records. The formatter
        // must still emit exactly one JSON object per line so the
        // overall response (header + payload lines + trailer) parses
        // line-by-line.
        let pretty = "{\n  \"rust-build-meta\": {\n    \"target-directory\": \"target\"\n  },\n  \"test-count\": 1\n}\n";
        let out = CargoOutput {
            stdout: pretty.into(),
            stderr: String::new(),
            exit_code: 0,
        };
        let s = format_nextest_list_output(&out, &["nextest", "list"], None);
        for line in s.lines() {
            assert!(
                !line.trim().is_empty(),
                "blank line in framed output: {s:?}"
            );
            serde_json::from_str::<Value>(line).unwrap_or_else(|e| {
                panic!("line is not a single JSON object: {line:?} ({e}); full output: {s}")
            });
        }
        // The compacted payload preserves the original data.
        let payload_line = s
            .lines()
            .find(|l| l.contains("rust-build-meta"))
            .expect("payload line present");
        let v: Value = serde_json::from_str(payload_line).expect("payload parses");
        assert_eq!(v["test-count"], 1);
        assert_eq!(v["rust-build-meta"]["target-directory"], "target");
    }

    #[test]
    fn format_nextest_list_output_passes_non_json_lines_through() {
        // If upstream ever emits a non-JSON warning line we'd rather
        // forward it than silently drop it. The framing still holds
        // because the non-JSON line is just text — the overall output
        // stops being parseable JSON-per-line for that one record, but
        // nothing is lost.
        let mixed = "{\"test-count\":0}\nWARN: experimental feature\n";
        let out = CargoOutput {
            stdout: mixed.into(),
            stderr: String::new(),
            exit_code: 0,
        };
        let s = format_nextest_list_output(&out, &["nextest", "list"], None);
        assert!(s.contains("\"test-count\":0"));
        assert!(s.contains("WARN: experimental feature"));
    }
}
