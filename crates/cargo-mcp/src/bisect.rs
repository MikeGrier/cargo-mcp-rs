//! Hang / slow-test bisection engine, shared by `cargo_test` and
//! `cargo_nextest_run`.
//!
//! When a caller passes a `bisect` object on either tool, control is routed
//! here instead of to the normal execution path. The engine:
//!
//! 1. Builds the test binaries once (`cargo test --no-run --message-format=json`)
//!    and parses the compiled artifacts (reusing [`crate::test_filter`]).
//! 2. Enumerates every test in each binary via libtest `--list`, optionally
//!    pre-filtered by a `pattern` regex.
//! 3. Bisects each binary's test set: it runs a group of tests under a short
//!    `group_timeout_secs` kill-deadline (always with `--test-threads 1` so a
//!    group's wall-clock time is the *sum* of its members' times, and a single
//!    hung test reliably wedges the whole group). A group is "interesting" when
//!    it either times out (a hang) or — when `slow_threshold_secs` is set —
//!    completes but exceeds that threshold (slow). Interesting groups are split
//!    into `split_factor` (or `split_percent`-derived) sub-groups and re-run,
//!    recursing until the group is down to `min_group_size` tests or
//!    `max_rounds` of subdivision is reached. The surviving tests in an
//!    interesting leaf group are reported as culprits.
//!
//! The taxonomy at a leaf: a leaf that *timed out* is reported `hung` (it
//! exceeded the kill deadline — an infinite loop, deadlock, or simply slower
//! than `group_timeout_secs`); a leaf that *completed but was over
//! `slow_threshold_secs`* is reported `slow`. Choose `group_timeout_secs`
//! large enough that a legitimately long-but-finite test still completes (so it
//! is classified `slow`, not `hung`), and `slow_threshold_secs` below it to
//! flag the merely-slow.
//!
//! Execution runs the compiled libtest binary **directly** (not through a fresh
//! `cargo test` per group) for precise, low-overhead timing. This bypasses any
//! cargo-set *runtime* environment variables; compile-time `env!()` values are
//! unaffected.

use std::path::Path;
use std::time::{Duration, Instant};

use regex::Regex;
use serde_json::Value;

use crate::invoke::{self};
use crate::test_filter::{TestBinary, enumerate_tests, parse_no_run_artifacts};
use crate::tools::{
    self, CommonOpts, SummaryKind, ToolResult, invocation_header, opt_str,
    push_compilation_options, push_feature_flags, push_manifest_options, push_package_selection,
    push_target_selection, toolchain_arg, validate_relative_output_path,
    write_output_path_and_summarize,
};

// ── NDJSON record discriminators ────────────────────────────────────────────

const CONFIG_REASON: &str = "x-cargo-mcp-bisect-config";
const GROUP_REASON: &str = "x-cargo-mcp-bisect-group";
const CULPRIT_REASON: &str = "x-cargo-mcp-bisect-culprit";
const SUMMARY_REASON: &str = "x-cargo-mcp-bisect-summary";

/// Conservative byte budget for a single `--exact name1 name2 …` argv so a
/// large group's launch stays well under the OS command-line limit (Windows
/// ~32 KiB). Groups bigger than this are split into several sequential
/// subprocess launches whose elapsed times are summed.
const ARG_BYTE_BUDGET: usize = 24_000;

/// Default number of sub-groups an interesting group is split into when neither
/// `split_factor` nor `split_percent` is given (binary bisection).
const DEFAULT_SPLIT_FACTOR: usize = 2;

/// Default cap on subdivision depth, a safety net against pathological inputs.
const DEFAULT_MAX_ROUNDS: usize = 32;

// ── option parsing ──────────────────────────────────────────────────────────

/// Resolved bisection knobs, parsed and validated from the `bisect` object.
struct BisectOpts {
    group_timeout: Duration,
    slow_threshold: Option<Duration>,
    split_factor: usize,
    min_group_size: usize,
    initial_group_size: Option<usize>,
    initial_groups: Option<usize>,
    max_rounds: usize,
    pattern: Option<Regex>,
    include_ignored: bool,
}

/// Read a strictly-positive seconds value (accepts integer or float JSON).
fn opt_pos_secs(o: &Value, key: &str) -> Result<Option<Duration>, Box<dyn std::error::Error>> {
    match o.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => {
            let secs = v
                .as_f64()
                .ok_or_else(|| format!("bisect.{key} must be a number (seconds)"))?;
            if !(secs.is_finite() && secs > 0.0) {
                return Err(format!("bisect.{key} must be a positive number of seconds").into());
            }
            Ok(Some(Duration::from_secs_f64(secs)))
        }
    }
}

/// Read a `usize` with an inclusive lower bound (accepts integer JSON only).
fn opt_usize_min(
    o: &Value,
    key: &str,
    min: u64,
) -> Result<Option<usize>, Box<dyn std::error::Error>> {
    match o.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => {
            let n = v
                .as_u64()
                .ok_or_else(|| format!("bisect.{key} must be a non-negative integer"))?;
            if n < min {
                return Err(format!("bisect.{key} must be at least {min}").into());
            }
            Ok(Some(n as usize))
        }
    }
}

impl BisectOpts {
    fn from_args(args: &Value) -> Result<Option<Self>, Box<dyn std::error::Error>> {
        let Some(o) = args.get("bisect") else {
            return Ok(None);
        };
        if !o.is_object() {
            return Err("bisect must be an object with at least `group_timeout_secs`".into());
        }

        let group_timeout = opt_pos_secs(o, "group_timeout_secs")?
            .ok_or("bisect.group_timeout_secs is required and must be a positive number")?;
        let slow_threshold = opt_pos_secs(o, "slow_threshold_secs")?;
        if let Some(slow) = slow_threshold
            && slow >= group_timeout
        {
            return Err(
                "bisect.slow_threshold_secs must be smaller than group_timeout_secs".into(),
            );
        }

        // split_percent (when given) overrides split_factor: a percentage p
        // yields ceil(100 / p) sub-groups (e.g. 25% -> 4, 33% -> 4, 50% -> 2).
        let split_factor = match o.get("split_percent") {
            Some(Value::Null) | None => {
                opt_usize_min(o, "split_factor", 2)?.unwrap_or(DEFAULT_SPLIT_FACTOR)
            }
            Some(v) => {
                let p = v
                    .as_f64()
                    .ok_or("bisect.split_percent must be a number in (0, 100]")?;
                if !(p.is_finite() && p > 0.0 && p <= 100.0) {
                    return Err("bisect.split_percent must be a number in (0, 100]".into());
                }
                if o.get("split_factor").is_some_and(|v| !v.is_null()) {
                    return Err(
                        "bisect: set only one of split_factor or split_percent, not both".into(),
                    );
                }
                (100.0 / p).ceil().max(2.0) as usize
            }
        };

        let min_group_size = opt_usize_min(o, "min_group_size", 1)?.unwrap_or(1);
        let initial_group_size = opt_usize_min(o, "initial_group_size", 1)?;
        let initial_groups = opt_usize_min(o, "initial_groups", 1)?;
        if initial_group_size.is_some() && initial_groups.is_some() {
            return Err(
                "bisect: set only one of initial_group_size or initial_groups, not both".into(),
            );
        }
        let max_rounds = opt_usize_min(o, "max_rounds", 1)?.unwrap_or(DEFAULT_MAX_ROUNDS);

        let pattern = match o.get("pattern") {
            Some(Value::Null) | None => None,
            Some(Value::String(p)) => Some(
                Regex::new(p).map_err(|e| format!("bisect.pattern is not a valid regex: {e}"))?,
            ),
            Some(_) => return Err("bisect.pattern must be a string regex".into()),
        };
        let include_ignored = o
            .get("include_ignored")
            .and_then(Value::as_bool)
            .unwrap_or(false);

        Ok(Some(BisectOpts {
            group_timeout,
            slow_threshold,
            split_factor,
            min_group_size,
            initial_group_size,
            initial_groups,
            max_rounds,
            pattern,
            include_ignored,
        }))
    }
}

// ── public entry ────────────────────────────────────────────────────────────

/// True when the caller requested bisection (`bisect` object present).
pub fn is_bisect_requested(args: &Value) -> bool {
    args.get("bisect").map(|v| !v.is_null()).unwrap_or(false)
}

/// A single test flagged by bisection as hung or slow.
struct Culprit {
    package: String,
    target: String,
    test: String,
    classification: &'static str,
    elapsed: Duration,
}

/// The result of running one group of tests.
struct GroupRun {
    elapsed: Duration,
    timed_out: bool,
    /// The test binary exited non-zero (a real test failure, not a hang).
    failed: bool,
}

/// Per-group reporting payload passed to [`emit_group_record`].
struct GroupReport<'a> {
    group: &'a [String],
    depth: usize,
    run: &'a GroupRun,
    outcome: &'a str,
}

/// Run the bisection pipeline. Returns `Ok(None)` when `bisect` was not
/// requested (so the caller falls through to the normal path).
pub fn run(
    args: &Value,
    on_progress: Option<&mut dyn FnMut(&str)>,
) -> Result<Option<ToolResult>, Box<dyn std::error::Error>> {
    let Some(opts) = BisectOpts::from_args(args)? else {
        return Ok(None);
    };
    let wd = opt_str(args, "working_dir");
    let output_path = opt_str(args, "output_path");
    if let Some(p) = output_path {
        validate_relative_output_path(p, wd)?;
    }
    let toolchain = toolchain_arg(args);
    let common = CommonOpts::from_args(args);

    let mut sink: Box<dyn FnMut(&str)> = match on_progress {
        Some(cb) => Box::new(move |line: &str| cb(line)),
        None => Box::new(|_line: &str| {}),
    };

    // ── phase 1: --no-run build ─────────────────────────────────────────────
    let mut no_run_argv: Vec<&str> = vec!["test", "--no-run", "--message-format=json"];
    push_package_selection(&mut no_run_argv, args, &common);
    push_target_selection(&mut no_run_argv, args, &common);
    push_feature_flags(&mut no_run_argv, args, &common);
    push_compilation_options(&mut no_run_argv, args, &common, false);
    push_manifest_options(&mut no_run_argv, args, &common, true);
    if let Some(ref t) = toolchain {
        no_run_argv.insert(0, t);
    }
    let no_run_out =
        invoke::run_cargo_streaming_with_timeout(&no_run_argv, wd, None, None, sink.as_mut())?;
    if no_run_out.exit_code != 0 {
        // Compilation failed: surface the build output and bail before
        // enumeration — there is nothing to bisect.
        let body = tools::format_test_output(&no_run_out, &no_run_argv, wd);
        let text = write_output_path_and_summarize(body, output_path, wd, SummaryKind::Test)?;
        return Ok(Some(ToolResult::Text {
            text,
            is_error: true,
        }));
    }
    let binaries = parse_no_run_artifacts(&no_run_out.stdout);

    // ── phase 2: enumerate + bisect ─────────────────────────────────────────
    let start = Instant::now();
    let mut body = String::new();
    // Header argv mirrors the conceptual command for the invocation record.
    let mut header_argv = no_run_argv.clone();
    header_argv.retain(|a| *a != "--no-run" && *a != "--message-format=json");
    header_argv.push("--bisect");
    body.push_str(&invocation_header(&header_argv, wd));
    body.push('\n');
    body.push_str(&config_record(&opts));
    body.push('\n');

    let mut enumeration_errors: Vec<String> = Vec::new();
    let mut tests_enumerated = 0usize;
    let mut tests_considered = 0usize;
    let mut groups_run = 0usize;
    let mut culprits: Vec<Culprit> = Vec::new();
    let mut binaries_with_tests = 0usize;

    for binary in &binaries {
        let all = match enumerate_tests(&binary.executable, opts.include_ignored, wd) {
            Ok(t) => t,
            Err(e) => {
                let msg = if e.is::<invoke::TimeoutError>() {
                    "enumeration timed out (`--list` did not finish)".to_string()
                } else {
                    format!("{e}")
                };
                enumeration_errors.push(format!("{}: {msg}", binary.executable.display()));
                continue;
            }
        };
        tests_enumerated += all.len();
        let selected: Vec<String> = match &opts.pattern {
            Some(re) => all.into_iter().filter(|n| re.is_match(n)).collect(),
            None => all,
        };
        if selected.is_empty() {
            continue;
        }
        binaries_with_tests += 1;
        tests_considered += selected.len();
        bisect_binary(
            binary,
            selected,
            &opts,
            wd,
            &mut sink,
            &mut groups_run,
            &mut culprits,
            &mut body,
        )?;
    }

    // ── culprit + summary records ───────────────────────────────────────────
    for c in &culprits {
        body.push_str(&culprit_record(c, opts.group_timeout));
        body.push('\n');
    }
    let hung = culprits
        .iter()
        .filter(|c| c.classification == "hung")
        .count();
    let failed = culprits
        .iter()
        .filter(|c| c.classification == "failed")
        .count();
    let slow = culprits.len() - hung - failed;
    let summary = serde_json::json!({
        "reason": SUMMARY_REASON,
        "binaries_built": binaries.len(),
        "binaries_with_tests": binaries_with_tests,
        "tests_enumerated": tests_enumerated,
        "tests_considered": tests_considered,
        "groups_run": groups_run,
        "culprits": culprits.len(),
        "hung": hung,
        "failed": failed,
        "slow": slow,
        "wall_secs": round3(start.elapsed().as_secs_f64()),
        "enumeration_errors": enumeration_errors,
    });
    body.push_str(&summary.to_string());
    body.push('\n');

    let is_error = !culprits.is_empty() || !enumeration_errors.is_empty();
    let status = if is_error { "error" } else { "success" };
    body.push_str(&serde_json::json!({ "status": status }).to_string());
    body.push('\n');

    let text = write_bisect_output(body, output_path, wd)?;
    Ok(Some(ToolResult::Text { text, is_error }))
}

// ── per-binary bisection ────────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
fn bisect_binary(
    binary: &TestBinary,
    selected: Vec<String>,
    opts: &BisectOpts,
    wd: Option<&str>,
    sink: &mut dyn FnMut(&str),
    groups_run: &mut usize,
    culprits: &mut Vec<Culprit>,
    body: &mut String,
) -> Result<(), Box<dyn std::error::Error>> {
    // Form the initial groups (a stack of (names, depth) processed depth-first
    // so a single culprit is narrowed before moving to the next group).
    let initial = initial_groups(&selected, opts);
    let mut stack: Vec<(Vec<String>, usize)> = initial.into_iter().map(|g| (g, 0usize)).collect();
    // Reverse so the first group is processed first under LIFO order.
    stack.reverse();

    while let Some((group, depth)) = stack.pop() {
        if group.is_empty() {
            continue;
        }
        let refs: Vec<&str> = group.iter().map(String::as_str).collect();
        let run = run_group(
            &binary.executable,
            &refs,
            opts.include_ignored,
            opts.group_timeout,
            wd,
        )?;
        *groups_run += 1;

        let slow = opts
            .slow_threshold
            .is_some_and(|t| !run.timed_out && !run.failed && run.elapsed > t);
        let interesting = run.timed_out || run.failed || slow;
        let outcome = if run.timed_out {
            "hung"
        } else if run.failed {
            "failed"
        } else if slow {
            "slow"
        } else {
            "clean"
        };
        emit_group_record(
            body,
            sink,
            binary,
            opts,
            &GroupReport {
                group: &group,
                depth,
                run: &run,
                outcome,
            },
        );

        if !interesting {
            continue;
        }
        if group.len() <= opts.min_group_size || depth >= opts.max_rounds {
            // Leaf: every surviving test is a culprit.
            let classification = if run.timed_out {
                "hung"
            } else if run.failed {
                "failed"
            } else {
                "slow"
            };
            // Approximate per-test elapsed when several tests share a leaf.
            let per = run.elapsed / (group.len() as u32).max(1);
            for name in &group {
                culprits.push(Culprit {
                    package: binary.package_name.clone(),
                    target: binary.target_name.clone(),
                    test: name.clone(),
                    classification,
                    elapsed: per,
                });
            }
            continue;
        }
        // Subdivide and recurse.
        let k = opts.split_factor.min(group.len()).max(2);
        let mut subs = split_into(&group, k);
        // Push in reverse so they are popped in natural order.
        subs.reverse();
        for s in subs {
            stack.push((s, depth + 1));
        }
    }
    Ok(())
}

/// Build the first-level groups from a binary's selected test set.
fn initial_groups(selected: &[String], opts: &BisectOpts) -> Vec<Vec<String>> {
    if let Some(size) = opts.initial_group_size {
        return selected
            .chunks(size.max(1))
            .map(<[String]>::to_vec)
            .collect();
    }
    if let Some(k) = opts.initial_groups {
        return split_into(selected, k.max(1));
    }
    vec![selected.to_vec()]
}

/// Split `names` into `k` contiguous, roughly-equal, non-empty groups.
fn split_into(names: &[String], k: usize) -> Vec<Vec<String>> {
    let n = names.len();
    let k = k.clamp(1, n.max(1));
    let base = n / k;
    let rem = n % k;
    let mut out = Vec::with_capacity(k);
    let mut idx = 0;
    for i in 0..k {
        let size = base + usize::from(i < rem);
        if size == 0 {
            continue;
        }
        out.push(names[idx..idx + size].to_vec());
        idx += size;
    }
    out
}

// ── group execution ─────────────────────────────────────────────────────────

/// Run a group of tests in `binary`, summing elapsed across argv-length chunks
/// and short-circuiting on the first chunk that hangs.
fn run_group(
    binary: &Path,
    names: &[&str],
    include_ignored: bool,
    group_timeout: Duration,
    wd: Option<&str>,
) -> Result<GroupRun, Box<dyn std::error::Error>> {
    let mut total = Duration::ZERO;
    for chunk in chunk_names(names) {
        // Enforce a single group-level deadline across all argv-length
        // chunks: each chunk gets only the time remaining in the group's
        // budget, not a fresh `group_timeout`, otherwise a group split into
        // N chunks could run for up to N * group_timeout before a hang in a
        // later chunk is detected.
        let remaining = group_timeout.saturating_sub(total);
        if remaining.is_zero() {
            return Ok(GroupRun {
                elapsed: total,
                timed_out: true,
                failed: false,
            });
        }
        let run = run_one(binary, &chunk, include_ignored, remaining, wd)?;
        total += run.elapsed;
        // Short-circuit on a hang or a real test failure: with
        // `--test-threads 1` either one wedges/poisons the rest of the
        // group's meaning, so there is nothing more useful to learn from
        // running the remaining chunks.
        if run.timed_out || run.failed {
            return Ok(GroupRun {
                elapsed: total,
                timed_out: run.timed_out,
                failed: run.failed,
            });
        }
    }
    Ok(GroupRun {
        elapsed: total,
        timed_out: false,
        failed: false,
    })
}

/// Launch the test binary once for an exact set of test names.
fn run_one(
    binary: &Path,
    names: &[&str],
    include_ignored: bool,
    timeout: Duration,
    wd: Option<&str>,
) -> Result<GroupRun, Box<dyn std::error::Error>> {
    let mut cmd = std::process::Command::new(binary);
    // Serialize so a group's wall-clock time is the sum of its members and a
    // single hung test reliably wedges the run.
    cmd.arg("--test-threads").arg("1");
    if include_ignored {
        cmd.arg("--include-ignored");
    }
    cmd.arg("--exact");
    for n in names {
        cmd.arg(n);
    }
    let start = Instant::now();
    match invoke::run_subprocess_capture(cmd, wd, Some(timeout)) {
        // A non-zero exit means a real test failure (not a hang) — surface it
        // as `failed` rather than reporting the group as `clean`, which would
        // otherwise silently mask a failing test that finished quickly.
        Ok(out) => Ok(GroupRun {
            elapsed: start.elapsed(),
            timed_out: false,
            failed: out.exit_code != 0,
        }),
        Err(e) if e.is::<invoke::TimeoutError>() => Ok(GroupRun {
            elapsed: start.elapsed(),
            timed_out: true,
            failed: false,
        }),
        Err(e) => Err(e),
    }
}

/// Split a name list so each `--exact` launch stays under [`ARG_BYTE_BUDGET`].
fn chunk_names<'a>(names: &[&'a str]) -> Vec<Vec<&'a str>> {
    let mut chunks: Vec<Vec<&str>> = Vec::new();
    let mut cur: Vec<&str> = Vec::new();
    let mut bytes = 0usize;
    for &n in names {
        let cost = n.len() + 1;
        if !cur.is_empty() && bytes + cost > ARG_BYTE_BUDGET {
            chunks.push(std::mem::take(&mut cur));
            bytes = 0;
        }
        cur.push(n);
        bytes += cost;
    }
    if !cur.is_empty() {
        chunks.push(cur);
    }
    if chunks.is_empty() {
        chunks.push(Vec::new());
    }
    chunks
}

// ── record formatting ───────────────────────────────────────────────────────

fn round3(x: f64) -> f64 {
    (x * 1000.0).round() / 1000.0
}

fn config_record(opts: &BisectOpts) -> String {
    serde_json::json!({
        "reason": CONFIG_REASON,
        "group_timeout_secs": round3(opts.group_timeout.as_secs_f64()),
        "slow_threshold_secs": opts.slow_threshold.map(|d| round3(d.as_secs_f64())),
        "split_factor": opts.split_factor,
        "min_group_size": opts.min_group_size,
        "initial_group_size": opts.initial_group_size,
        "initial_groups": opts.initial_groups,
        "max_rounds": opts.max_rounds,
        "pattern": opts.pattern.as_ref().map(Regex::as_str),
        "include_ignored": opts.include_ignored,
    })
    .to_string()
}

fn emit_group_record(
    body: &mut String,
    sink: &mut dyn FnMut(&str),
    binary: &TestBinary,
    opts: &BisectOpts,
    report: &GroupReport<'_>,
) {
    let GroupReport {
        group,
        depth,
        run,
        outcome,
    } = *report;
    // A small sample of names keeps the record readable for large groups.
    let sample: Vec<&str> = group.iter().take(4).map(String::as_str).collect();
    let rec = serde_json::json!({
        "reason": GROUP_REASON,
        "package": binary.package_name,
        "target": binary.target_name,
        "depth": depth,
        "group_size": group.len(),
        "sample": sample,
        "elapsed_secs": round3(run.elapsed.as_secs_f64()),
        "group_timeout_secs": round3(opts.group_timeout.as_secs_f64()),
        "outcome": outcome,
    })
    .to_string();
    body.push_str(&rec);
    body.push('\n');
    // Stream a concise human line so progress is visible during long runs.
    sink(&format!(
        "bisect: {} [{}] {} test(s) -> {} ({:.3}s)",
        binary.target_name,
        depth,
        group.len(),
        outcome,
        run.elapsed.as_secs_f64(),
    ));
}

fn culprit_record(c: &Culprit, group_timeout: Duration) -> String {
    serde_json::json!({
        "reason": CULPRIT_REASON,
        "package": c.package,
        "target": c.target,
        "test": c.test,
        "classification": c.classification,
        "elapsed_secs": round3(c.elapsed.as_secs_f64()),
        "group_timeout_secs": round3(group_timeout.as_secs_f64()),
    })
    .to_string()
}

// ── output_path handling ────────────────────────────────────────────────────

/// Write the bisect body to `output_path` (when given) and return a compact
/// summary that always retains the invocation header, config, every culprit,
/// the summary, the status trailer, and a pointer to the full file. When no
/// `output_path` is given, the full body is returned unchanged.
fn write_bisect_output(
    body: String,
    output_path: Option<&str>,
    wd: Option<&str>,
) -> Result<String, Box<dyn std::error::Error>> {
    let Some(rel) = output_path else {
        return Ok(body);
    };
    let resolved = tools::resolve_output_path(rel, wd);
    let bytes = body.len();
    let lines = body.lines().count();
    std::fs::write(&resolved, &body)
        .map_err(|e| format!("failed to write output_path {}: {e}", resolved.display()))?;

    let mut summary = String::new();
    for line in body.lines() {
        let keep = line.contains(tools::INVOCATION_REASON)
            || line.contains(CONFIG_REASON)
            || line.contains(CULPRIT_REASON)
            || line.contains(SUMMARY_REASON)
            || line.contains("\"status\":");
        if keep {
            summary.push_str(line);
            summary.push('\n');
        }
    }
    let pointer = serde_json::json!({
        "reason": tools::OUTPUT_FILE_REASON,
        "path": resolved.to_string_lossy(),
        "bytes": bytes,
        "lines": lines,
    });
    // Insert the pointer just after the invocation header (first line).
    let mut out = String::new();
    let mut lines_iter = summary.lines();
    if let Some(first) = lines_iter.next() {
        out.push_str(first);
        out.push('\n');
        out.push_str(&pointer.to_string());
        out.push('\n');
        for l in lines_iter {
            out.push_str(l);
            out.push('\n');
        }
    } else {
        out.push_str(&pointer.to_string());
        out.push('\n');
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_into_balances_groups() {
        let names: Vec<String> = (0..10).map(|i| format!("t{i}")).collect();
        let g = split_into(&names, 3);
        assert_eq!(g.len(), 3);
        let sizes: Vec<usize> = g.iter().map(Vec::len).collect();
        assert_eq!(sizes, vec![4, 3, 3]);
        // Every test appears exactly once, in order.
        let flat: Vec<&String> = g.iter().flatten().collect();
        assert_eq!(flat.len(), 10);
    }

    #[test]
    fn split_into_clamps_k_to_len() {
        let names: Vec<String> = (0..3).map(|i| format!("t{i}")).collect();
        let g = split_into(&names, 10);
        assert_eq!(g.len(), 3);
        assert!(g.iter().all(|s| s.len() == 1));
    }

    #[test]
    fn initial_groups_by_size() {
        let names: Vec<String> = (0..7).map(|i| format!("t{i}")).collect();
        let opts = test_opts(|o| o.initial_group_size = Some(3));
        let g = initial_groups(&names, &opts);
        assert_eq!(g.iter().map(Vec::len).collect::<Vec<_>>(), vec![3, 3, 1]);
    }

    #[test]
    fn initial_groups_by_count() {
        let names: Vec<String> = (0..7).map(|i| format!("t{i}")).collect();
        let opts = test_opts(|o| o.initial_groups = Some(2));
        let g = initial_groups(&names, &opts);
        assert_eq!(g.iter().map(Vec::len).collect::<Vec<_>>(), vec![4, 3]);
    }

    #[test]
    fn initial_groups_default_single() {
        let names: Vec<String> = (0..5).map(|i| format!("t{i}")).collect();
        let opts = test_opts(|_| {});
        let g = initial_groups(&names, &opts);
        assert_eq!(g.len(), 1);
        assert_eq!(g[0].len(), 5);
    }

    #[test]
    fn chunk_names_splits_on_budget() {
        // Build names long enough that two won't fit in one chunk.
        let big = "x".repeat(ARG_BYTE_BUDGET);
        let refs = [big.as_str(), big.as_str(), big.as_str()];
        let chunks = chunk_names(&refs);
        assert_eq!(chunks.len(), 3);
    }

    #[test]
    fn chunk_names_single_chunk_when_small() {
        let refs = ["a", "b", "c"];
        let chunks = chunk_names(&refs);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0], vec!["a", "b", "c"]);
    }

    #[test]
    fn split_percent_overrides_factor() {
        let args = serde_json::json!({
            "bisect": { "group_timeout_secs": 10, "split_percent": 25 }
        });
        let opts = BisectOpts::from_args(&args).unwrap().unwrap();
        assert_eq!(opts.split_factor, 4);
    }

    #[test]
    fn split_percent_and_factor_conflict() {
        let args = serde_json::json!({
            "bisect": { "group_timeout_secs": 10, "split_percent": 25, "split_factor": 3 }
        });
        assert!(BisectOpts::from_args(&args).is_err());
    }

    #[test]
    fn slow_threshold_must_be_below_group_timeout() {
        let args = serde_json::json!({
            "bisect": { "group_timeout_secs": 10, "slow_threshold_secs": 10 }
        });
        assert!(BisectOpts::from_args(&args).is_err());
    }

    #[test]
    fn group_timeout_required() {
        let args = serde_json::json!({ "bisect": { "split_factor": 2 } });
        assert!(BisectOpts::from_args(&args).is_err());
    }

    #[test]
    fn absent_bisect_returns_none() {
        let args = serde_json::json!({ "working_dir": "/tmp" });
        assert!(BisectOpts::from_args(&args).unwrap().is_none());
        assert!(!is_bisect_requested(&args));
    }

    #[test]
    fn initial_group_size_and_groups_conflict() {
        let args = serde_json::json!({
            "bisect": { "group_timeout_secs": 5, "initial_group_size": 4, "initial_groups": 2 }
        });
        assert!(BisectOpts::from_args(&args).is_err());
    }

    /// Build a default-valid [`BisectOpts`] and apply `mutate` to it.
    fn test_opts(mutate: impl FnOnce(&mut BisectOpts)) -> BisectOpts {
        let mut o = BisectOpts {
            group_timeout: Duration::from_secs(10),
            slow_threshold: None,
            split_factor: DEFAULT_SPLIT_FACTOR,
            min_group_size: 1,
            initial_group_size: None,
            initial_groups: None,
            max_rounds: DEFAULT_MAX_ROUNDS,
            pattern: None,
            include_ignored: false,
        };
        mutate(&mut o);
        o
    }
}
