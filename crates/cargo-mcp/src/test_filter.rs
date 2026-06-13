// Copyright (c) Michael Grier. All rights reserved.

//! Regex-based test-case selection for `cargo_test`.
//!
//! When a caller sets the `test_filter` parameter on `cargo_test`, control is
//! handed to [`run`], which orchestrates a three-phase pipeline:
//!
//! 1. **Build** — `cargo test --no-run --message-format=json` compiles every
//!    test binary the caller's target / package selection would normally run,
//!    and we collect the `compiler-artifact` records (where `profile.test`
//!    is `true`) to learn each binary's executable path, its owning package,
//!    and its target kind.
//! 2. **Enumerate** — each test binary is invoked directly with `--list`
//!    (the libtest harness's built-in discovery flag) to produce its full
//!    test-name catalogue.
//! 3. **Execute** — the caller's regex is matched against the union of all
//!    enumerated names. Matched tests are grouped by their owning binary,
//!    and `cargo test … -- --exact <names…>` is launched once per binary
//!    that has at least one match, threading every name through libtest's
//!    `--exact` mode. The runs share up to two independent watchdogs:
//!    a hard OVERALL cap (`timeout_secs`, off by default in filter mode)
//!    that bounds the whole execution phase, and a per-test idle cap
//!    (`per_test_timeout_secs`, defaulting to the server's
//!    `cargo-mcp.test.timeoutSecs`) that arms when cargo emits
//!    `build-finished` and resets to `now + per_test_timeout` on every
//!    `test … ok|FAILED|ignored` completion line the harness publishes.
//!
//! Doctests live in a separate, rustdoc-managed harness with no
//! `compiler-artifact` executable, no `--list`, and no `--exact`. They are
//! intentionally excluded from filter selection; a future revision can add a
//! parallel doctest pipeline.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use regex::Regex;
use serde_json::Value;

use crate::invoke::{self, CargoOutput};
use crate::tools::{self, CommonOpts, SummaryKind, ToolResult};

/// Conservative per-binary upper bound on the joined length of all `--exact`
/// test-name positional arguments. Below the Windows CreateProcess command-
/// line limit of 32 768 chars with substantial headroom for the rest of the
/// cargo argv (subcommand, target selector, manifest path, profile, etc.).
const ARGV_NAME_CHUNK_BYTES: usize = 16 * 1024;

/// Default per-test budget when the caller does not supply
/// `per_test_timeout_secs` and no server default is configured: a libtest
/// case that never completes should not silently consume the whole MCP
/// request. 30s matches the VS Code extension's
/// `cargo-mcp.test.timeoutSecs` factory default.
const DEFAULT_PER_TEST_TIMEOUT: Duration = Duration::from_secs(30);

// ── parameter parsing ────────────────────────────────────────────────────────

/// Validated `test_filter` parameter block.
#[derive(Debug)]
struct FilterArgs {
    /// Caller-supplied regex pattern compiled against the full
    /// `module::path::test_name` of each enumerated libtest case. RE2-style
    /// (the `regex` crate); no backreferences.
    regex: Regex,
    /// Original pattern source, kept for echoing back in the discovery
    /// record so the caller can confirm what the server compiled.
    pattern_source: String,
    /// Whether `#[ignore]` tests participate in matching and execution. When
    /// `false`, ignored tests are excluded during discovery so a pattern can
    /// never accidentally select them. When `true`, ignored tests are
    /// enumerated and the per-binary execution invocation passes
    /// `--include-ignored` to libtest so the harness actually runs the
    /// matched ignored cases instead of skipping them.
    include_ignored: bool,
}

impl FilterArgs {
    /// Parse the `test_filter` field from a JSON args block. Returns `Ok(None)`
    /// when the field is absent or `null` (caller did not opt into filter
    /// mode), `Ok(Some(args))` when it is a well-formed object, and `Err`
    /// when the shape is wrong or the regex does not compile.
    fn from_args(args: &Value) -> Result<Option<Self>, Box<dyn std::error::Error>> {
        let Some(v) = args.get("test_filter") else {
            return Ok(None);
        };
        if v.is_null() {
            return Ok(None);
        }
        let Some(obj) = v.as_object() else {
            return Err(
                format!("test_filter must be an object with a `pattern` field, got {v}").into(),
            );
        };
        let pattern = obj
            .get("pattern")
            .and_then(|p| p.as_str())
            .ok_or_else(|| -> Box<dyn std::error::Error> {
                "test_filter.pattern must be a string".into()
            })?
            .to_owned();
        if pattern.is_empty() {
            return Err("test_filter.pattern must not be empty".into());
        }
        let regex = Regex::new(&pattern).map_err(|e| -> Box<dyn std::error::Error> {
            format!("test_filter.pattern is not a valid regex: {e}").into()
        })?;
        let include_ignored = obj
            .get("include_ignored")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        Ok(Some(Self {
            regex,
            pattern_source: pattern,
            include_ignored,
        }))
    }
}

// ── discovered test binary ──────────────────────────────────────────────────

/// A compiled test binary discovered from cargo's `compiler-artifact`
/// records during the `--no-run` build phase.
#[derive(Debug, Clone)]
struct TestBinary {
    /// Absolute path to the compiled test binary on disk. Used both for the
    /// `--list` enumeration call and (indirectly, via the cargo
    /// re-invocation) for execution.
    executable: PathBuf,
    /// The cargo target's `name`, as it appears in `compiler-artifact.target.name`.
    target_name: String,
    /// The cargo target's `kind`, used to derive the right cargo selector flag
    /// (`--lib`, `--test <name>`, `--bin <name>`, `--example <name>`,
    /// `--bench <name>`).
    target_kind: TargetKind,
    /// The owning package's name (extracted from `compiler-artifact.package_id`).
    /// Passed back to cargo as `--package <name>` so a workspace member is
    /// unambiguously selected even when the workspace contains multiple
    /// packages with similarly-named targets.
    package_name: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TargetKind {
    Lib,
    Test,
    Bin,
    Example,
    Bench,
}

impl TargetKind {
    fn from_kinds(kinds: &[&str]) -> Option<Self> {
        // A single artifact can have multiple kinds (e.g. ["lib", "rlib"] or
        // ["proc-macro"]); take the first that maps to a test-runnable
        // selector. Anything else (custom-build, …) is not a test target.
        for k in kinds {
            match *k {
                "lib" | "rlib" | "dylib" | "cdylib" | "staticlib" | "proc-macro" => {
                    return Some(Self::Lib);
                }
                "test" => return Some(Self::Test),
                "bin" => return Some(Self::Bin),
                "example" => return Some(Self::Example),
                "bench" => return Some(Self::Bench),
                _ => {}
            }
        }
        None
    }

    /// Append the cargo selector flag(s) that pick *just this target* on a
    /// subsequent `cargo test` invocation. Always paired with
    /// `--package <name>` at the call site so workspace-wide ambiguity is
    /// impossible.
    fn append_selector<'a>(&'a self, argv: &mut Vec<&'a str>, target_name: &'a str) {
        match self {
            Self::Lib => argv.push("--lib"),
            Self::Test => {
                argv.push("--test");
                argv.push(target_name);
            }
            Self::Bin => {
                argv.push("--bin");
                argv.push(target_name);
            }
            Self::Example => {
                argv.push("--example");
                argv.push(target_name);
            }
            Self::Bench => {
                argv.push("--bench");
                argv.push(target_name);
            }
        }
    }
}

/// Parse cargo's `--no-run --message-format=json` stdout into the set of test
/// binaries that were compiled.
///
/// Filters to `compiler-artifact` records with `profile.test == true`. Records
/// without an `executable` field (rare — cargo emits artifact records for
/// dependency rebuilds that have no binary), or with a kind we cannot map to
/// a cargo target selector, are skipped silently rather than failing the run:
/// the user-visible failure mode would be "build succeeded but nothing was
/// selected", which the empty-discovery path already handles gracefully.
fn parse_no_run_artifacts(stdout: &str) -> Vec<TestBinary> {
    let mut out = Vec::new();
    for line in stdout.lines() {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if v.get("reason").and_then(|r| r.as_str()) != Some("compiler-artifact") {
            continue;
        }
        let is_test = v
            .pointer("/profile/test")
            .and_then(|t| t.as_bool())
            .unwrap_or(false);
        if !is_test {
            continue;
        }
        let Some(executable) = v.get("executable").and_then(|e| e.as_str()) else {
            continue;
        };
        let target_name = v
            .pointer("/target/name")
            .and_then(|n| n.as_str())
            .unwrap_or("")
            .to_string();
        if target_name.is_empty() {
            continue;
        }
        let kinds: Vec<&str> = v
            .pointer("/target/kind")
            .and_then(|k| k.as_array())
            .map(|arr| arr.iter().filter_map(|v| v.as_str()).collect::<Vec<&str>>())
            .unwrap_or_default();
        let Some(kind) = TargetKind::from_kinds(&kinds) else {
            continue;
        };
        let package_id = v.get("package_id").and_then(|p| p.as_str()).unwrap_or("");
        let package_name = extract_package_name(package_id);
        if package_name.is_empty() {
            continue;
        }
        out.push(TestBinary {
            executable: PathBuf::from(executable),
            target_name,
            target_kind: kind,
            package_name,
        });
    }
    out
}

/// Extract the package name from a cargo `package_id` string.
///
/// Cargo 1.77+ format: `registry+https://…#serde@1.0.219` or
/// `path+file:///…/foo#0.6.2`. Legacy format: `serde 1.0.219 (registry+…)`.
/// We try both shapes and return an empty string on failure so the caller
/// can drop the artifact (rather than synthesise a wrong package name).
fn extract_package_name(package_id: &str) -> String {
    // New format: …#name@version or …#version (when name is the URL's last segment)
    if let Some(hash) = package_id.rfind('#') {
        let tail = &package_id[hash + 1..];
        if let Some(at) = tail.find('@') {
            return tail[..at].to_owned();
        }
        // …#version form: name is the basename of the URL path before the #.
        let url = &package_id[..hash];
        if let Some(slash) = url.rfind('/') {
            let last = &url[slash + 1..];
            if !last.is_empty() {
                return last.to_owned();
            }
        }
    }
    // Legacy format: "name version (source)"
    if let Some(sp) = package_id.find(' ') {
        return package_id[..sp].to_owned();
    }
    String::new()
}

// ── per-binary enumeration ──────────────────────────────────────────────────

/// Hard wall-clock cap on a single `--list` enumeration of a test binary.
///
/// A healthy libtest binary lists its tests in milliseconds even for large
/// suites. We pick a generous cap (well above any realistic enumeration)
/// purely as a safety net: if a binary hangs in global initialization, we
/// surface a `TimeoutError` for that binary (which the orchestrator records
/// in `enumeration_errors` and treats as an overall error) rather than
/// wedging the whole `cargo_test` request indefinitely.
const ENUMERATION_TIMEOUT: Duration = Duration::from_secs(60);

/// Invoke a compiled test binary with `--list` and parse its name catalogue.
///
/// libtest's `--list` output is a sequence of `<name>: <kind>` lines, plus a
/// trailing summary line we ignore. We keep only entries whose kind is
/// `test` (excluding `bench` records that integration crates with
/// `--benches` may also produce). When `include_ignored` is true, we also
/// run a second `--list --ignored` pass to discover the ignored cases —
/// `--list` alone hides them and there is no stable text marker on each
/// line that says "ignored".
///
/// `working_dir` is forwarded to the child so the binary runs with the same
/// cwd the caller will use for execution (phase 3) and that cargo used for
/// the build (phase 1). Without this, enumeration would run with whatever
/// cwd the MCP server process happens to hold, which can produce different
/// initialization behaviour than the execution launches.
fn enumerate_tests(
    binary: &Path,
    include_ignored: bool,
    working_dir: Option<&str>,
) -> Result<Vec<String>, Box<dyn std::error::Error>> {
    let mut tests = list_tests(binary, false, working_dir)?;
    if include_ignored {
        let ignored = list_tests(binary, true, working_dir)?;
        tests.extend(ignored);
    }
    // Dedupe defensively in case a future libtest revision starts including
    // ignored tests in the default `--list` output.
    tests.sort();
    tests.dedup();
    Ok(tests)
}

fn list_tests(
    binary: &Path,
    ignored_only: bool,
    working_dir: Option<&str>,
) -> Result<Vec<String>, Box<dyn std::error::Error>> {
    let mut cmd = Command::new(binary);
    cmd.arg("--list");
    if ignored_only {
        cmd.arg("--ignored");
    }
    // Run under the cancel-token + wall-clock supervisor so a hung test
    // binary (e.g. stuck in global initialization) is killed instead of
    // wedging the whole `cargo_test` request.
    let output = invoke::run_subprocess_capture(cmd, working_dir, Some(ENUMERATION_TIMEOUT))
        .map_err(|e| -> Box<dyn std::error::Error> {
            format!(
                "failed to enumerate tests via --list on {} ({e})",
                binary.display()
            )
            .into()
        })?;
    if output.exit_code != 0 {
        return Err(format!(
            "test binary {} exited with code {} during --list enumeration: {}",
            binary.display(),
            output.exit_code,
            output.stderr.trim()
        )
        .into());
    }
    let mut tests = Vec::new();
    for line in output.stdout.lines() {
        let line = line.trim_end();
        if line.is_empty() {
            continue;
        }
        // libtest summary lines look like "N tests, M benchmarks" — they have
        // no `: ` separator and are emitted exactly once at the tail of the
        // output, so a simple "no colon-space → skip" rule rejects them.
        let Some((name, kind)) = line.rsplit_once(": ") else {
            continue;
        };
        if kind.trim() == "test" {
            tests.push(name.trim().to_owned());
        }
    }
    Ok(tests)
}

// ── matching ────────────────────────────────────────────────────────────────

/// A test binary paired with its full enumerated name catalogue and the
/// subset that matched the caller's regex. Carries through to the execution
/// phase so per-binary discovery records can report both counts.
struct DiscoveredBinary {
    binary: TestBinary,
    /// Every test the binary advertises via `--list`, in libtest order.
    /// Retained even when no match was found so the discovery record can
    /// report `tests_enumerated` accurately.
    all_tests: Vec<String>,
    /// The subset of `all_tests` whose name matched the caller's regex.
    matched: Vec<String>,
}

fn match_tests(binaries: Vec<(TestBinary, Vec<String>)>, regex: &Regex) -> Vec<DiscoveredBinary> {
    binaries
        .into_iter()
        .map(|(binary, all_tests)| {
            let matched: Vec<String> = all_tests
                .iter()
                .filter(|n| regex.is_match(n))
                .cloned()
                .collect();
            DiscoveredBinary {
                binary,
                all_tests,
                matched,
            }
        })
        .collect()
}

// ── per-binary execution ────────────────────────────────────────────────────

/// True for the libtest harness lines that mark either the start of a test
/// run (`running N tests`) or the completion of a single case (`test foo …
/// ok|FAILED|ignored|bench:`). Used as the per-test watchdog reset predicate
/// in [`invoke::run_cargo_streaming_with_watchdog`]; matching against the
/// raw cargo stdout line is correct because cargo passes the harness's
/// stdout through unchanged (libtest text is not JSON, so the test-output
/// filter only wraps it later when assembling the response body).
fn is_test_completion_or_start(line: &str) -> bool {
    let l = line.trim_start();
    if let Some(rest) = l.strip_prefix("running ") {
        // "running N tests" — execution-phase start marker.
        return rest.split_whitespace().next().is_some();
    }
    if let Some(rest) = l.strip_prefix("test ") {
        // "test some::path ... ok|FAILED|ignored"
        return rest.contains(" ... ");
    }
    false
}

/// Chunk a binary's matched test names so each launch's `--exact` positional
/// list stays under the OS command-line limit.
///
/// Returns at least one chunk even when the input is empty (callers filter
/// empty chunks out separately if they want the "no matches → no launch"
/// semantics; this helper keeps the chunking logic itself total).
fn chunk_test_names(names: &[String]) -> Vec<Vec<&str>> {
    if names.is_empty() {
        return vec![Vec::new()];
    }
    let mut chunks: Vec<Vec<&str>> = Vec::new();
    let mut current: Vec<&str> = Vec::new();
    let mut current_bytes: usize = 0;
    for name in names {
        // +1 for the separating space the shell sees. Conservative but
        // cheap; the actual subprocess argv concatenation is platform-
        // dependent.
        let cost = name.len() + 1;
        if !current.is_empty() && current_bytes + cost > ARGV_NAME_CHUNK_BYTES {
            chunks.push(std::mem::take(&mut current));
            current_bytes = 0;
        }
        current.push(name);
        current_bytes += cost;
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}

/// Per-binary outcome record carried through to the assembled response body.
struct BinaryRunOutcome {
    /// Output of each launch for this binary (one entry per argv chunk),
    /// already formatted by [`tools::format_test_output`] — i.e. a strict
    /// NDJSON block beginning with `x-cargo-mcp-invocation`.
    formatted: Vec<String>,
    /// Aggregate exit code across this binary's chunks. Non-zero if *any*
    /// chunk failed (consistent with how `cargo test --no-fail-fast` would
    /// report the run as failed even if only one test inside it failed).
    exit_code: i32,
    /// Number of chunks that actually ran (== 1 when matches fit a single
    /// launch; > 1 when chunking kicked in).
    launches: usize,
}

/// Build the cargo argv used to *execute* a specific binary's matched tests.
/// The argv mirrors the caller's original target / package / manifest /
/// feature selection so the same toolchain and profile resolve as during
/// the discovery build — but narrows the target down to exactly one binary
/// and appends the matched names under libtest's `--exact` mode.
fn build_per_binary_argv<'a>(
    args: &'a Value,
    common: &'a CommonOpts,
    discovered: &'a DiscoveredBinary,
    names_chunk: &'a [&str],
    include_ignored: bool,
) -> Vec<&'a str> {
    let mut argv: Vec<&'a str> = vec!["test", "--message-format=json"];
    // Always pass --package <name> to disambiguate workspace members.
    argv.push("--package");
    argv.push(&discovered.binary.package_name);
    // Replace the caller's target-selection set with the single selector that
    // picks just this binary (so a single `cargo_test` request that would
    // normally compile + run dozens of binaries collapses to one process per
    // binary with matches).
    discovered
        .binary
        .target_kind
        .append_selector(&mut argv, &discovered.binary.target_name);
    // Inherit feature, compilation, and manifest flags from the caller so
    // the same profile/target/features take effect during execution. Target
    // selection is *not* inherited — we just overrode it with the per-
    // binary selector above.
    tools::push_feature_flags(&mut argv, args, common);
    tools::push_compilation_options(&mut argv, args, common, false);
    tools::push_manifest_options(&mut argv, args, common, true);
    // Always run with --no-fail-fast: a failure in the first matched case
    // should not prevent the remaining matches from executing. The caller's
    // own `no_fail_fast` is intentionally ignored here because the whole
    // point of `test_filter` is to run "exactly this set".
    argv.push("--no-fail-fast");
    // Harness arguments after the `--` separator.
    argv.push("--");
    argv.push("--exact");
    if include_ignored {
        argv.push("--include-ignored");
    }
    for name in names_chunk {
        argv.push(name);
    }
    argv
}

/// Execute one matched binary, splitting into multiple cargo launches if the
/// argv would otherwise exceed [`ARGV_NAME_CHUNK_BYTES`]. Each launch is
/// supervised by up to two independent watchdogs:
/// - `phase3_deadline`: shared OVERALL wall-clock cap for the entire
///   execution phase (phase 3) across every per-binary launch. Each launch
///   receives the *remaining* budget — the slice between `Instant::now()`
///   and `phase3_deadline` — as its per-launch overall watchdog. If the
///   deadline has already elapsed by the time we reach a launch, the launch
///   is failed synthetically without spawning cargo.
/// - `per_test_timeout`: per-test idle cap (arms on `build-finished`, resets
///   on every libtest boundary line). Independent across launches.
///
/// Whichever fires first kills the launch; the orchestrator then moves on to
/// the next chunk / binary so one hung test does not block the rest of the
/// matched run from completing.
#[allow(clippy::too_many_arguments)] // each input is independent; bundling them would obscure intent
fn run_one_binary(
    args: &Value,
    common: &CommonOpts,
    discovered: &DiscoveredBinary,
    include_ignored: bool,
    wd: Option<&str>,
    phase3_deadline: Option<Instant>,
    per_test_timeout: Option<Duration>,
    on_progress: &mut dyn FnMut(&str),
) -> BinaryRunOutcome {
    let chunks = chunk_test_names(&discovered.matched);
    let mut formatted = Vec::with_capacity(chunks.len());
    let mut agg_exit_code = 0i32;
    let mut launches = 0usize;
    for chunk in chunks {
        if chunk.is_empty() {
            continue;
        }
        launches += 1;
        let argv = build_per_binary_argv(args, common, discovered, &chunk, include_ignored);
        // Compute the slice of the shared overall budget remaining for this
        // launch. If the deadline has already elapsed, fail this launch
        // synthetically rather than spawning cargo and racing the watchdog.
        let remaining_overall =
            phase3_deadline.map(|d| d.saturating_duration_since(Instant::now()));
        let result = if let Some(Duration::ZERO) = remaining_overall {
            Err(Box::<dyn std::error::Error>::from(
                "overall test_filter timeout exceeded before launch",
            ))
        } else {
            invoke::run_cargo_streaming_with_watchdog(
                &argv,
                wd,
                remaining_overall,
                per_test_timeout,
                Some(&tools::is_build_finished_line),
                Some(&is_test_completion_or_start),
                on_progress,
            )
        };
        let body = match result {
            Ok(out) => {
                if out.exit_code != 0 && agg_exit_code == 0 {
                    agg_exit_code = out.exit_code;
                }
                tools::format_test_output(&out, &argv, wd)
            }
            Err(e) => {
                // Treat any error from the run (timeout, cancellation, spawn
                // failure) as a non-zero outcome and synthesise a minimal
                // NDJSON body so the caller sees the cause inline.
                if agg_exit_code == 0 {
                    agg_exit_code = -1;
                }
                let synthetic = CargoOutput {
                    stdout: String::new(),
                    stderr: format!("cargo-mcp test_filter: per-binary run failed: {e}"),
                    exit_code: -1,
                };
                tools::format_test_output(&synthetic, &argv, wd)
            }
        };
        formatted.push(body);
    }
    BinaryRunOutcome {
        formatted,
        exit_code: agg_exit_code,
        launches,
    }
}

// ── orchestration ───────────────────────────────────────────────────────────

/// Cheap predicate the dispatcher in [`crate::tools::call_test`] uses to decide
/// whether to route a `cargo_test` call through the filter pipeline or fall
/// through to the original unfiltered flow.
///
/// Returns `true` whenever the caller supplied a non-null `test_filter`
/// field; the actual shape validation (and regex compilation) happens later
/// inside [`run`] via [`FilterArgs::from_args`]. Keeping this check shape-
/// agnostic means a malformed `test_filter` still routes through the
/// filter pipeline and surfaces its error there instead of being silently
/// swallowed into the unfiltered path.
pub fn is_filter_requested(args: &Value) -> bool {
    args.get("test_filter")
        .map(|v| !v.is_null())
        .unwrap_or(false)
}

/// Top-level orchestrator for `cargo_test` runs with `test_filter` set.
///
/// Returns `Ok(None)` when the caller did not request filter mode (so
/// `call_test` should fall through to its normal flow). Returns
/// `Ok(Some(result))` when filter mode ran end-to-end — even when discovery
/// matched zero tests, since "ran successfully and matched nothing" is a
/// distinct outcome from "did not run filter mode at all" and the caller
/// should not double-execute by then running the unfiltered path.
pub fn run(
    args: &Value,
    on_progress: Option<&mut dyn FnMut(&str)>,
) -> Result<Option<ToolResult>, Box<dyn std::error::Error>> {
    let Some(filter) = FilterArgs::from_args(args)? else {
        return Ok(None);
    };
    let wd = tools::opt_str(args, "working_dir");
    let output_path = tools::opt_str(args, "output_path");
    if let Some(p) = output_path {
        tools::validate_relative_output_path(p, wd)?;
    }
    let toolchain = tools::toolchain_arg(args);
    let common = CommonOpts::from_args(args);
    // Two independent budgets:
    //   overall_timeout  — hard wall-clock cap on the whole execution phase
    //                       across all per-binary launches. Explicit-only in
    //                       filter mode: omitting it means "no overall cap",
    //                       preserving the historical behaviour where a long
    //                       matched run was allowed to complete.
    //   per_test_timeout — per-test idle watchdog (resets on each libtest
    //                       boundary line). Falls back to the server default
    //                       and then to DEFAULT_PER_TEST_TIMEOUT so the
    //                       hung-test guarantee is on by default.
    let overall_timeout = tools::opt_timeout_explicit(args)?.unwrap_or(None);
    let per_test_timeout = match tools::opt_per_test_timeout_explicit(args)? {
        None => tools::default_test_timeout().or(Some(DEFAULT_PER_TEST_TIMEOUT)),
        Some(explicit) => explicit,
    };

    // Adapter so we can pass an Option<&mut> through to inner callers as a
    // single mutable closure. on_progress is consumed by the no-run phase
    // and then re-used by each per-binary phase; we keep ownership of the
    // outer reference and route every notification through this closure.
    let mut sink: Box<dyn FnMut(&str)> = match on_progress {
        Some(cb) => Box::new(move |line: &str| cb(line)),
        None => Box::new(|_line: &str| {}),
    };

    // ── phase 1: --no-run build ─────────────────────────────────────────
    let mut no_run_argv: Vec<&str> = vec!["test", "--no-run", "--message-format=json"];
    tools::push_package_selection(&mut no_run_argv, args, &common);
    tools::push_target_selection(&mut no_run_argv, args, &common);
    tools::push_feature_flags(&mut no_run_argv, args, &common);
    tools::push_compilation_options(&mut no_run_argv, args, &common, false);
    tools::push_manifest_options(&mut no_run_argv, args, &common, true);
    if let Some(ref t) = toolchain {
        no_run_argv.insert(0, t);
    }
    // Run the build phase via the standard streaming-with-timeout path so it
    // retains the existing retry-on-transient-busy behaviour (relevant on
    // Windows, where `cargo test --no-run` can intermittently hit
    // `os error 32/5` while writing artifacts). The watchdog runner is
    // reserved for execution, where retries are intentionally unsafe.
    let no_run_out = invoke::run_cargo_streaming_with_timeout(
        &no_run_argv,
        wd,
        None, // build phase is never timed by either watchdog
        None,
        sink.as_mut(),
    )?;
    let no_run_failed = no_run_out.exit_code != 0;
    let no_run_body = tools::format_test_output(&no_run_out, &no_run_argv, wd);
    if no_run_failed {
        // Compilation failed: surface the no-run body as the response and
        // bail out before discovery — there is nothing to enumerate.
        let body = no_run_body;
        let text =
            tools::write_output_path_and_summarize(body, output_path, wd, SummaryKind::Test)?;
        return Ok(Some(ToolResult::Text {
            text,
            is_error: true,
        }));
    }
    let binaries = parse_no_run_artifacts(&no_run_out.stdout);

    // ── phase 2: --list enumeration ─────────────────────────────────────
    let mut enumerated: Vec<(TestBinary, Vec<String>)> = Vec::with_capacity(binaries.len());
    let mut enumeration_errors: Vec<String> = Vec::new();
    for binary in binaries {
        match enumerate_tests(&binary.executable, filter.include_ignored, wd) {
            Ok(tests) => enumerated.push((binary, tests)),
            Err(e) => {
                enumeration_errors.push(format!("{}: {e}", binary.executable.display()));
            }
        }
    }
    let discovered = match_tests(enumerated, &filter.regex);
    let total_enumerated: usize = discovered.iter().map(|d| d.all_tests.len()).sum();
    let total_matched: usize = discovered.iter().map(|d| d.matched.len()).sum();

    // ── phase 3: per-binary execution ────────────────────────────────────
    // Capture the start of phase 3 and convert `overall_timeout` into a
    // shared absolute deadline, so the OVERALL cap covers wall-clock across
    // *all* per-binary launches rather than restarting per launch. Each
    // call into `run_one_binary` receives the remaining slice of this
    // budget; once exhausted, subsequent launches fail synthetically.
    let phase3_start = Instant::now();
    let phase3_deadline = overall_timeout.map(|t| phase3_start + t);
    let mut binary_bodies: Vec<String> = Vec::new();
    let mut total_launches: usize = 0;
    let mut overall_exit_code: i32 = 0;
    // Treat any phase-2 enumeration failures as an overall error: the
    // discovery record carries the per-binary errors, and the caller would
    // otherwise see `status: success` even though the regex was matched
    // against fewer binaries than discovery enumerated.
    if !enumeration_errors.is_empty() {
        overall_exit_code = -1;
    }
    for d in &discovered {
        if d.matched.is_empty() {
            continue;
        }
        let outcome = run_one_binary(
            args,
            &common,
            d,
            filter.include_ignored,
            wd,
            phase3_deadline,
            per_test_timeout,
            sink.as_mut(),
        );
        total_launches += outcome.launches;
        if outcome.exit_code != 0 && overall_exit_code == 0 {
            overall_exit_code = outcome.exit_code;
        }
        binary_bodies.extend(outcome.formatted);
    }

    // ── response assembly ───────────────────────────────────────────────
    let mut body = String::new();
    // The no-run body is the first chunk so the response opens with an
    // `x-cargo-mcp-invocation` header (cargo --no-run --message-format=json),
    // exactly as the caller would have seen for a plain build phase.
    body.push_str(&no_run_body);
    if !body.ends_with('\n') {
        body.push('\n');
    }
    let discovery = build_discovery_record(
        &filter,
        &discovered,
        total_enumerated,
        total_matched,
        total_launches,
        &enumeration_errors,
    );
    body.push_str(&discovery);
    body.push('\n');
    for binary_body in binary_bodies {
        body.push_str(&binary_body);
        if !body.ends_with('\n') {
            body.push('\n');
        }
    }
    // Final rollup trailer. Distinct from each per-binary trailer so a
    // consumer can read the overall outcome from a single line.
    let rollup = serde_json::to_string(&serde_json::json!({
        "reason": "x-cargo-mcp-test-filter-summary",
        "pattern": &filter.pattern_source,
        "binaries_discovered": discovered.len(),
        "binaries_with_matches": discovered.iter().filter(|d| !d.matched.is_empty()).count(),
        "tests_enumerated": total_enumerated,
        "tests_matched": total_matched,
        "launches": total_launches,
        "status": if overall_exit_code == 0 { "success" } else { "error" },
        "exit_code": overall_exit_code,
    }))
    .unwrap_or_else(|_| "{}".into());
    body.push_str(&rollup);
    body.push('\n');

    let is_error = overall_exit_code != 0;
    let text = tools::write_output_path_and_summarize(body, output_path, wd, SummaryKind::Test)?;
    Ok(Some(ToolResult::Text { text, is_error }))
}

/// Build the `x-cargo-mcp-test-filter-discovery` NDJSON record summarising
/// what the regex matched, per binary, so the caller has a single line
/// that explains exactly which tests are about to run.
fn build_discovery_record(
    filter: &FilterArgs,
    discovered: &[DiscoveredBinary],
    total_enumerated: usize,
    total_matched: usize,
    total_launches: usize,
    enumeration_errors: &[String],
) -> String {
    let binaries: Vec<Value> = discovered
        .iter()
        .map(|d| {
            serde_json::json!({
                "package": d.binary.package_name,
                "target": d.binary.target_name,
                "kind": match d.binary.target_kind {
                    TargetKind::Lib => "lib",
                    TargetKind::Test => "test",
                    TargetKind::Bin => "bin",
                    TargetKind::Example => "example",
                    TargetKind::Bench => "bench",
                },
                "executable": d.binary.executable.display().to_string(),
                "tests_enumerated": d.all_tests.len(),
                "tests_matched": d.matched.len(),
                "matched": &d.matched,
            })
        })
        .collect();
    serde_json::to_string(&serde_json::json!({
        "reason": "x-cargo-mcp-test-filter-discovery",
        "pattern": &filter.pattern_source,
        "include_ignored": filter.include_ignored,
        "tests_enumerated": total_enumerated,
        "tests_matched": total_matched,
        "launches_planned": total_launches,
        "binaries": binaries,
        "enumeration_errors": enumeration_errors,
    }))
    .unwrap_or_else(|_| "{}".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_package_name_handles_new_format_with_name_at_version() {
        assert_eq!(
            extract_package_name(
                "registry+https://github.com/rust-lang/crates.io-index#serde@1.0.219"
            ),
            "serde",
        );
    }

    #[test]
    fn extract_package_name_handles_new_format_path_dep_with_name_at_version() {
        assert_eq!(
            extract_package_name(
                "path+file:///c/github/cargo-mcp-rs/crates/cargo-mcp#cargo-mcp@0.6.2"
            ),
            "cargo-mcp",
        );
    }

    #[test]
    fn extract_package_name_handles_new_format_version_only() {
        // …#version form (name implied by the URL path's last segment).
        assert_eq!(
            extract_package_name("path+file:///c/github/cargo-mcp-rs/crates/cargo-mcp#0.6.2"),
            "cargo-mcp",
        );
    }

    #[test]
    fn extract_package_name_handles_legacy_format() {
        assert_eq!(
            extract_package_name(
                "serde 1.0.219 (registry+https://github.com/rust-lang/crates.io-index)"
            ),
            "serde",
        );
    }

    #[test]
    fn target_kind_from_kinds_prefers_test_runnable_classifier() {
        assert_eq!(
            TargetKind::from_kinds(&["lib", "rlib"]),
            Some(TargetKind::Lib)
        );
        assert_eq!(
            TargetKind::from_kinds(&["proc-macro"]),
            Some(TargetKind::Lib)
        );
        assert_eq!(TargetKind::from_kinds(&["test"]), Some(TargetKind::Test));
        assert_eq!(TargetKind::from_kinds(&["bin"]), Some(TargetKind::Bin));
        assert_eq!(TargetKind::from_kinds(&["bench"]), Some(TargetKind::Bench));
        assert_eq!(
            TargetKind::from_kinds(&["example"]),
            Some(TargetKind::Example)
        );
        assert_eq!(TargetKind::from_kinds(&["custom-build"]), None);
    }

    #[test]
    fn parse_no_run_artifacts_keeps_only_test_profile_with_executable() {
        let stdout = [
            r#"{"reason":"compiler-artifact","package_id":"path+file:///x#foo@0.1.0","target":{"name":"foo","kind":["lib"]},"profile":{"test":true},"executable":"/tmp/foo-abc"}"#,
            // profile.test false → skipped
            r#"{"reason":"compiler-artifact","package_id":"path+file:///x#foo@0.1.0","target":{"name":"foo","kind":["lib"]},"profile":{"test":false},"executable":"/tmp/foo-rel"}"#,
            // missing executable → skipped
            r#"{"reason":"compiler-artifact","package_id":"path+file:///x#foo@0.1.0","target":{"name":"foo","kind":["lib"]},"profile":{"test":true}}"#,
            // integration test
            r#"{"reason":"compiler-artifact","package_id":"path+file:///x#foo@0.1.0","target":{"name":"it","kind":["test"]},"profile":{"test":true},"executable":"/tmp/it-abc"}"#,
            // unrelated record
            r#"{"reason":"build-finished","success":true}"#,
        ]
        .join("\n");
        let bins = parse_no_run_artifacts(&stdout);
        assert_eq!(bins.len(), 2);
        assert_eq!(bins[0].target_name, "foo");
        assert_eq!(bins[0].target_kind, TargetKind::Lib);
        assert_eq!(bins[0].executable, PathBuf::from("/tmp/foo-abc"));
        assert_eq!(bins[0].package_name, "foo");
        assert_eq!(bins[1].target_name, "it");
        assert_eq!(bins[1].target_kind, TargetKind::Test);
    }

    #[test]
    fn is_test_completion_or_start_matches_expected_lines() {
        assert!(is_test_completion_or_start("running 5 tests"));
        assert!(is_test_completion_or_start("test foo::bar ... ok"));
        assert!(is_test_completion_or_start("test foo::bar ... FAILED"));
        assert!(is_test_completion_or_start("test foo::bar ... ignored"));
        assert!(is_test_completion_or_start(
            "test foo::bar ... ignored, reason: needs network"
        ));
        assert!(!is_test_completion_or_start(
            "test result: ok. 5 passed; 0 failed"
        ));
        assert!(!is_test_completion_or_start("hello world"));
        assert!(!is_test_completion_or_start(""));
    }

    #[test]
    fn chunk_test_names_packs_until_budget_then_splits() {
        // Two short names → single chunk.
        let names = vec!["a::b".to_owned(), "c::d".to_owned()];
        let chunks = chunk_test_names(&names);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0], vec!["a::b", "c::d"]);

        // Synthesise enough names to force a split.
        let long = "x".repeat(1024);
        let many: Vec<String> = (0..40).map(|i| format!("{long}_{i}")).collect();
        let chunks = chunk_test_names(&many);
        assert!(
            chunks.len() > 1,
            "expected at least one split, got {}",
            chunks.len()
        );
        let total: usize = chunks.iter().map(|c| c.len()).sum();
        assert_eq!(total, many.len());
    }

    #[test]
    fn filter_args_rejects_invalid_regex() {
        let v = serde_json::json!({"test_filter": {"pattern": "(unclosed"}});
        let err = FilterArgs::from_args(&v).unwrap_err();
        assert!(err.to_string().contains("not a valid regex"), "got: {err}",);
    }

    #[test]
    fn filter_args_requires_object_shape() {
        let v = serde_json::json!({"test_filter": "just_a_string"});
        let err = FilterArgs::from_args(&v).unwrap_err();
        assert!(err.to_string().contains("must be an object"), "got: {err}");
    }

    #[test]
    fn filter_args_returns_none_when_absent_or_null() {
        let v_absent = serde_json::json!({});
        assert!(FilterArgs::from_args(&v_absent).unwrap().is_none());
        let v_null = serde_json::json!({"test_filter": null});
        assert!(FilterArgs::from_args(&v_null).unwrap().is_none());
    }

    #[test]
    fn filter_args_parses_pattern_and_include_ignored() {
        let v = serde_json::json!({
            "test_filter": {"pattern": "^foo::", "include_ignored": true},
        });
        let parsed = FilterArgs::from_args(&v).unwrap().unwrap();
        assert!(parsed.regex.is_match("foo::bar"));
        assert!(!parsed.regex.is_match("bar::foo"));
        assert!(parsed.include_ignored);
        assert_eq!(parsed.pattern_source, "^foo::");
    }
}
