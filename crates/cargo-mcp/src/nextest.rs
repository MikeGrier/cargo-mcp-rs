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

use regex::Regex;
use serde_json::Value;

use crate::invoke::{self, CargoOutput};
use crate::tools::{
    self, CommonOpts, STDERR_REASON, SummaryKind, ToolResult, combine_build_and_exec_output,
    invocation_header, is_build_finished_line, opt_bool, opt_int_str, opt_str, push_feature_flags,
    push_manifest_options, push_package_selection, run_phase, toolchain_arg,
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
    let stderr_trimmed = tools::stderr_for_display(&out.stderr);
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
/// The response is a **strict one-JSON-object-per-line stream**:
/// - The whole-blob path: when all of stdout parses as a single JSON
///   document (today's normal case, plus a future nextest that ever
///   pretty-prints), it is re-serialised in compact form so it occupies
///   exactly one line.
/// - The per-line fallback: when the whole-blob parse fails, each
///   non-empty stdout line is handled individually — lines that parse
///   as JSON are compacted verbatim, lines that do NOT parse as JSON
///   (e.g. a warning nextest prints alongside the discovery payload)
///   are wrapped in an [`NEXTEST_OUTPUT_REASON`] record so the line is
///   preserved without breaking the NDJSON contract.
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
        // fail on every brace line. If the whole-blob parse fails,
        // fall back to line-by-line compaction so an NDJSON-style
        // stream (cargo build records ahead of the list payload)
        // still works.
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
                    // Non-JSON line (e.g. an upstream warning printed
                    // alongside the discovery payload). Wrap so the
                    // overall response stays a strict NDJSON stream.
                    Err(_) => serde_json::to_string(&serde_json::json!({
                        "reason": NEXTEST_OUTPUT_REASON,
                        "text": l,
                    }))
                    .unwrap_or_else(|_| "{}".into()),
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

/// Translate `cargo_test`'s `test_filter` (a `{ pattern, include_ignored }`
/// regex object) into the nextest-native equivalent: a `test(/pattern/)`
/// filterset expression plus, when `include_ignored` is true, a
/// `--run-ignored all`. This exists because callers habitually reach for
/// `test_filter` on `cargo_nextest_run` out of `cargo_test` muscle memory;
/// translating it removes that recurring failure entirely instead of just
/// producing a better error. Returns `Ok(None)` when `test_filter` is absent
/// or `null`.
///
/// Nextest's filterset regex is delimited by `/`, which it has no escape
/// sequence for, so a `pattern` containing a literal `/` is rejected with a
/// pointer to `filter_expr` rather than silently producing a broken
/// expression.
fn translate_test_filter(
    args: &Value,
) -> Result<Option<(String, bool)>, Box<dyn std::error::Error>> {
    let Some(v) = args.get("test_filter") else {
        return Ok(None);
    };
    if v.is_null() {
        return Ok(None);
    }
    let obj = v.as_object().ok_or_else(|| -> Box<dyn std::error::Error> {
        format!("test_filter must be an object with a `pattern` field, got {v}").into()
    })?;
    let pattern = obj.get("pattern").and_then(|p| p.as_str()).ok_or_else(
        || -> Box<dyn std::error::Error> { "test_filter.pattern must be a string".into() },
    )?;
    if pattern.is_empty() {
        return Err("test_filter.pattern must not be empty".into());
    }
    if pattern.contains('/') {
        return Err(format!(
            "test_filter.pattern {pattern:?} contains `/`, which nextest's `test(/regex/)` \
             filterset syntax uses as an unescapable delimiter. Pass the equivalent \
             `filter_expr` directly instead (see https://nexte.st/docs/filtersets)."
        )
        .into());
    }
    Regex::new(pattern).map_err(|e| -> Box<dyn std::error::Error> {
        format!("test_filter.pattern is not a valid regex: {e}").into()
    })?;
    let include_ignored = tools::opt_bool(v, "include_ignored");
    Ok(Some((format!("test(/{pattern}/)"), include_ignored)))
}

/// Escape a literal string for embedding inside a nextest filterset name
/// matcher (`=string`, `~string`, or the bare-word default). Per the
/// filterset escape-sequence grammar, `\`, `/`, `)`, and `,` must be escaped;
/// Rust test names cannot themselves contain most other metacharacters, but
/// this covers the full escapable set defensively.
fn escape_filterset_matcher(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if matches!(c, '\\' | '/' | ')' | ',') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Translate `cargo_test`'s `test_name` (+ optional `exact`) into the
/// nextest-native equivalent, so callers reaching for `test_name` on
/// `cargo_nextest_run`/`cargo_nextest_list` out of `cargo_test` habit get the
/// expected selection instead of an "unknown parameter" error:
///
/// - `test_name` alone (substring match) becomes nextest's own bare `filter`
///   positional argument, which is already a libtest-compatible substring
///   filter — no filterset expression needed.
/// - `test_name` + `exact: true` becomes `test(=name)`, the filterset
///   equality matcher, since a bare positional `filter` has no exact-match
///   mode.
///
/// Mirrors `cargo_test`'s own handling of `exact` without `test_name`: the
/// flag is meaningless without a name to match, so it is silently ignored
/// rather than rejected (see `build_doc_test_argv`).
fn translate_test_name(args: &Value) -> Option<(Option<String>, Option<String>)> {
    let name = opt_str(args, "test_name")?;
    if opt_bool(args, "exact") {
        Some((
            None,
            Some(format!("test(={})", escape_filterset_matcher(name))),
        ))
    } else {
        Some((Some(name.to_owned()), None))
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

    /// Fold a translated `test_filter` (see [`translate_test_filter`]) into
    /// this struct's `filter_expr` / `run_ignored`, rejecting the call when
    /// the caller also supplied one of those directly — with both present
    /// there is no non-surprising way to decide which selection wins.
    fn apply_test_filter(&mut self, args: &Value) -> Result<(), Box<dyn std::error::Error>> {
        let Some((expr, include_ignored)) = translate_test_filter(args)? else {
            return Ok(());
        };
        if self.filter_expr.is_some() {
            return Err("test_filter is not supported together with filter_expr on \
                 cargo_nextest_run/cargo_nextest_list; test_filter is automatically \
                 translated into an equivalent filter_expr, so supplying both is \
                 ambiguous. Use just one."
                .into());
        }
        if self.filter.is_some() {
            return Err("test_filter is not supported together with filter on \
                 cargo_nextest_run/cargo_nextest_list; use just one selection mechanism."
                .into());
        }
        if self.run_ignored.is_some() {
            return Err("test_filter is not supported together with run_ignored on \
                 cargo_nextest_run/cargo_nextest_list; test_filter.include_ignored \
                 already derives the equivalent `--run-ignored` value automatically."
                .into());
        }
        self.filter_expr = Some(expr);
        if include_ignored {
            self.run_ignored = Some("all".to_owned());
        }
        Ok(())
    }

    /// Fold a translated `test_name`/`exact` (see [`translate_test_name`])
    /// into this struct's `filter` / `filter_expr`, rejecting the call when
    /// the caller also supplied `filter`, `filter_expr`, or `test_filter`
    /// directly — ambiguous which selection should win.
    fn apply_test_name(&mut self, args: &Value) -> Result<(), Box<dyn std::error::Error>> {
        let Some((filter, filter_expr)) = translate_test_name(args) else {
            return Ok(());
        };
        if self.filter.is_some() {
            return Err("test_name is not supported together with filter on \
                 cargo_nextest_run/cargo_nextest_list; test_name is automatically \
                 translated into filter, so supplying both is ambiguous. Use just one."
                .into());
        }
        if self.filter_expr.is_some() {
            return Err("test_name is not supported together with filter_expr or \
                 test_filter on cargo_nextest_run/cargo_nextest_list; use just one \
                 selection mechanism."
                .into());
        }
        self.filter = filter;
        self.filter_expr = filter_expr;
        Ok(())
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
    // Bisection mode takes precedence and uses the shared engine (which runs
    // the compiled libtest binaries directly), so `bisect` behaves identically
    // whether requested via cargo_test or cargo_nextest_run.
    if crate::bisect::is_bisect_requested(args) {
        if tools::opt_test_timeout_explicit(args)?.is_some() {
            return Err(
                "test_timeout_secs is not supported together with bisect; the bisection \
                 engine has its own `group_timeout_secs` / `slow_threshold_secs` budget \
                 model instead of the build/execute phase split. Use those instead."
                    .into(),
            );
        }
        return crate::bisect::run(args, on_progress)?.ok_or_else(
            || -> Box<dyn std::error::Error> {
                "bisect requested but the bisection engine returned no result".into()
            },
        );
    }
    let wd = opt_str(args, "working_dir");
    let output_path = opt_str(args, "output_path");
    if let Some(p) = output_path {
        validate_relative_output_path(p, wd)?;
    }
    let tc = toolchain_arg(args);
    let o = CommonOpts::from_args(args);
    let mut nx = NextestOwnedOpts::from_args(args);
    nx.apply_test_filter(args)?;
    nx.apply_test_name(args)?;
    validate_run_ignored(nx.run_ignored.as_deref())?;

    // nextest's `run` subcommand. We always ask cargo to emit JSON build
    // messages so the existing compiler-message / build-finished pipeline
    // works unchanged for the build phase.
    //
    // `base` holds the selectors shared by the build (`--no-run`) and the
    // execution phase; the per-phase argv is derived from it below.
    let mut base: Vec<&str> = vec!["nextest", "run", "--cargo-message-format=json"];

    // Nextest profile (selects per-test config from .config/nextest.toml).
    if let Some(p) = &nx.nextest_profile {
        base.push("--profile");
        base.push(p);
    }

    // Standard cargo selectors.
    push_package_selection(&mut base, args, &o);
    push_nextest_target_selection(&mut base, args, &o);
    push_feature_flags(&mut base, args, &o);
    push_nextest_compilation_options(
        &mut base,
        args,
        nx.cargo_profile.as_ref(),
        nx.build_jobs.as_ref(),
        o.target.as_ref(),
        o.target_dir.as_ref(),
    );
    // `ignore_rust_version` is supported by nextest (it forwards to cargo).
    push_manifest_options(&mut base, args, &o, true);
    if let Some(ref t) = tc {
        base.insert(0, t);
    }

    // Same three-state timeout selection as cargo_test: caller wins; missing
    // falls back to the server-wide default. The budget is applied
    // INDEPENDENTLY to each phase below, so a slow build never consumes the
    // test-execution budget. `test_timeout_secs` overrides the execution
    // phase specifically — see resolve_test_phase_timeouts. Per-test
    // enforcement is left to nextest's profile (slow-timeout / terminate-after).
    let (build_timeout, test_timeout) = tools::resolve_test_phase_timeouts(args)?;

    let mut on_progress = on_progress;

    // ── Phase 1: build (`--no-run`) ──────────────────────────────────────────
    // Compile the test binaries up front so the build time is excluded from the
    // test-execution budget. The timeout arms immediately so it bounds the
    // whole build; a timeout here is labelled "build".
    let mut build_argv = base.clone();
    build_argv.push("--no-run");
    let build_out = run_phase(
        &build_argv,
        wd,
        build_timeout,
        None,
        &mut on_progress,
        "build",
    )?;

    // If the build failed, or the caller only wanted a build (`no_run`), return
    // the build output directly without an execution phase.
    let build_failed = build_out.exit_code != 0;
    if build_failed || opt_bool(args, "no_run") {
        let body = format_nextest_run_output(&build_out, &build_argv, wd);
        let text = write_output_path_and_summarize(body, output_path, wd, SummaryKind::Test)?;
        return Ok(ToolResult::Text {
            text,
            is_error: build_failed,
        });
    }

    // ── Phase 2: test execution ──────────────────────────────────────────────
    // Re-run with the full set of run flags; the build is now a cache hit, so
    // the timeout (armed on build-finished) bounds test execution only. A
    // timeout here is labelled "test execution".
    let mut argv = base.clone();
    // Nextest-specific run flags.
    if opt_bool(args, "no_fail_fast") {
        argv.push("--no-fail-fast");
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

    let exec_out = run_phase(
        &argv,
        wd,
        test_timeout,
        Some(&is_build_finished_line),
        &mut on_progress,
        "test execution",
    )?;

    // Merge the build phase's compiler-message records (warnings emitted during
    // the now-cached build) into the execution output before formatting.
    let combined = combine_build_and_exec_output(&build_out, &exec_out);
    let is_error = exec_out.exit_code != 0;
    let body = format_nextest_run_output(&combined, &argv, wd);
    let text = write_output_path_and_summarize(body, output_path, wd, SummaryKind::Test)?;
    Ok(ToolResult::Text { text, is_error })
}

/// Implementation of the `cargo_nextest_list` tool.
pub(crate) fn call_list(args: &Value) -> Result<ToolResult, Box<dyn std::error::Error>> {
    let wd = opt_str(args, "working_dir");
    let tc = toolchain_arg(args);
    let o = CommonOpts::from_args(args);
    let mut nx = NextestOwnedOpts::from_args(args);
    nx.apply_test_filter(args)?;
    nx.apply_test_name(args)?;
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
    fn call_run_rejects_bisect_with_test_timeout_secs() {
        // The bisection engine has its own group_timeout_secs /
        // slow_threshold_secs budget model instead of the build/execute
        // phase split, so test_timeout_secs must be rejected rather than
        // silently ignored (no working_dir needed — rejected before any
        // cargo subprocess is spawned).
        let args = serde_json::json!({
            "bisect": { "group_timeout_secs": 10 },
            "test_timeout_secs": 10,
        });
        match call_run(&args, None) {
            Err(e) => {
                assert!(e.to_string().contains("test_timeout_secs"));
                assert!(e.to_string().contains("bisect"));
            }
            Ok(_) => panic!("expected bisect + test_timeout_secs to be rejected"),
        }
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
    fn translate_test_filter_builds_filterset_expression() {
        let args = serde_json::json!({ "test_filter": { "pattern": "mod::test_a" } });
        let (expr, include_ignored) = translate_test_filter(&args).unwrap().unwrap();
        assert_eq!(expr, "test(/mod::test_a/)");
        assert!(!include_ignored);
    }

    #[test]
    fn translate_test_filter_returns_none_when_absent_or_null() {
        assert!(
            translate_test_filter(&serde_json::json!({}))
                .unwrap()
                .is_none()
        );
        assert!(
            translate_test_filter(&serde_json::json!({ "test_filter": null }))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn translate_test_filter_rejects_pattern_with_slash() {
        // nextest's `test(/regex/)` delimiter has no escape for a literal
        // `/`, so a pattern containing one must be rejected rather than
        // silently producing a broken filterset expression.
        let args = serde_json::json!({ "test_filter": { "pattern": "a/b" } });
        let err = translate_test_filter(&args).unwrap_err();
        assert!(err.to_string().contains('/'));
        assert!(err.to_string().contains("filter_expr"));
    }

    #[test]
    fn translate_test_filter_rejects_invalid_regex() {
        let args = serde_json::json!({ "test_filter": { "pattern": "(unclosed" } });
        let err = translate_test_filter(&args).unwrap_err();
        assert!(err.to_string().contains("not a valid regex"));
    }

    #[test]
    fn translate_test_filter_propagates_include_ignored() {
        let args =
            serde_json::json!({ "test_filter": { "pattern": "x", "include_ignored": true } });
        let (_, include_ignored) = translate_test_filter(&args).unwrap().unwrap();
        assert!(include_ignored);
    }

    #[test]
    fn apply_test_filter_sets_filter_expr_and_run_ignored() {
        let args =
            serde_json::json!({ "test_filter": { "pattern": "x", "include_ignored": true } });
        let mut nx = NextestOwnedOpts::from_args(&args);
        nx.apply_test_filter(&args).unwrap();
        assert_eq!(nx.filter_expr.as_deref(), Some("test(/x/)"));
        assert_eq!(nx.run_ignored.as_deref(), Some("all"));
    }

    #[test]
    fn apply_test_filter_leaves_run_ignored_unset_when_not_include_ignored() {
        let args = serde_json::json!({ "test_filter": { "pattern": "x" } });
        let mut nx = NextestOwnedOpts::from_args(&args);
        nx.apply_test_filter(&args).unwrap();
        assert_eq!(nx.filter_expr.as_deref(), Some("test(/x/)"));
        assert!(nx.run_ignored.is_none());
    }

    #[test]
    fn apply_test_filter_rejects_combination_with_filter_expr() {
        let args = serde_json::json!({
            "test_filter": { "pattern": "x" },
            "filter_expr": "test(y)",
        });
        let mut nx = NextestOwnedOpts::from_args(&args);
        let err = nx.apply_test_filter(&args).unwrap_err();
        assert!(err.to_string().contains("filter_expr"));
    }

    #[test]
    fn apply_test_filter_rejects_combination_with_filter() {
        let args = serde_json::json!({
            "test_filter": { "pattern": "x" },
            "filter": "y",
        });
        let mut nx = NextestOwnedOpts::from_args(&args);
        let err = nx.apply_test_filter(&args).unwrap_err();
        assert!(err.to_string().contains("filter"));
    }

    #[test]
    fn apply_test_filter_rejects_combination_with_run_ignored() {
        let args = serde_json::json!({
            "test_filter": { "pattern": "x" },
            "run_ignored": "all",
        });
        let mut nx = NextestOwnedOpts::from_args(&args);
        let err = nx.apply_test_filter(&args).unwrap_err();
        assert!(err.to_string().contains("run_ignored"));
    }

    #[test]
    fn call_run_rejects_test_filter_with_filter_expr() {
        // Rejected before any cargo subprocess is spawned, so no
        // working_dir is needed.
        let args = serde_json::json!({
            "test_filter": { "pattern": "x" },
            "filter_expr": "test(y)",
        });
        match call_run(&args, None) {
            Err(e) => assert!(e.to_string().contains("filter_expr")),
            Ok(_) => panic!("expected test_filter + filter_expr to be rejected"),
        }
    }

    #[test]
    fn escape_filterset_matcher_escapes_special_characters() {
        assert_eq!(escape_filterset_matcher("plain"), "plain");
        assert_eq!(escape_filterset_matcher("a/b\\c)d,e"), "a\\/b\\\\c\\)d\\,e");
    }

    #[test]
    fn translate_test_name_returns_none_when_absent() {
        assert!(translate_test_name(&serde_json::json!({})).is_none());
        assert!(
            translate_test_name(&serde_json::json!({ "exact": true })).is_none(),
            "exact without test_name is meaningless and ignored, mirroring cargo_test"
        );
    }

    #[test]
    fn translate_test_name_becomes_bare_filter_when_not_exact() {
        let args = serde_json::json!({ "test_name": "mod::test_a" });
        let (filter, filter_expr) = translate_test_name(&args).unwrap();
        assert_eq!(filter.as_deref(), Some("mod::test_a"));
        assert!(filter_expr.is_none());
    }

    #[test]
    fn translate_test_name_becomes_equality_filterset_expr_when_exact() {
        let args = serde_json::json!({ "test_name": "mod::test_a", "exact": true });
        let (filter, filter_expr) = translate_test_name(&args).unwrap();
        assert!(filter.is_none());
        assert_eq!(filter_expr.as_deref(), Some("test(=mod::test_a)"));
    }

    #[test]
    fn translate_test_name_escapes_exact_matcher() {
        let args = serde_json::json!({ "test_name": "a/b", "exact": true });
        let (_, filter_expr) = translate_test_name(&args).unwrap();
        assert_eq!(filter_expr.as_deref(), Some("test(=a\\/b)"));
    }

    #[test]
    fn apply_test_name_sets_filter_when_not_exact() {
        let args = serde_json::json!({ "test_name": "x" });
        let mut nx = NextestOwnedOpts::from_args(&args);
        nx.apply_test_name(&args).unwrap();
        assert_eq!(nx.filter.as_deref(), Some("x"));
        assert!(nx.filter_expr.is_none());
    }

    #[test]
    fn apply_test_name_sets_filter_expr_when_exact() {
        let args = serde_json::json!({ "test_name": "x", "exact": true });
        let mut nx = NextestOwnedOpts::from_args(&args);
        nx.apply_test_name(&args).unwrap();
        assert!(nx.filter.is_none());
        assert_eq!(nx.filter_expr.as_deref(), Some("test(=x)"));
    }

    #[test]
    fn apply_test_name_rejects_combination_with_filter() {
        let args = serde_json::json!({ "test_name": "x", "filter": "y" });
        let mut nx = NextestOwnedOpts::from_args(&args);
        let err = nx.apply_test_name(&args).unwrap_err();
        assert!(err.to_string().contains("filter"));
    }

    #[test]
    fn apply_test_name_rejects_combination_with_filter_expr() {
        let args = serde_json::json!({ "test_name": "x", "filter_expr": "test(y)" });
        let mut nx = NextestOwnedOpts::from_args(&args);
        let err = nx.apply_test_name(&args).unwrap_err();
        assert!(err.to_string().contains("filter_expr"));
    }

    #[test]
    fn apply_test_name_rejects_combination_with_test_filter() {
        let args = serde_json::json!({ "test_name": "x", "test_filter": { "pattern": "y" } });
        let mut nx = NextestOwnedOpts::from_args(&args);
        // apply_test_filter runs first in the real call sites, folding
        // test_filter into filter_expr; apply_test_name then sees that
        // filter_expr occupied and rejects the ambiguous combination.
        nx.apply_test_filter(&args).unwrap();
        let err = nx.apply_test_name(&args).unwrap_err();
        assert!(err.to_string().contains("test_filter"));
    }

    #[test]
    fn call_run_rejects_test_name_with_filter() {
        let args = serde_json::json!({ "test_name": "x", "filter": "y" });
        match call_run(&args, None) {
            Err(e) => assert!(e.to_string().contains("filter")),
            Ok(_) => panic!("expected test_name + filter to be rejected"),
        }
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
    fn format_nextest_list_output_wraps_non_json_lines_in_nextest_output_records() {
        // Regression: if upstream ever emits a non-JSON warning line
        // alongside the discovery payload, we must wrap it in an
        // NEXTEST_OUTPUT_REASON record rather than forwarding it
        // verbatim — otherwise a single warning breaks the tool's
        // "one JSON object per line" framing contract for every
        // downstream consumer doing line-by-line JSON parsing.
        let mixed = "{\"test-count\":0}\nWARN: experimental feature\n";
        let out = CargoOutput {
            stdout: mixed.into(),
            stderr: String::new(),
            exit_code: 0,
        };
        let s = format_nextest_list_output(&out, &["nextest", "list"], None);

        // Every emitted line must be valid JSON — this is the
        // load-bearing assertion this regression test exists to lock
        // in.
        for line in s.lines() {
            assert!(
                !line.trim().is_empty(),
                "blank line in framed output: {s:?}"
            );
            serde_json::from_str::<Value>(line).unwrap_or_else(|e| {
                panic!("line is not a single JSON object: {line:?} ({e}); full output: {s}")
            });
        }

        // The compacted payload is preserved.
        assert!(s.contains("\"test-count\":0"));

        // The warning is preserved, but as the `text` field of a
        // wrapped NEXTEST_OUTPUT_REASON record (not as raw text).
        let warn_line = s
            .lines()
            .find(|l| l.contains("experimental feature"))
            .expect("warning line present");
        let v: Value = serde_json::from_str(warn_line).expect("warning line parses as JSON");
        assert_eq!(v["reason"], NEXTEST_OUTPUT_REASON);
        assert_eq!(v["text"], "WARN: experimental feature");
    }
}
