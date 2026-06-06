// Copyright (c) Michael Grier. All rights reserved.

//! Tool definitions and dispatch for the `cargo-mcp` MCP server.
//!
//! Each tool invokes `cargo` as a subprocess via [`crate::invoke::run_cargo`]
//! (or the streaming / file-piped variants). The server is a thin dispatch
//! layer — all build logic lives in Cargo.
//!
//! ## Tool set
//!
//! - `cargo_metadata`   — project structure and dependency info (JSON)
//! - `cargo_check`      — fast error checking without producing binaries
//! - `cargo_build`      — compile the project
//! - `cargo_test`       — run tests with optional filters
//! - `cargo_clippy`     — run lint checks
//! - `cargo_fmt_check`  — check formatting without modifying files
//! - `cargo_fmt`        — format source code
//! - `cargo_tree`       — display dependency tree
//! - `cargo_doc`        — build documentation
//! - `cargo_clean`      — remove build artefacts
//! - `cargo_update`     — update Cargo.lock
//! - `cargo_fix`        — auto-apply compiler fixes
//! - `cargo_add`        — add a dependency
//! - `cargo_remove`     — remove a dependency
//! - `cargo_publish`    — publish to crates.io
//! - `cargo_setup`      — emit canonical Copilot-instructions text
//! - `cargo_diagnostic` — report the resolved cargo/rustc binary, the active
//!   `rust-toolchain.toml` (if any), and the relevant environment
//!
//! ## Output shape
//!
//! Tool results begin with a one-line **JSON invocation header** produced by
//! [`invocation_header`], shaped to look like another cargo NDJSON record:
//!
//! ```json
//! {"reason":"x-cargo-mcp-invocation","argv":["build","--message-format=json"],"cwd":"C:\\path\\to\\workspace"}
//! ```
//!
//! The `reason` value uses an `x-` prefix so it can never collide with a
//! cargo-defined record type (`compiler-message`, `build-finished`,
//! `compiler-artifact`, etc.). The header makes the *effective* command
//! (including flags the dispatch layer added implicitly, such as
//! `--message-format=json`) visible in the MCP client's tool-result panel
//! even when the original `arguments` JSON is sparse.
//!
//! For **JSON-mode tools** (`check`, `build`, `clippy`, `doc`,
//! `metadata`) the *entire* response is a clean NDJSON stream — the
//! invocation header followed by one JSON object per line, so consumers
//! can parse the whole response with a single line-by-line JSON parser.
//!
//! **`cargo_test`** is a special case: the test execution phase emits
//! plain-text libtest output (harness lines, captured `println!` replays)
//! that is not valid JSON. Each such line is wrapped in an
//! `{"reason":"x-cargo-mcp-test-output","text":"..."}` NDJSON record so the
//! stream remains strictly parseable line-by-line. `eprintln!` from test
//! code bypasses libtest capture and is always included (even on success)
//! as `{"reason":"x-cargo-mcp-stderr","text":"..."}`.
//!
//! For **text-mode tools** (`fmt`, `tree`, `clean`, `update`, `fix`,
//! `add`, `remove`, `publish`) only the first line (the invocation
//! header) is JSON; the body that follows is the cargo child's combined
//! stdout/stderr and is not guaranteed to be JSON.

use serde_json::Value;

use crate::{
    invoke::{self, CargoOutput},
    suggest::{self, Suggestion},
};

use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU64, Ordering},
};

/// Default wall-clock timeout for `cargo_test` when the caller does not
/// supply an explicit `timeout_secs`. `0` means no default (wait forever).
/// Set once at startup via [`set_default_test_timeout`].
static DEFAULT_TEST_TIMEOUT_SECS: AtomicU64 = AtomicU64::new(0);

/// Configure the per-test-run default timeout. Called once from `main` after
/// CLI parse. `None` (or `Some(0)`) means no default timeout.
pub fn set_default_test_timeout(secs: Option<u64>) {
    DEFAULT_TEST_TIMEOUT_SECS.store(secs.unwrap_or(0), Ordering::Relaxed);
}

/// Returns the configured default test timeout, or `None` if no default is set.
fn default_test_timeout() -> Option<std::time::Duration> {
    let secs = DEFAULT_TEST_TIMEOUT_SECS.load(Ordering::Relaxed);
    if secs > 0 {
        Some(std::time::Duration::from_secs(secs))
    } else {
        None
    }
}

/// The section appended to (or used to create) `.github/copilot-instructions.md`
/// during `cargo_setup`. Kept here so the tool description and the written
/// content stay in sync.
const CARGO_MCP_INSTRUCTIONS: &str = "## Cargo commands \u{2014} use MCP tools, never the terminal\n\n\
When working in this Rust/Cargo project, ALWAYS use the `cargo_*` MCP tools\n\
instead of running `cargo` commands in a PowerShell or bash terminal.\n\
This applies even inside a larger workflow \u{2014} do not switch to the terminal\n\
for cargo just because a previous step used the terminal.\n\n\
| MCP tool | Replaces |\n\
|---|---|\n\
| `cargo_metadata` | `cargo metadata` |\n\
| `cargo_check` | `cargo check` |\n\
| `cargo_build` | `cargo build` |\n\
| `cargo_test` | `cargo test` |\n\
| `cargo_clippy` | `cargo clippy` |\n\
| `cargo_fmt_check` | `cargo fmt --check` |\n\
| `cargo_fmt` | `cargo fmt` |\n\
| `cargo_tree` | `cargo tree` |\n\
| `cargo_doc` | `cargo doc` |\n\
| `cargo_clean` | `cargo clean` |\n\
| `cargo_update` | `cargo update` |\n\
| `cargo_fix` | `cargo fix` |\n\
| `cargo_add` | `cargo add` |\n\
| `cargo_remove` | `cargo remove` |\n\
| `cargo_publish` | `cargo publish` |\n\
| `cargo_setup` | *(no terminal equivalent)* |\n\
| `cargo_diagnostic` | *(no terminal equivalent)* |\n\n\
### cargo_test — timeout\n\n\
When launched by the VS Code extension, `cargo_test` applies a server-side\n\
default timeout from the `cargo-mcp.test.timeoutSecs` setting (**30 seconds**\n\
by default). Without the extension (or with `cargo-mcp.test.timeoutSecs: 0`),\n\
the server has no default timeout. The budget covers only test **execution** —\n\
the clock starts when compilation and linking finish (cargo's `build-finished`\n\
record), so a slow build never trips the timeout.\n\
- Omit `timeout_secs` to let the server default apply.\n\
- Pass `timeout_secs: N` to use a specific budget for this run.\n\
- Pass `timeout_secs: 0` to disable the timeout for this run, regardless of\n\
  the server default.\n\n\
### Reading cargo_test output\n\n\
`cargo_test` returns a strict NDJSON stream. Parse it line-by-line; every\n\
non-blank line is a JSON object. The `reason` field identifies the record type:\n\n\
| `reason` | Content | Key fields |\n\
|---|---|---|\n\
| `x-cargo-mcp-invocation` | Effective command and working dir (first line) | `argv`, `cwd` |\n\
| `compiler-message` | Compilation error or warning | `message` (rustc diagnostic) |\n\
| `build-finished` | Build phase outcome | `success` (bool) |\n\
| `x-cargo-mcp-test-output` | One line of libtest harness output or captured `println!` | `text` |\n\
| `x-cargo-mcp-stderr` | `eprintln!` and other test stderr (when non-empty) | `text` |\n\
| *(last line)* | Exit status | `status` (`\"success\"` or `\"error\"`), `exit_code` (on error) |\n\n\
`println!` inside tests is captured by libtest and replayed as\n\
`x-cargo-mcp-test-output` lines only when the test fails (standard\n\
libtest behaviour). `eprintln!` bypasses libtest capture and always\n\
appears in `x-cargo-mcp-stderr`.\n";

/// Description for the `working_dir` parameter, shared across every tool.
///
/// The phrasing is deliberately blunt about the failure mode: the MCP server's
/// own working directory is almost never the user's workspace (on Windows it
/// is typically the rustup toolchains directory or `C:\Windows\System32`),
/// so omitting `working_dir` makes manifest and `rust-toolchain.toml`
/// resolution fail silently. See `cargo_diagnostic` for the recovery path.
const WORKING_DIR_DESC: &str = "Absolute path to the directory containing the Cargo.toml \
     (or any descendant of the workspace root that owns a rust-toolchain.toml). \
     STRONGLY RECOMMENDED to pass explicitly. If omitted, defaults to the \
     cargo-mcp server process's working directory, which is typically NOT your \
     workspace and will usually cause manifest or toolchain resolution to fail.";

/// `working_dir` description for `cargo_metadata`, which also accepts a
/// workspace member directory.
const WORKING_DIR_DESC_METADATA: &str = "Absolute path to the directory containing the Cargo.toml (or a workspace \
     member). STRONGLY RECOMMENDED to pass explicitly. If omitted, defaults to \
     the cargo-mcp server process's working directory, which is typically NOT \
     your workspace and will usually cause manifest resolution to fail.";

/// `working_dir` description for `cargo_diagnostic`.
const WORKING_DIR_DESC_DIAGNOSTIC: &str = "Absolute path to the directory to diagnose. STRONGLY RECOMMENDED to pass \
     explicitly: this tool is most useful when pointed at the workspace where \
     a cargo command misbehaved. If omitted, defaults to the cargo-mcp server \
     process's working directory, which is typically NOT your workspace.";

// ── shared cargo-option descriptions ──────────────────────────────────────────
// These describe the standard cargo flag groups (package / target / feature /
// compilation / manifest selection) that every build-graph subcommand accepts.
// Centralised as consts so the per-tool JSON schemas stay compact and the
// wording stays consistent.

// Package selection
const WORKSPACE_DESC: &str =
    "If true, operate on all packages in the workspace (--workspace). Default: false.";
const EXCLUDE_DESC: &str = "Package to exclude from a workspace operation (--exclude <SPEC>). \
     Only meaningful together with workspace=true.";

// Target selection
const LIB_DESC: &str = "If true, restrict to the package's library target (--lib). Default: false.";
const BINS_DESC: &str = "If true, select all binary targets (--bins). Default: false.";
const BIN_DESC: &str = "Select only the named binary target (--bin <NAME>).";
const EXAMPLES_DESC: &str = "If true, select all example targets (--examples). Default: false.";
const EXAMPLE_DESC: &str = "Select only the named example target (--example <NAME>).";
const TESTS_DESC: &str = "If true, select all test targets (--tests). Default: false.";
const TEST_TARGET_DESC: &str = "Select only the named integration-test target \
     (--test <NAME>, the filename without .rs under tests/).";
const BENCHES_DESC: &str = "If true, select all benchmark targets (--benches). Default: false.";
const BENCH_DESC: &str = "Select only the named benchmark target (--bench <NAME>).";
const ALL_TARGETS_DESC: &str = "If true, select all targets \
     (lib, bins, tests, benches, examples) (--all-targets). Default: false.";

// Compilation options
const PROFILE_DESC: &str = "Build with the named profile (--profile <NAME>), \
     e.g. a custom profile defined in Cargo.toml. Mutually exclusive with `release`.";
const JOBS_DESC: &str = "Number of parallel build jobs (--jobs <N>). \
     Defaults to the number of logical CPUs.";
const KEEP_GOING_DESC: &str = "If true, build as many targets as possible instead of \
     aborting on the first error (--keep-going). Default: false.";
const TARGET_DESC: &str = "Build for the given target triple (--target <TRIPLE>), \
     e.g. x86_64-unknown-linux-gnu. Omit to build for the host platform.";
const TARGET_DIR_DESC: &str = "Directory for all generated artifacts (--target-dir <DIR>).";
const TIMINGS_DESC: &str = "If true, emit an HTML build-timing report at the end of the \
     build (--timings). Default: false.";

// Manifest options
const IGNORE_RUST_VERSION_DESC: &str = "If true, ignore the `rust-version` field in the \
     affected packages (--ignore-rust-version). Default: false.";
const OFFLINE_DESC: &str =
    "If true, run without accessing the network (--offline). Default: false.";
const FROZEN_DESC: &str = "If true, require Cargo.lock and the cache to be up to date; \
     equivalent to --locked plus --offline (--frozen). Default: false.";
const MANIFEST_PATH_DESC: &str = "Path to the Cargo.toml to operate on (--manifest-path <PATH>).";
const LOCKED_DESC: &str = "If true, assert that Cargo.lock will remain unchanged \
     (--locked): error if it is out of date rather than updating it. Default: false.";

// Subcommand-specific variants
const TREE_TARGET_DESC: &str = "Filter dependencies matching the given target triple \
     (--target <TRIPLE>). Pass `all` to include all targets. Defaults to the host platform.";
const TEST_DOC_DESC: &str = "If true, run only documentation tests (--doc). Default: false.";
const NO_RUN_DESC: &str =
    "If true, compile the tests but do not run them (--no-run). Default: false.";

/// The result of a tool call, which may carry actionable suggestions.
pub enum ToolResult {
    /// Plain text output (no suggestions to extract).
    Text(String),
    /// Output accompanied by actionable compiler/lint suggestions.
    WithSuggestions {
        /// The full output text (NDJSON or formatted).
        output: String,
        /// Extracted suggestions with machine-applicable replacements.
        suggestions: Vec<Suggestion>,
    },
}

// ── helpers ───────────────────────────────────────────────────────────────────

/// Extract an optional string field from JSON args.
fn opt_str<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key).and_then(|v| v.as_str())
}

/// Extract an optional boolean field from JSON args (defaults to false).
fn opt_bool(args: &Value, key: &str) -> bool {
    args.get(key).and_then(|v| v.as_bool()).unwrap_or(false)
}

/// Extract an optional integer field from JSON args as its string form, for
/// flags whose value cargo expects as text (e.g. `--jobs 4`). Non-integer
/// shapes are ignored (treated as absent).
fn opt_int_str(args: &Value, key: &str) -> Option<String> {
    args.get(key)
        .and_then(|v| v.as_i64())
        .map(|n| n.to_string())
}

/// Owned string values for the standard cargo options, extracted up front so
/// the borrowed `&str`s pushed into `argv` outlive the vector. Boolean flags
/// are read directly from `args` at push time and do not need to be stored.
#[derive(Default)]
struct CommonOpts {
    package: Option<String>,
    exclude: Option<String>,
    bin: Option<String>,
    example: Option<String>,
    test: Option<String>,
    bench: Option<String>,
    features: Option<String>,
    profile: Option<String>,
    jobs: Option<String>,
    target: Option<String>,
    target_dir: Option<String>,
    manifest_path: Option<String>,
}

impl CommonOpts {
    fn from_args(args: &Value) -> Self {
        Self {
            package: opt_str(args, "package").map(String::from),
            exclude: opt_str(args, "exclude").map(String::from),
            bin: opt_str(args, "bin").map(String::from),
            example: opt_str(args, "example").map(String::from),
            test: opt_str(args, "test").map(String::from),
            bench: opt_str(args, "bench").map(String::from),
            features: opt_str(args, "features").map(String::from),
            profile: opt_str(args, "profile").map(String::from),
            jobs: opt_int_str(args, "jobs"),
            target: opt_str(args, "target").map(String::from),
            target_dir: opt_str(args, "target_dir").map(String::from),
            manifest_path: opt_str(args, "manifest_path").map(String::from),
        }
    }
}

/// Append cargo's package-selection flags: `--package <SPEC>`, `--workspace`,
/// and `--exclude <SPEC>`. Accepted by every build-graph command.
fn push_package_selection<'a>(argv: &mut Vec<&'a str>, args: &Value, o: &'a CommonOpts) {
    if let Some(p) = &o.package {
        argv.push("--package");
        argv.push(p);
    }
    if opt_bool(args, "workspace") {
        argv.push("--workspace");
    }
    if let Some(e) = &o.exclude {
        argv.push("--exclude");
        argv.push(e);
    }
}

/// Append cargo's full target-selection flags (`--lib`, `--bins`, `--bin`,
/// `--examples`, `--example`, `--tests`, `--test`, `--benches`, `--bench`,
/// `--all-targets`). Accepted by check, build, test, and clippy. `cargo doc`
/// supports only a subset — use [`push_doc_target_selection`] for it.
fn push_target_selection<'a>(argv: &mut Vec<&'a str>, args: &Value, o: &'a CommonOpts) {
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

/// Append the reduced target-selection flags supported by `cargo doc`
/// (`--lib`, `--bins`, `--bin`, `--examples`, `--example`). `cargo doc`
/// has no `--tests`, `--benches`, `--test`, `--bench`, or `--all-targets`.
fn push_doc_target_selection<'a>(argv: &mut Vec<&'a str>, args: &Value, o: &'a CommonOpts) {
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
}

/// Append cargo's feature-selection flags: `--features <list>`,
/// `--all-features`, and `--no-default-features`. Accepted by every
/// build-graph command (check, build, test, clippy, doc, tree).
fn push_feature_flags<'a>(argv: &mut Vec<&'a str>, args: &Value, o: &'a CommonOpts) {
    if let Some(f) = &o.features {
        argv.push("--features");
        argv.push(f);
    }
    if opt_bool(args, "all_features") {
        argv.push("--all-features");
    }
    if opt_bool(args, "no_default_features") {
        argv.push("--no-default-features");
    }
}

/// Append cargo's compilation-option flags: `--release`, `--profile <NAME>`,
/// `--jobs <N>`, `--target <TRIPLE>`, `--target-dir <DIR>`, `--timings`, and
/// (when `keep_going` is true, i.e. the subcommand supports it) `--keep-going`.
/// `cargo test` accepts every flag here except `--keep-going`.
fn push_compilation_options<'a>(
    argv: &mut Vec<&'a str>,
    args: &Value,
    o: &'a CommonOpts,
    keep_going: bool,
) {
    if opt_bool(args, "release") {
        argv.push("--release");
    }
    if let Some(p) = &o.profile {
        argv.push("--profile");
        argv.push(p);
    }
    if let Some(j) = &o.jobs {
        argv.push("--jobs");
        argv.push(j);
    }
    if keep_going && opt_bool(args, "keep_going") {
        argv.push("--keep-going");
    }
    if let Some(t) = &o.target {
        argv.push("--target");
        argv.push(t);
    }
    if let Some(d) = &o.target_dir {
        argv.push("--target-dir");
        argv.push(d);
    }
    if opt_bool(args, "timings") {
        argv.push("--timings");
    }
}

/// Append cargo's manifest-option flags: `--manifest-path <PATH>`, `--locked`,
/// `--offline`, `--frozen`, and (when `ignore_rust_version` is true, i.e. the
/// subcommand supports it) `--ignore-rust-version`. `cargo tree` accepts every
/// flag here except `--ignore-rust-version`.
fn push_manifest_options<'a>(
    argv: &mut Vec<&'a str>,
    args: &Value,
    o: &'a CommonOpts,
    ignore_rust_version: bool,
) {
    if let Some(m) = &o.manifest_path {
        argv.push("--manifest-path");
        argv.push(m);
    }
    if ignore_rust_version && opt_bool(args, "ignore_rust_version") {
        argv.push("--ignore-rust-version");
    }
    if opt_bool(args, "locked") {
        argv.push("--locked");
    }
    if opt_bool(args, "offline") {
        argv.push("--offline");
    }
    if opt_bool(args, "frozen") {
        argv.push("--frozen");
    }
}

/// Extract an optional wall-clock timeout (`timeout_secs`) from JSON args.
///
/// Accepts non-negative integer seconds (the tool schemas declare
/// `minimum: 0`). Missing, `null`, or zero returns `Ok(None)`
/// ("wait forever"). Any other shape — a negative integer, a float,
/// a string, a boolean, an integer outside `u64`, etc. — is rejected
/// with an error rather than silently coerced or dropped, so bad
/// client input surfaces immediately instead of producing an
/// unexpectedly unbounded run.
fn opt_timeout(args: &Value) -> Result<Option<std::time::Duration>, Box<dyn std::error::Error>> {
    let Some(v) = args.get("timeout_secs") else {
        return Ok(None);
    };
    if v.is_null() {
        return Ok(None);
    }
    let Some(n) = v.as_number() else {
        return Err(format!("timeout_secs must be a non-negative integer, got {v}").into());
    };
    let secs = n.as_u64().ok_or_else(|| -> Box<dyn std::error::Error> {
        format!("timeout_secs must be a non-negative integer, got {n}").into()
    })?;
    if secs == 0 {
        return Ok(None);
    }
    Ok(Some(std::time::Duration::from_secs(secs)))
}

/// Like [`opt_timeout`] but distinguishes three states:
/// - `Ok(None)` — key absent or null (caller did not supply a value)
/// - `Ok(Some(None))` — explicitly `0` (caller wants no timeout for this run)
/// - `Ok(Some(Some(d)))` — positive value (caller-specified budget)
fn opt_timeout_explicit(
    args: &Value,
) -> Result<Option<Option<std::time::Duration>>, Box<dyn std::error::Error>> {
    let Some(v) = args.get("timeout_secs") else {
        return Ok(None);
    };
    if v.is_null() {
        return Ok(None);
    }
    let Some(n) = v.as_number() else {
        return Err(format!("timeout_secs must be a non-negative integer, got {v}").into());
    };
    let secs = n.as_u64().ok_or_else(|| -> Box<dyn std::error::Error> {
        format!("timeout_secs must be a non-negative integer, got {n}").into()
    })?;
    if secs == 0 {
        return Ok(Some(None)); // explicit disable
    }
    Ok(Some(Some(std::time::Duration::from_secs(secs))))
}

/// Filter `--message-format=json` NDJSON output to keep only actionable lines.
///
/// Retains only `compiler-message` lines (errors and warnings) and the
/// `build-finished` summary. Everything else — artifacts, build-script events,
/// etc. — was already surfaced via streaming progress notifications and is not
/// useful in the final response. Non-JSON stdout lines (libtest text events,
/// stray prints) are dropped so the formatter's output remains a strict
/// NDJSON stream parseable end-to-end with a single line-by-line JSON parser.
fn filter_build_ndjson(stdout: &str) -> String {
    stdout
        .lines()
        .filter(|line| {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
                matches!(
                    v.get("reason").and_then(|r| r.as_str()),
                    Some("compiler-message") | Some("build-finished")
                )
            } else {
                false
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Like [`filter_build_ndjson`] but also preserves non-JSON stdout lines
/// produced by the test binary (libtest harness output, captured `println!`
/// replays on failure) by wrapping each one in an
/// `x-cargo-mcp-test-output` NDJSON record.
///
/// The whole response remains a strict NDJSON stream — every non-blank line
/// is a JSON object — so consumers can parse it with a single line-by-line
/// JSON parser while still seeing the test output.
fn filter_test_ndjson(stdout: &str) -> String {
    stdout
        .lines()
        .filter_map(|line| {
            if line.trim().is_empty() {
                return None;
            }
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
                // Same filtering as filter_build_ndjson: keep compiler
                // diagnostics and the build-finished summary; drop artifact
                // and build-script noise already delivered via streaming.
                match v.get("reason").and_then(|r| r.as_str()) {
                    Some("compiler-message") | Some("build-finished") => Some(line.to_owned()),
                    _ => None,
                }
            } else {
                // Non-JSON line: libtest harness text or captured test stdout.
                // Wrap in a custom NDJSON record so the stream stays parseable.
                Some(
                    serde_json::to_string(&serde_json::json!({
                        "reason": TEST_OUTPUT_REASON,
                        "text": line,
                    }))
                    .unwrap_or_else(|_| "{}".into()),
                )
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Build a one-line **JSON invocation header** describing the cargo command
/// that produced an output, so the LLM (or a human inspecting the tool
/// result) can see the effective command and working directory at a glance —
/// including flags that the dispatch layer added implicitly (e.g.
/// `--message-format=json`).
///
/// The header is shaped as a cargo-style NDJSON record with a custom,
/// `x-`-prefixed `reason` so the tool result remains a valid stream of
/// JSON-per-line objects:
///
/// ```json
/// {"reason":"x-cargo-mcp-invocation","argv":["build","--message-format=json"],"cwd":"C:\\path"}
/// ```
///
/// The trailing newline is included so the next NDJSON line starts cleanly.
/// `cwd` is `"."` when no working directory was supplied (the same default
/// the underlying child inherits).
fn invocation_header(argv: &[&str], wd: Option<&str>) -> String {
    let payload = serde_json::json!({
        "reason": INVOCATION_REASON,
        "argv": argv,
        "cwd": wd.unwrap_or("."),
    });
    // serde_json::to_string is infallible for owned `Value`s.
    let mut s = serde_json::to_string(&payload).unwrap_or_else(|_| "{}".into());
    s.push('\n');
    s
}

/// Discriminator value placed in the `reason` field of cargo-mcp's
/// invocation-header NDJSON record. Kept as a single constant so consumers
/// (and grep) only have one string to look for.
pub(crate) const INVOCATION_REASON: &str = "x-cargo-mcp-invocation";

/// Discriminator for the NDJSON record that carries the cargo child's
/// stderr (where the Restart Manager holder report and other side-channel
/// diagnostics land). Emitted only on failure when stderr is non-empty.
pub(crate) const STDERR_REASON: &str = "x-cargo-mcp-stderr";

/// Discriminator for the NDJSON record that wraps one line of test harness
/// output (libtest's `running N tests`, `test foo ... ok`, `FAILED` lines,
/// and any captured `println!` replays) in a `cargo_test` result.
/// Each non-JSON stdout line is wrapped individually so the whole response
/// remains a strict NDJSON stream parseable line-by-line.
pub(crate) const TEST_OUTPUT_REASON: &str = "x-cargo-mcp-test-output";

/// Format a [`CargoOutput`] from a `--message-format=json` invocation.
///
/// Filters the NDJSON stream to remove dep-artifact and build-script noise
/// (already delivered as streaming progress), then returns the remainder
/// prefixed with [`invocation_header`]. The output is always a strict
/// NDJSON stream — every non-blank line is a JSON object — so consumers
/// can parse the whole response with a single line-by-line JSON parser.
///
/// On failure, a `{"status":"error","exit_code":N}` trailer is appended
/// after the filtered diagnostics, and any non-empty stderr text is
/// appended as an extra NDJSON record with `reason = STDERR_REASON`. The
/// stderr record is the channel the retry-on-busy code path uses to
/// surface the Restart Manager "who holds this file" report; without it
/// the report is generated but never reaches the agent or the user.
/// Both shapes are the same: filtered records (possibly none) → status
/// trailer → optional stderr record. There is no `message` field on the
/// trailer; stderr always travels in the dedicated record.
fn format_json_output(out: &CargoOutput, argv: &[&str], wd: Option<&str>) -> String {
    let header = invocation_header(argv, wd);
    let body = if out.exit_code == 0 {
        if out.stdout.is_empty() {
            r#"{"status":"success"}"#.to_owned()
        } else {
            filter_build_ndjson(&out.stdout)
        }
    } else {
        let filtered = filter_build_ndjson(&out.stdout);
        let filtered = filtered.trim_end();
        let trailer = format!(r#"{{"status":"error","exit_code":{}}}"#, out.exit_code);
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
        parts.join("\n")
    };
    format!("{header}{body}")
}

/// Format a [`CargoOutput`] from `cargo test --message-format=json`.
///
/// Behaves like [`format_json_output`] for the compilation phase (filtered
/// NDJSON), but also preserves non-JSON stdout lines (libtest harness output,
/// captured `println!` replays on failure) by wrapping each one in an
/// `x-cargo-mcp-test-output` record.  Non-empty stderr — which carries any
/// `eprintln!` output from test code, since libtest does **not** capture
/// stderr — is always included regardless of exit code, wrapped in the usual
/// `x-cargo-mcp-stderr` record.
///
/// Output shape (every line is a JSON object):
/// - `x-cargo-mcp-invocation` — first line, effective command + cwd
/// - `compiler-message` — zero or more compilation errors/warnings
/// - `build-finished` — build phase outcome
/// - `x-cargo-mcp-test-output` — zero or more test harness / captured output
/// - `{"status":"success"}` or `{"status":"error","exit_code":N}` — trailer
/// - `x-cargo-mcp-stderr` — optional, when stderr is non-empty
fn format_test_output(out: &CargoOutput, argv: &[&str], wd: Option<&str>) -> String {
    let header = invocation_header(argv, wd);
    let filtered = filter_test_ndjson(&out.stdout);
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

/// Format a [`CargoOutput`] from a command with no JSON mode (fmt, tree, clean).
///
/// Combines stdout and stderr into a single text block prefixed with
/// [`invocation_header`].
fn format_text_output(out: &CargoOutput, argv: &[&str], wd: Option<&str>) -> String {
    let header = invocation_header(argv, wd);
    let combined = if out.stderr.is_empty() {
        out.stdout.clone()
    } else if out.stdout.is_empty() {
        out.stderr.clone()
    } else {
        format!("{}\n{}", out.stdout, out.stderr)
    };
    let body = if out.exit_code == 0 {
        if combined.is_empty() {
            "(success, no output)".to_owned()
        } else {
            combined
        }
    } else {
        format!("(exit code {})\n{}", out.exit_code, combined)
    };
    format!("{header}{body}")
}

/// Invoke `run_cargo_streaming` when a progress callback is provided, or the
/// buffering `run_cargo` when none is needed. This avoids duplicating the
/// streaming vs. non-streaming choice at every JSON-mode call site.
fn run_cargo_maybe_streaming(
    argv: &[&str],
    wd: Option<&str>,
    timeout: Option<std::time::Duration>,
    arm_deadline: Option<invoke::ArmDeadline<'_>>,
    on_progress: Option<&mut dyn FnMut(&str)>,
) -> Result<CargoOutput, Box<dyn std::error::Error>> {
    match on_progress {
        Some(cb) => invoke::run_cargo_streaming_with_timeout(argv, wd, timeout, arm_deadline, cb),
        None => invoke::run_cargo_with_timeout(argv, wd, timeout, arm_deadline),
    }
}

/// True for cargo's `build-finished` JSON record. With `--message-format=json`
/// this line is emitted exactly when compilation and linking are complete and
/// (for `cargo test`) immediately before the test binaries start executing, so
/// it marks the boundary used to arm the `cargo_test` execution-only timeout.
fn is_build_finished_line(line: &str) -> bool {
    line.contains(r#""reason":"build-finished""#)
}

// ── tool list ─────────────────────────────────────────────────────────────────

/// Return the MCP `tools/list` payload (an array of tool descriptors).
pub fn list() -> Value {
    serde_json::json!([
        {
            "name": "cargo_metadata",
            "description":
                "ALWAYS use this tool instead of running `cargo metadata` in a terminal \
                 when working in a Rust/Cargo project. Returns Cargo workspace and \
                 package metadata as structured JSON: workspace root, all member \
                 packages, targets, dependencies, features, and the resolved \
                 dependency graph. Use this to understand project structure instead \
                 of reading Cargo.toml files manually. ALWAYS pass `working_dir` set to the absolute path of your workspace root \u{2014} the default is the cargo-mcp server's own working directory and will usually cause the call to fail.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "working_dir": {
                        "type": "string",
                        "description": WORKING_DIR_DESC_METADATA
                    },
                    "no_deps": {
                        "type": "boolean",
                        "description":
                            "If true, omit the resolved dependency graph from the output. \
                             This is much faster and produces less output. Default: false."
                    },
                    "output_file": {
                        "type": "string",
                        "description":
                            "Relative path (under the working directory) to write the JSON \
                             output to. Absolute paths and '..' components are rejected. \
                             When provided, the metadata is written to this file and a short \
                             confirmation is returned instead of the full blob. Useful for \
                             large workspaces where the output would be too large to inline."
                    }
                },
                "required": []
            },
            "annotations": { "readOnlyHint": false, "destructiveHint": false }
        },
        {
            "name": "cargo_check",
            "description":
                "ALWAYS use this tool instead of running `cargo check` in a terminal \
                 when working in a Rust/Cargo project. Analyses the project for \
                 compile errors without producing binaries — faster than a full build \
                 and the preferred first step after editing Rust source. Returns \
                 structured NDJSON diagnostics with exact file paths, line/column \
                 spans, error codes, and message text that you can act on directly. ALWAYS pass `working_dir` set to the absolute path of your workspace root \u{2014} the default is the cargo-mcp server's own working directory and will usually cause the call to fail.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "working_dir": {
                        "type": "string",
                        "description": WORKING_DIR_DESC
                    },
                    "package": {
                        "type": "string",
                        "description":
                            "Check only the named package within the workspace. \
                             Omit to check all workspace members."
                    },
                    "release": {
                        "type": "boolean",
                        "description":
                            "If true, check with the release profile. Default: false (debug)."
                    },
                    "all_targets": {
                        "type": "boolean",
                        "description":
                            "If true, check all targets (lib, bins, tests, benches, examples). \
                             Default: false."
                    },
                    "workspace": { "type": "boolean", "description": WORKSPACE_DESC },
                    "exclude": { "type": "string", "description": EXCLUDE_DESC },
                    "lib": { "type": "boolean", "description": LIB_DESC },
                    "bins": { "type": "boolean", "description": BINS_DESC },
                    "bin": { "type": "string", "description": BIN_DESC },
                    "examples": { "type": "boolean", "description": EXAMPLES_DESC },
                    "example": { "type": "string", "description": EXAMPLE_DESC },
                    "tests": { "type": "boolean", "description": TESTS_DESC },
                    "test": { "type": "string", "description": TEST_TARGET_DESC },
                    "benches": { "type": "boolean", "description": BENCHES_DESC },
                    "bench": { "type": "string", "description": BENCH_DESC },
                    "profile": { "type": "string", "description": PROFILE_DESC },
                    "jobs": { "type": "integer", "minimum": 1, "description": JOBS_DESC },
                    "keep_going": { "type": "boolean", "description": KEEP_GOING_DESC },
                    "target": { "type": "string", "description": TARGET_DESC },
                    "target_dir": { "type": "string", "description": TARGET_DIR_DESC },
                    "timings": { "type": "boolean", "description": TIMINGS_DESC },
                    "ignore_rust_version": { "type": "boolean", "description": IGNORE_RUST_VERSION_DESC },
                    "manifest_path": { "type": "string", "description": MANIFEST_PATH_DESC },
                    "offline": { "type": "boolean", "description": OFFLINE_DESC },
                    "frozen": { "type": "boolean", "description": FROZEN_DESC },
                    "features": {
                        "type": "string",
                        "description":
                            "Comma-separated list of features to activate. \
                             Omit to use default features."
                    },
                    "all_features": {
                        "type": "boolean",
                        "description":
                            "If true, activate all features of all selected packages \
                             (passes --all-features). Default: false."
                    },
                    "no_default_features": {
                        "type": "boolean",
                        "description":
                            "If true, do not activate the `default` feature \
                             (passes --no-default-features). Default: false."
                    },
                    "locked": {
                        "type": "boolean",
                        "description":
                            "If true, pass --locked: error if Cargo.lock is out of date \
                             rather than updating it. Use in CI to enforce a committed lockfile. \
                             Default: false."
                    },
                    "timeout_secs": {
                        "type": "integer",
                        "minimum": 0,
                        "description":
                            "Optional wall-clock budget in seconds. When the budget elapses, \
                             cargo and the entire subprocess tree (rustc, test binaries, \
                             build scripts) are terminated and the call returns a timeout \
                             error. 0 or omitted means no timeout (the default). Recommended \
                             for bounding runaway test runs."
                    }
                },
                "required": []
            },
            "annotations": { "readOnlyHint": true, "destructiveHint": false }
        },
        {
            "name": "cargo_build",
            "description":
                "ALWAYS use this tool instead of running `cargo build` in a terminal \
                 when working in a Rust/Cargo project. Compiles the project and \
                 returns structured NDJSON diagnostics with exact file paths, \
                 line/column spans, and message text. Prefer cargo_check for \
                 error-only checking when binaries are not needed. ALWAYS pass `working_dir` set to the absolute path of your workspace root \u{2014} the default is the cargo-mcp server's own working directory and will usually cause the call to fail.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "working_dir": {
                        "type": "string",
                        "description": WORKING_DIR_DESC
                    },
                    "package": {
                        "type": "string",
                        "description":
                            "Build only the named package within the workspace. \
                             Omit to build all workspace members."
                    },
                    "release": {
                        "type": "boolean",
                        "description":
                            "If true, build with the release profile (optimised). \
                             Default: false (debug)."
                    },
                    "all_targets": {
                        "type": "boolean",
                        "description":
                            "If true, build all targets (lib, bins, tests, benches, examples). \
                             Default: false."
                    },
                    "workspace": { "type": "boolean", "description": WORKSPACE_DESC },
                    "exclude": { "type": "string", "description": EXCLUDE_DESC },
                    "lib": { "type": "boolean", "description": LIB_DESC },
                    "bins": { "type": "boolean", "description": BINS_DESC },
                    "bin": { "type": "string", "description": BIN_DESC },
                    "examples": { "type": "boolean", "description": EXAMPLES_DESC },
                    "example": { "type": "string", "description": EXAMPLE_DESC },
                    "tests": { "type": "boolean", "description": TESTS_DESC },
                    "test": { "type": "string", "description": TEST_TARGET_DESC },
                    "benches": { "type": "boolean", "description": BENCHES_DESC },
                    "bench": { "type": "string", "description": BENCH_DESC },
                    "profile": { "type": "string", "description": PROFILE_DESC },
                    "jobs": { "type": "integer", "minimum": 1, "description": JOBS_DESC },
                    "keep_going": { "type": "boolean", "description": KEEP_GOING_DESC },
                    "target": { "type": "string", "description": TARGET_DESC },
                    "target_dir": { "type": "string", "description": TARGET_DIR_DESC },
                    "timings": { "type": "boolean", "description": TIMINGS_DESC },
                    "ignore_rust_version": { "type": "boolean", "description": IGNORE_RUST_VERSION_DESC },
                    "manifest_path": { "type": "string", "description": MANIFEST_PATH_DESC },
                    "offline": { "type": "boolean", "description": OFFLINE_DESC },
                    "frozen": { "type": "boolean", "description": FROZEN_DESC },
                    "features": {
                        "type": "string",
                        "description":
                            "Comma-separated list of features to activate. \
                             Omit to use default features."
                    },
                    "all_features": {
                        "type": "boolean",
                        "description":
                            "If true, activate all features of all selected packages \
                             (passes --all-features). Default: false."
                    },
                    "no_default_features": {
                        "type": "boolean",
                        "description":
                            "If true, do not activate the `default` feature \
                             (passes --no-default-features). Default: false."
                    },
                    "locked": {
                        "type": "boolean",
                        "description":
                            "If true, pass --locked: error if Cargo.lock is out of date \
                             rather than updating it. Use in CI to enforce a committed lockfile. \
                             Default: false."
                    },
                    "timeout_secs": {
                        "type": "integer",
                        "minimum": 0,
                        "description":
                            "Optional wall-clock budget in seconds. When the budget elapses, \
                             cargo and the entire subprocess tree (rustc, test binaries, \
                             build scripts) are terminated and the call returns a timeout \
                             error. 0 or omitted means no timeout (the default). Recommended \
                             for bounding runaway test runs."
                    }
                },
                "required": []
            },
            "annotations": { "readOnlyHint": true, "destructiveHint": false }
        },
        {
            "name": "cargo_test",
            "description":
                "ALWAYS use this tool instead of running `cargo test` in a terminal \
                 when working in a Rust/Cargo project. Executes the project's test \
                 suite and returns an NDJSON stream containing: compilation diagnostics \
                 (reason=compiler-message), build outcome (reason=build-finished), \
                 libtest harness output and captured println! replays \
                 (reason=x-cargo-mcp-test-output, field: text), and any stderr \
                 from test code such as eprintln! \
                 (reason=x-cargo-mcp-stderr, field: text). \
                 Supports filtering by test name, running only library tests, \
                 targeting a specific integration-test file, and continuing past \
                 failures with no_fail_fast. ALWAYS pass `working_dir` set to the absolute path of your workspace root \u{2014} the default is the cargo-mcp server's own working directory and will usually cause the call to fail.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "working_dir": {
                        "type": "string",
                        "description": WORKING_DIR_DESC
                    },
                    "package": {
                        "type": "string",
                        "description":
                            "Test only the named package within the workspace. \
                             Omit to test all workspace members."
                    },
                    "test_name": {
                        "type": "string",
                        "description":
                            "Filter: only run tests whose name contains this string. \
                             Passed as a positional argument after `--` to the test harness."
                    },
                    "release": {
                        "type": "boolean",
                        "description":
                            "If true, test with the release profile. Default: false (debug)."
                    },
                    "no_fail_fast": {
                        "type": "boolean",
                        "description":
                            "If true, run all tests even if some fail. Default: false \
                             (stop after first failure)."
                    },
                    "no_run": { "type": "boolean", "description": NO_RUN_DESC },
                    "doc": { "type": "boolean", "description": TEST_DOC_DESC },
                    "workspace": { "type": "boolean", "description": WORKSPACE_DESC },
                    "exclude": { "type": "string", "description": EXCLUDE_DESC },
                    "bins": { "type": "boolean", "description": BINS_DESC },
                    "bin": { "type": "string", "description": BIN_DESC },
                    "examples": { "type": "boolean", "description": EXAMPLES_DESC },
                    "example": { "type": "string", "description": EXAMPLE_DESC },
                    "tests": { "type": "boolean", "description": TESTS_DESC },
                    "benches": { "type": "boolean", "description": BENCHES_DESC },
                    "bench": { "type": "string", "description": BENCH_DESC },
                    "all_targets": { "type": "boolean", "description": ALL_TARGETS_DESC },
                    "profile": { "type": "string", "description": PROFILE_DESC },
                    "jobs": { "type": "integer", "minimum": 1, "description": JOBS_DESC },
                    "target": { "type": "string", "description": TARGET_DESC },
                    "target_dir": { "type": "string", "description": TARGET_DIR_DESC },
                    "timings": { "type": "boolean", "description": TIMINGS_DESC },
                    "ignore_rust_version": { "type": "boolean", "description": IGNORE_RUST_VERSION_DESC },
                    "manifest_path": { "type": "string", "description": MANIFEST_PATH_DESC },
                    "offline": { "type": "boolean", "description": OFFLINE_DESC },
                    "frozen": { "type": "boolean", "description": FROZEN_DESC },
                    "features": {
                        "type": "string",
                        "description":
                            "Comma-separated list of features to activate. \
                             Omit to use default features."
                    },
                    "all_features": {
                        "type": "boolean",
                        "description":
                            "If true, activate all features of all selected packages \
                             (passes --all-features). Default: false."
                    },
                    "no_default_features": {
                        "type": "boolean",
                        "description":
                            "If true, do not activate the `default` feature \
                             (passes --no-default-features). Default: false."
                    },
                    "lib": {
                        "type": "boolean",
                        "description":
                            "If true, only run library tests (unit tests in src/). \
                             Default: false."
                    },
                    "test": {
                        "type": "string",
                        "description":
                            "Run only the integration test target with this name \
                             (filename without .rs extension under tests/)."
                    },
                    "exact": {
                        "type": "boolean",
                        "description":
                            "If true, the test_name filter must match exactly (not as substring). \
                             Default: false."
                    },
                    "locked": {
                        "type": "boolean",
                        "description":
                            "If true, pass --locked: error if Cargo.lock is out of date \
                             rather than updating it. Use in CI to enforce a committed lockfile. \
                             Default: false."
                    },
                    "timeout_secs": {
                        "type": "integer",
                        "minimum": 0,
                        "description":
                            "Wall-clock budget in seconds for test execution. The clock starts \
                             when compilation and linking finish (cargo's build-finished record), \
                             so the build phase is never timed — only the running tests. When the \
                             budget elapses, cargo and the entire subprocess tree (test binaries, \
                             etc.) are terminated. If omitted, the server-configured default \
                             applies (30 seconds when launched by the VS Code extension; no timeout \
                             otherwise). Pass 0 to disable the timeout for this run regardless of \
                             the server default."
                    }
                },
                "required": []
            },
            "annotations": { "readOnlyHint": true, "destructiveHint": false }
        },
        {
            "name": "cargo_clippy",
            "description":
                "ALWAYS use this tool instead of running `cargo clippy` in a terminal \
                 when working in a Rust/Cargo project. Runs lint analysis and returns \
                 structured NDJSON diagnostics with exact file paths, line/column spans, \
                 severity, lint names, and suggested fixes. Use this after editing Rust \
                 source to catch non-idiomatic patterns and common mistakes. ALWAYS pass `working_dir` set to the absolute path of your workspace root \u{2014} the default is the cargo-mcp server's own working directory and will usually cause the call to fail.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "working_dir": {
                        "type": "string",
                        "description": WORKING_DIR_DESC
                    },
                    "package": {
                        "type": "string",
                        "description":
                            "Lint only the named package within the workspace. \
                             Omit to lint all workspace members."
                    },
                    "all_targets": {
                        "type": "boolean",
                        "description":
                            "If true, lint all targets (lib, bins, tests, benches, examples). \
                             Default: false."
                    },
                    "workspace": { "type": "boolean", "description": WORKSPACE_DESC },
                    "exclude": { "type": "string", "description": EXCLUDE_DESC },
                    "lib": { "type": "boolean", "description": LIB_DESC },
                    "bins": { "type": "boolean", "description": BINS_DESC },
                    "bin": { "type": "string", "description": BIN_DESC },
                    "examples": { "type": "boolean", "description": EXAMPLES_DESC },
                    "example": { "type": "string", "description": EXAMPLE_DESC },
                    "tests": { "type": "boolean", "description": TESTS_DESC },
                    "test": { "type": "string", "description": TEST_TARGET_DESC },
                    "benches": { "type": "boolean", "description": BENCHES_DESC },
                    "bench": { "type": "string", "description": BENCH_DESC },
                    "profile": { "type": "string", "description": PROFILE_DESC },
                    "jobs": { "type": "integer", "minimum": 1, "description": JOBS_DESC },
                    "keep_going": { "type": "boolean", "description": KEEP_GOING_DESC },
                    "target": { "type": "string", "description": TARGET_DESC },
                    "target_dir": { "type": "string", "description": TARGET_DIR_DESC },
                    "timings": { "type": "boolean", "description": TIMINGS_DESC },
                    "ignore_rust_version": { "type": "boolean", "description": IGNORE_RUST_VERSION_DESC },
                    "manifest_path": { "type": "string", "description": MANIFEST_PATH_DESC },
                    "offline": { "type": "boolean", "description": OFFLINE_DESC },
                    "frozen": { "type": "boolean", "description": FROZEN_DESC },
                    "features": {
                        "type": "string",
                        "description":
                            "Comma-separated list of features to activate. \
                             Omit to use default features."
                    },
                    "all_features": {
                        "type": "boolean",
                        "description":
                            "If true, activate all features of all selected packages \
                             (passes --all-features). Default: false."
                    },
                    "no_default_features": {
                        "type": "boolean",
                        "description":
                            "If true, do not activate the `default` feature \
                             (passes --no-default-features). Default: false."
                    },
                    "locked": {
                        "type": "boolean",
                        "description":
                            "If true, pass --locked: error if Cargo.lock is out of date \
                             rather than updating it. Use in CI to enforce a committed lockfile. \
                             Default: false."
                    },
                    "timeout_secs": {
                        "type": "integer",
                        "minimum": 0,
                        "description":
                            "Optional wall-clock budget in seconds. When the budget elapses, \
                             cargo and the entire subprocess tree (rustc, test binaries, \
                             build scripts) are terminated and the call returns a timeout \
                             error. 0 or omitted means no timeout (the default). Recommended \
                             for bounding runaway test runs."
                    }
                },
                "required": []
            },
            "annotations": { "readOnlyHint": true, "destructiveHint": false }
        },
        {
            "name": "cargo_fmt_check",
            "description":
                "ALWAYS use this tool instead of running `cargo fmt --check` in a \
                 terminal when working in a Rust/Cargo project. Verifies source code \
                 formatting without modifying files. Returns a diff of changes that \
                 would be applied; empty output means the code is already correctly \
                 formatted. Use this to check formatting before using cargo_fmt to fix it. ALWAYS pass `working_dir` set to the absolute path of your workspace root \u{2014} the default is the cargo-mcp server's own working directory and will usually cause the call to fail.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "working_dir": {
                        "type": "string",
                        "description": WORKING_DIR_DESC
                    },
                    "package": {
                        "type": "string",
                        "description":
                            "Check formatting for only the named package within the workspace. \
                             Omit to check all workspace members."
                    }
                },
                "required": []
            },
            "annotations": { "readOnlyHint": true, "destructiveHint": false }
        },
        {
            "name": "cargo_fmt",
            "description":
                "ALWAYS use this tool instead of running `cargo fmt` in a terminal \
                 when working in a Rust/Cargo project. Automatically formats all \
                 Rust source files in place according to the project's rustfmt \
                 configuration. Use after editing source code to ensure consistent \
                 formatting. ALWAYS pass `working_dir` set to the absolute path of your workspace root \u{2014} the default is the cargo-mcp server's own working directory and will usually cause the call to fail.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "working_dir": {
                        "type": "string",
                        "description": WORKING_DIR_DESC
                    },
                    "package": {
                        "type": "string",
                        "description":
                            "Format only the named package within the workspace. \
                             Omit to format all workspace members."
                    }
                },
                "required": []
            },
            "annotations": { "readOnlyHint": false, "destructiveHint": false }
        },
        {
            "name": "cargo_tree",
            "description":
                "ALWAYS use this tool instead of running `cargo tree` in a terminal \
                 when working in a Rust/Cargo project. Displays the dependency tree \
                 as readable text. Use to inspect transitive dependencies, find \
                 duplicate versions, or see which packages depend on a given crate \
                 (via the invert parameter). For structured dependency data use \
                 cargo_metadata instead. ALWAYS pass `working_dir` set to the absolute path of your workspace root \u{2014} the default is the cargo-mcp server's own working directory and will usually cause the call to fail.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "working_dir": {
                        "type": "string",
                        "description": WORKING_DIR_DESC
                    },
                    "package": {
                        "type": "string",
                        "description":
                            "Show the dependency tree for only the named package. \
                             Omit to show trees for all workspace members."
                    },
                    "depth": {
                        "type": "integer",
                        "description":
                            "Maximum depth of the dependency tree to display. \
                             Omit for unlimited depth."
                    },
                    "invert": {
                        "type": "string",
                        "description":
                            "Invert the tree to show which packages depend on the \
                             named crate. Value is the crate name to invert on."
                    },
                    "duplicates": {
                        "type": "boolean",
                        "description":
                            "If true, only show packages that appear more than once \
                             in the dependency graph (duplicate versions). Default: false."
                    },
                    "workspace": { "type": "boolean", "description": WORKSPACE_DESC },
                    "exclude": { "type": "string", "description": EXCLUDE_DESC },
                    "target": { "type": "string", "description": TREE_TARGET_DESC },
                    "manifest_path": { "type": "string", "description": MANIFEST_PATH_DESC },
                    "locked": { "type": "boolean", "description": LOCKED_DESC },
                    "offline": { "type": "boolean", "description": OFFLINE_DESC },
                    "frozen": { "type": "boolean", "description": FROZEN_DESC },
                    "features": {
                        "type": "string",
                        "description":
                            "Comma-separated list of features to activate. \
                             Omit to use default features."
                    },
                    "all_features": {
                        "type": "boolean",
                        "description":
                            "If true, activate all features of all selected packages \
                             (passes --all-features). Default: false."
                    },
                    "no_default_features": {
                        "type": "boolean",
                        "description":
                            "If true, do not activate the `default` feature \
                             (passes --no-default-features). Default: false."
                    }
                },
                "required": []
            },
            "annotations": { "readOnlyHint": true, "destructiveHint": false }
        },
        {
            "name": "cargo_doc",
            "description":
                "ALWAYS use this tool instead of running `cargo doc` in a terminal \
                 when working in a Rust/Cargo project. Builds HTML documentation for \
                 the project (written to target/doc/) and returns structured NDJSON \
                 diagnostics for any warnings or errors encountered during the build. ALWAYS pass `working_dir` set to the absolute path of your workspace root \u{2014} the default is the cargo-mcp server's own working directory and will usually cause the call to fail.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "working_dir": {
                        "type": "string",
                        "description": WORKING_DIR_DESC
                    },
                    "package": {
                        "type": "string",
                        "description":
                            "Build docs for only the named package. \
                             Omit to build docs for all workspace members."
                    },
                    "no_deps": {
                        "type": "boolean",
                        "description":
                            "If true, do not build documentation for dependencies. \
                             Default: false."
                    },
                    "document_private_items": {
                        "type": "boolean",
                        "description":
                            "If true, include documentation for private items. \
                             Default: false."
                    },
                    "workspace": { "type": "boolean", "description": WORKSPACE_DESC },
                    "exclude": { "type": "string", "description": EXCLUDE_DESC },
                    "lib": { "type": "boolean", "description": LIB_DESC },
                    "bins": { "type": "boolean", "description": BINS_DESC },
                    "bin": { "type": "string", "description": BIN_DESC },
                    "examples": { "type": "boolean", "description": EXAMPLES_DESC },
                    "example": { "type": "string", "description": EXAMPLE_DESC },
                    "profile": { "type": "string", "description": PROFILE_DESC },
                    "jobs": { "type": "integer", "minimum": 1, "description": JOBS_DESC },
                    "keep_going": { "type": "boolean", "description": KEEP_GOING_DESC },
                    "target": { "type": "string", "description": TARGET_DESC },
                    "target_dir": { "type": "string", "description": TARGET_DIR_DESC },
                    "timings": { "type": "boolean", "description": TIMINGS_DESC },
                    "ignore_rust_version": { "type": "boolean", "description": IGNORE_RUST_VERSION_DESC },
                    "manifest_path": { "type": "string", "description": MANIFEST_PATH_DESC },
                    "offline": { "type": "boolean", "description": OFFLINE_DESC },
                    "frozen": { "type": "boolean", "description": FROZEN_DESC },
                    "features": {
                        "type": "string",
                        "description":
                            "Comma-separated list of features to activate. \
                             Omit to use default features."
                    },
                    "all_features": {
                        "type": "boolean",
                        "description":
                            "If true, activate all features of all selected packages \
                             (passes --all-features). Default: false."
                    },
                    "no_default_features": {
                        "type": "boolean",
                        "description":
                            "If true, do not activate the `default` feature \
                             (passes --no-default-features). Default: false."
                    },
                    "locked": {
                        "type": "boolean",
                        "description":
                            "If true, pass --locked: error if Cargo.lock is out of date \
                             rather than updating it. Use in CI to enforce a committed lockfile. \
                             Default: false."
                    }
                },
                "required": []
            },
            "annotations": { "readOnlyHint": true, "destructiveHint": false }
        },
        {
            "name": "cargo_clean",
            "description":
                "ALWAYS use this tool instead of running `cargo clean` in a terminal \
                 when working in a Rust/Cargo project. Removes build artefacts from \
                 the target directory, freeing disk space and forcing a full rebuild \
                 on the next build command. Use when builds are in an inconsistent state. ALWAYS pass `working_dir` set to the absolute path of your workspace root \u{2014} the default is the cargo-mcp server's own working directory and will usually cause the call to fail.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "working_dir": {
                        "type": "string",
                        "description": WORKING_DIR_DESC
                    },
                    "package": {
                        "type": "string",
                        "description":
                            "Clean only the named package's artefacts. \
                             Omit to clean all artefacts."
                    },
                    "release": {
                        "type": "boolean",
                        "description":
                            "If true, clean only the release profile artefacts. \
                             Default: false (clean all profiles)."
                    }
                },
                "required": []
            },
            "annotations": { "readOnlyHint": false, "destructiveHint": true }
        },
        {
            "name": "cargo_update",
            "description":
                "ALWAYS use this tool instead of running `cargo update` in a terminal \
                 when working in a Rust/Cargo project. Updates dependency versions in \
                 Cargo.lock to the latest compatible versions allowed by Cargo.toml. \
                 Use after adding new dependencies or when you want to pull in \
                 compatible dependency updates. ALWAYS pass `working_dir` set to the absolute path of your workspace root \u{2014} the default is the cargo-mcp server's own working directory and will usually cause the call to fail.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "working_dir": {
                        "type": "string",
                        "description": WORKING_DIR_DESC
                    },
                    "package": {
                        "type": "string",
                        "description":
                            "Update only the named dependency. \
                             Omit to update all dependencies."
                    },
                    "precise": {
                        "type": "string",
                        "description":
                            "Update the package specified by `package` to exactly \
                             this version string (e.g. \"1.2.3\")."
                    }
                },
                "required": []
            },
            "annotations": { "readOnlyHint": false, "destructiveHint": false }
        },
        {
            "name": "cargo_fix",
            "description":
                "ALWAYS use this tool instead of running `cargo fix` in a terminal \
                 when working in a Rust/Cargo project. Automatically applies \
                 compiler-suggested fixes (machine-applicable lints and edition \
                 migrations) to source files. Use after cargo_check or cargo_clippy \
                 to apply safe fixes in bulk. Returns plain text output. ALWAYS pass `working_dir` set to the absolute path of your workspace root \u{2014} the default is the cargo-mcp server's own working directory and will usually cause the call to fail.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "working_dir": {
                        "type": "string",
                        "description": WORKING_DIR_DESC
                    },
                    "package": {
                        "type": "string",
                        "description":
                            "Fix only the named package within the workspace. \
                             Omit to fix all workspace members."
                    },
                    "allow_dirty": {
                        "type": "boolean",
                        "description":
                            "If true, allow fixing even if the working tree has \
                             uncommitted changes. Default: false."
                    },
                    "allow_staged": {
                        "type": "boolean",
                        "description":
                            "If true, allow fixing even if there are staged but \
                             uncommitted changes. Default: false."
                    },
                    "clippy": {
                        "type": "boolean",
                        "description":
                            "If true, also apply Clippy's machine-applicable suggestions \
                             (equivalent to `cargo clippy --fix`). Default: false."
                    }
                },
                "required": []
            },
            "annotations": { "readOnlyHint": false, "destructiveHint": false }
        },
        {
            "name": "cargo_add",
            "description":
                "ALWAYS use this tool instead of running `cargo add` in a terminal \
                 when working in a Rust/Cargo project. Adds one or more dependencies \
                 to Cargo.toml and updates Cargo.lock. Specify an exact version with \
                 the `version` parameter or let Cargo choose the latest compatible release. ALWAYS pass `working_dir` set to the absolute path of your workspace root \u{2014} the default is the cargo-mcp server's own working directory and will usually cause the call to fail.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "working_dir": {
                        "type": "string",
                        "description": WORKING_DIR_DESC
                    },
                    "dependency": {
                        "type": "string",
                        "description":
                            "Name of the crate to add (e.g. \"serde\"). \
                             May include a version requirement (e.g. \"serde@1.0\")."
                    },
                    "features": {
                        "type": "string",
                        "description":
                            "Comma-separated list of features to enable for the added dependency."
                    },
                    "dev": {
                        "type": "boolean",
                        "description":
                            "If true, add as a dev-dependency. Default: false."
                    },
                    "build": {
                        "type": "boolean",
                        "description":
                            "If true, add as a build-dependency. Default: false."
                    },
                    "package": {
                        "type": "string",
                        "description":
                            "Add the dependency to this workspace member's Cargo.toml. \
                             Omit to use the root/only package."
                    }
                },
                "required": ["dependency"]
            },
            "annotations": { "readOnlyHint": false, "destructiveHint": false }
        },
        {
            "name": "cargo_remove",
            "description":
                "ALWAYS use this tool instead of running `cargo remove` in a terminal \
                 when working in a Rust/Cargo project. Removes a dependency from \
                 Cargo.toml and updates Cargo.lock. ALWAYS pass `working_dir` set to the absolute path of your workspace root \u{2014} the default is the cargo-mcp server's own working directory and will usually cause the call to fail.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "working_dir": {
                        "type": "string",
                        "description": WORKING_DIR_DESC
                    },
                    "dependency": {
                        "type": "string",
                        "description":
                            "Name of the crate to remove."
                    },
                    "dev": {
                        "type": "boolean",
                        "description":
                            "If true, remove from dev-dependencies. Default: false."
                    },
                    "build": {
                        "type": "boolean",
                        "description":
                            "If true, remove from build-dependencies. Default: false."
                    },
                    "package": {
                        "type": "string",
                        "description":
                            "Remove the dependency from this workspace member's Cargo.toml. \
                             Omit to use the root/only package."
                    }
                },
                "required": ["dependency"]
            },
            "annotations": { "readOnlyHint": false, "destructiveHint": false }
        },
        {
            "name": "cargo_publish",
            "description":
                "ALWAYS use this tool instead of running `cargo publish` in a terminal \
                 when working in a Rust/Cargo project. Packages and uploads the crate \
                 to crates.io. IMPORTANT: publishing is permanent — a version cannot \
                 be deleted from crates.io. Always run with dry_run=true first to \
                 validate the package before publishing for real. ALWAYS pass `working_dir` set to the absolute path of your workspace root \u{2014} the default is the cargo-mcp server's own working directory and will usually cause the call to fail.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "working_dir": {
                        "type": "string",
                        "description": WORKING_DIR_DESC
                    },
                    "package": {
                        "type": "string",
                        "description":
                            "Publish only the named package within the workspace."
                    },
                    "dry_run": {
                        "type": "boolean",
                        "description":
                            "If true, perform all checks and packaging steps but do not \
                             actually upload to crates.io. Use this first to validate \
                             before a real publish. Default: false."
                    },
                    "allow_dirty": {
                        "type": "boolean",
                        "description":
                            "If true, allow publishing even if the working tree has \
                             uncommitted changes. Default: false."
                    }
                },
                "required": []
            },
            "annotations": { "readOnlyHint": false, "destructiveHint": true }
        },
        {
            "name": "cargo_setup",
            "description":
                "Returns the canonical cargo-mcp instructions that should appear \
                 somewhere in this repository's Copilot instruction files \
                 (e.g. `.github/copilot-instructions.md` or a sub-instructions file). \
                 After receiving this output, YOU should locate the relevant \
                 instruction files in the workspace, decide where the cargo-mcp \
                 section best fits given the project's existing conventions, and \
                 add or update it as needed. The wording does not need to match \
                 exactly — adapt it to the style of any existing instructions. \
                 Run this once after installing cargo-mcp in a new repository.",
            "inputSchema": {
                "type": "object",
                "properties": {},
                "required": []
            },
            "annotations": { "readOnlyHint": true, "destructiveHint": false }
        },
        {
            "name": "cargo_diagnostic",
            "description":
                "Report which `cargo` and `rustc` binaries cargo-mcp will invoke for \
                 the given working directory, why those were chosen, whether a \
                 rust-toolchain.toml is in effect, and the relevant environment \
                 (PATH, CARGO, RUSTC, RUSTUP_TOOLCHAIN, RUSTUP_HOME, CARGO_HOME). \
                 Use this when a cargo command appears to use the wrong toolchain \
                 (e.g. rust-toolchain.toml seems to be ignored). ALWAYS pass `working_dir` set to the absolute path of your workspace root \u{2014} the default is the cargo-mcp server's own working directory and will usually cause the call to fail.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "working_dir": {
                        "type": "string",
                        "description": WORKING_DIR_DESC_DIAGNOSTIC
                    }
                },
                "required": []
            },
            "annotations": { "readOnlyHint": true, "destructiveHint": false }
        }
    ])
}

// ── dispatch ──────────────────────────────────────────────────────────────────

/// Return the names of all registered tools, in the same order as [`list`].
pub fn tool_names() -> Vec<&'static str> {
    vec![
        "cargo_metadata",
        "cargo_check",
        "cargo_build",
        "cargo_test",
        "cargo_clippy",
        "cargo_fmt_check",
        "cargo_fmt",
        "cargo_tree",
        "cargo_doc",
        "cargo_clean",
        "cargo_update",
        "cargo_fix",
        "cargo_add",
        "cargo_remove",
        "cargo_publish",
        "cargo_setup",
        "cargo_diagnostic",
    ]
}

/// Dispatch an MCP `tools/call` to the appropriate tool implementation.
///
/// Pass `on_progress` for streaming progress callbacks during JSON-mode
/// compilation tools. Pass `None` to buffer all output and return it only
/// at the end.
pub fn call(
    name: &str,
    args: &Value,
    on_progress: Option<&mut dyn FnMut(&str)>,
    cancel: Option<Arc<AtomicBool>>,
) -> Result<ToolResult, Box<dyn std::error::Error>> {
    // Install the cancel token for the duration of the tool call so that the
    // invoke functions can kill the child process if the client cancels.
    invoke::set_cancel_token(cancel);
    let result = call_inner(name, args, on_progress);
    invoke::set_cancel_token(None);
    result
}

fn call_inner(
    name: &str,
    args: &Value,
    on_progress: Option<&mut dyn FnMut(&str)>,
) -> Result<ToolResult, Box<dyn std::error::Error>> {
    match name {
        "cargo_metadata" => call_metadata(args).map(ToolResult::Text),
        "cargo_check" => call_check(args, on_progress),
        "cargo_build" => call_build(args, on_progress).map(ToolResult::Text),
        "cargo_test" => call_test(args, on_progress).map(ToolResult::Text),
        "cargo_clippy" => call_clippy(args, on_progress),
        "cargo_fmt_check" => call_fmt_check(args).map(ToolResult::Text),
        "cargo_fmt" => call_fmt(args).map(ToolResult::Text),
        "cargo_tree" => call_tree(args).map(ToolResult::Text),
        "cargo_doc" => call_doc(args, on_progress).map(ToolResult::Text),
        "cargo_clean" => call_clean(args).map(ToolResult::Text),
        "cargo_update" => call_update(args).map(ToolResult::Text),
        "cargo_fix" => call_fix(args).map(ToolResult::Text),
        "cargo_add" => call_add(args).map(ToolResult::Text),
        "cargo_remove" => call_remove(args).map(ToolResult::Text),
        "cargo_publish" => call_publish(args).map(ToolResult::Text),
        "cargo_setup" => call_setup(args).map(ToolResult::Text),
        "cargo_diagnostic" => call_diagnostic(args).map(ToolResult::Text),
        _ => Err(format!("unknown tool: {name}").into()),
    }
}

// ── tool implementations ──────────────────────────────────────────────────────

fn call_metadata(args: &Value) -> Result<String, Box<dyn std::error::Error>> {
    let wd = opt_str(args, "working_dir");
    let output_file = opt_str(args, "output_file").map(String::from);
    let mut argv: Vec<&str> = vec!["metadata", "--format-version=1"];
    if opt_bool(args, "no_deps") {
        argv.push("--no-deps");
    }

    if let Some(ref path) = output_file {
        // ── streaming path: stdout piped directly to the file ────────────────
        // Validate the path *before* spawning so we never create a partial file
        // that would then be rejected mid-run.
        //
        // Constrain to relative paths under the working directory — an AI agent
        // could otherwise be tricked via prompt injection into overwriting
        // arbitrary user files (e.g. /home/user/.ssh/authorized_keys).
        let pb = std::path::Path::new(path);
        if pb.is_absolute() {
            return Err(
                "output_file must be a relative path; absolute paths are not permitted".into(),
            );
        }
        if pb
            .components()
            .any(|c| c == std::path::Component::ParentDir)
        {
            return Err("output_file must not contain '..' path traversal components".into());
        }
        // Parent directory must already exist; we never create new directories.
        if let Some(parent) = pb.parent()
            && !parent.as_os_str().is_empty()
            && !parent.exists()
        {
            return Err(format!(
                "output_file parent directory does not exist: {}",
                parent.display()
            )
            .into());
        }
        // Create the destination file and hand it to the subprocess as its
        // stdout fd. The OS pipes cargo's output straight to disk without
        // buffering the JSON blob in the server's heap — the whole point of
        // the output_file escape hatch for large workspaces.
        let dest = std::fs::File::create(path)?;
        let out = invoke::run_cargo_to_file(&argv, wd, dest, None)?;
        if out.exit_code != 0 {
            // Best-effort cleanup: remove the partial/empty file on failure.
            let _ = std::fs::remove_file(path);
            return Err(format!(
                "cargo metadata failed (exit {}): {}",
                out.exit_code,
                out.stderr.trim()
            )
            .into());
        }
        let file_size = std::fs::metadata(path)?.len();
        Ok(format!("Metadata written to {path} ({file_size} bytes)"))
    } else {
        // ── buffered path: return the JSON directly in the tool result ────────
        let out = invoke::run_cargo(&argv, wd)?;
        if out.exit_code != 0 {
            return Err(format!(
                "cargo metadata failed (exit {}): {}",
                out.exit_code,
                out.stderr.trim()
            )
            .into());
        }
        Ok(out.stdout)
    }
}

fn call_check(
    args: &Value,
    on_progress: Option<&mut dyn FnMut(&str)>,
) -> Result<ToolResult, Box<dyn std::error::Error>> {
    let wd = opt_str(args, "working_dir");
    let mut argv: Vec<&str> = vec!["check", "--message-format=json"];
    let o = CommonOpts::from_args(args);
    push_package_selection(&mut argv, args, &o);
    push_target_selection(&mut argv, args, &o);
    push_feature_flags(&mut argv, args, &o);
    push_compilation_options(&mut argv, args, &o, true);
    push_manifest_options(&mut argv, args, &o, true);
    let out = run_cargo_maybe_streaming(&argv, wd, opt_timeout(args)?, None, on_progress)?;
    let output = format_json_output(&out, &argv, wd);
    let suggestions = suggest::extract_suggestions(&out.stdout);
    Ok(ToolResult::WithSuggestions {
        output,
        suggestions,
    })
}

fn call_build(
    args: &Value,
    on_progress: Option<&mut dyn FnMut(&str)>,
) -> Result<String, Box<dyn std::error::Error>> {
    let wd = opt_str(args, "working_dir");
    let mut argv: Vec<&str> = vec!["build", "--message-format=json"];
    let o = CommonOpts::from_args(args);
    push_package_selection(&mut argv, args, &o);
    push_target_selection(&mut argv, args, &o);
    push_feature_flags(&mut argv, args, &o);
    push_compilation_options(&mut argv, args, &o, true);
    push_manifest_options(&mut argv, args, &o, true);
    let out = run_cargo_maybe_streaming(&argv, wd, opt_timeout(args)?, None, on_progress)?;
    Ok(format_json_output(&out, &argv, wd))
}

fn call_test(
    args: &Value,
    on_progress: Option<&mut dyn FnMut(&str)>,
) -> Result<String, Box<dyn std::error::Error>> {
    let wd = opt_str(args, "working_dir");
    let mut argv: Vec<&str> = vec!["test", "--message-format=json"];
    let o = CommonOpts::from_args(args);
    let test_name = opt_str(args, "test_name").map(String::from);
    push_package_selection(&mut argv, args, &o);
    // `cargo test` supports the full target-selection set (including --test,
    // handled by push_target_selection) plus --doc for doctests.
    push_target_selection(&mut argv, args, &o);
    if opt_bool(args, "doc") {
        argv.push("--doc");
    }
    if opt_bool(args, "no_run") {
        argv.push("--no-run");
    }
    if opt_bool(args, "no_fail_fast") {
        argv.push("--no-fail-fast");
    }
    push_feature_flags(&mut argv, args, &o);
    // `cargo test` accepts every compilation flag except --keep-going.
    push_compilation_options(&mut argv, args, &o, false);
    push_manifest_options(&mut argv, args, &o, true);
    // Test name filter goes after `--` to the test harness.
    if test_name.is_some() || opt_bool(args, "exact") {
        argv.push("--");
        if let Some(ref name) = test_name {
            argv.push(name);
        }
        if opt_bool(args, "exact") {
            argv.push("--exact");
        }
    }
    // Caller-supplied timeout wins; fall back to the server-configured default
    // (cargo-mcp.test.timeoutSecs VS Code setting, default 30s).
    // opt_timeout_explicit distinguishes three cases:
    //   None         → key absent: apply server default
    //   Some(None)   → explicit 0: disable timeout for this run
    //   Some(Some(d))→ explicit positive: use caller's budget
    let timeout = match opt_timeout_explicit(args)? {
        None => default_test_timeout(), // use server default
        Some(explicit) => explicit,     // caller wins (including None=disable)
    };
    // Arm the timeout only once compilation/linking finishes (cargo emits the
    // `build-finished` record), so the budget bounds test *execution* and not
    // the build phase.
    let out = run_cargo_maybe_streaming(
        &argv,
        wd,
        timeout,
        Some(&is_build_finished_line),
        on_progress,
    )?;
    // Test output is a mix: JSON from compilation, text from the test harness.
    // Use format_test_output so that non-JSON lines (libtest harness text,
    // captured println! replays) are preserved as x-cargo-mcp-test-output
    // records, and stderr (eprintln! from test code) is always included.
    Ok(format_test_output(&out, &argv, wd))
}

fn call_clippy(
    args: &Value,
    on_progress: Option<&mut dyn FnMut(&str)>,
) -> Result<ToolResult, Box<dyn std::error::Error>> {
    let wd = opt_str(args, "working_dir");
    let mut argv: Vec<&str> = vec!["clippy", "--message-format=json"];
    let o = CommonOpts::from_args(args);
    push_package_selection(&mut argv, args, &o);
    push_target_selection(&mut argv, args, &o);
    push_feature_flags(&mut argv, args, &o);
    push_compilation_options(&mut argv, args, &o, true);
    push_manifest_options(&mut argv, args, &o, true);
    let out = run_cargo_maybe_streaming(&argv, wd, opt_timeout(args)?, None, on_progress)?;
    let output = format_json_output(&out, &argv, wd);
    let suggestions = suggest::extract_suggestions(&out.stdout);
    Ok(ToolResult::WithSuggestions {
        output,
        suggestions,
    })
}

fn call_fmt_check(args: &Value) -> Result<String, Box<dyn std::error::Error>> {
    let wd = opt_str(args, "working_dir");
    let mut argv: Vec<&str> = vec!["fmt", "--check"];
    let pkg = opt_str(args, "package").map(String::from);
    if let Some(ref p) = pkg {
        argv.push("--package");
        argv.push(p);
    }
    let out = invoke::run_cargo(&argv, wd)?;
    Ok(format_text_output(&out, &argv, wd))
}

fn call_fmt(args: &Value) -> Result<String, Box<dyn std::error::Error>> {
    let wd = opt_str(args, "working_dir");
    let mut argv: Vec<&str> = vec!["fmt"];
    let pkg = opt_str(args, "package").map(String::from);
    if let Some(ref p) = pkg {
        argv.push("--package");
        argv.push(p);
    }
    let out = invoke::run_cargo(&argv, wd)?;
    Ok(format_text_output(&out, &argv, wd))
}

fn call_tree(args: &Value) -> Result<String, Box<dyn std::error::Error>> {
    let wd = opt_str(args, "working_dir");
    let mut argv: Vec<&str> = vec!["tree"];
    let o = CommonOpts::from_args(args);
    let invert = opt_str(args, "invert").map(String::from);
    let depth_val: String;
    push_package_selection(&mut argv, args, &o);
    if let Some(ref i) = invert {
        argv.push("--invert");
        argv.push(i);
    }
    if opt_bool(args, "duplicates") {
        argv.push("--duplicates");
    }
    if let Some(d) = args.get("depth").and_then(|v| v.as_i64()) {
        depth_val = d.to_string();
        argv.push("--depth");
        argv.push(&depth_val);
    }
    push_feature_flags(&mut argv, args, &o);
    // `cargo tree` supports only --target from the compilation group.
    if let Some(ref t) = o.target {
        argv.push("--target");
        argv.push(t);
    }
    // `cargo tree` has no --ignore-rust-version.
    push_manifest_options(&mut argv, args, &o, false);
    let out = invoke::run_cargo(&argv, wd)?;
    Ok(format_text_output(&out, &argv, wd))
}

fn call_doc(
    args: &Value,
    on_progress: Option<&mut dyn FnMut(&str)>,
) -> Result<String, Box<dyn std::error::Error>> {
    let wd = opt_str(args, "working_dir");
    let mut argv: Vec<&str> = vec!["doc", "--message-format=json"];
    let o = CommonOpts::from_args(args);
    push_package_selection(&mut argv, args, &o);
    // `cargo doc` supports only a subset of target selection.
    push_doc_target_selection(&mut argv, args, &o);
    if opt_bool(args, "no_deps") {
        argv.push("--no-deps");
    }
    if opt_bool(args, "document_private_items") {
        argv.push("--document-private-items");
    }
    push_feature_flags(&mut argv, args, &o);
    push_compilation_options(&mut argv, args, &o, true);
    push_manifest_options(&mut argv, args, &o, true);
    let out = run_cargo_maybe_streaming(&argv, wd, None, None, on_progress)?;
    Ok(format_json_output(&out, &argv, wd))
}

fn call_clean(args: &Value) -> Result<String, Box<dyn std::error::Error>> {
    let wd = opt_str(args, "working_dir");
    let mut argv: Vec<&str> = vec!["clean"];
    let pkg = opt_str(args, "package").map(String::from);
    if let Some(ref p) = pkg {
        argv.push("--package");
        argv.push(p);
    }
    if opt_bool(args, "release") {
        argv.push("--release");
    }
    let out = invoke::run_cargo(&argv, wd)?;
    Ok(format_text_output(&out, &argv, wd))
}

fn call_update(args: &Value) -> Result<String, Box<dyn std::error::Error>> {
    let wd = opt_str(args, "working_dir");
    let mut argv: Vec<&str> = vec!["update"];
    let pkg = opt_str(args, "package").map(String::from);
    let precise = opt_str(args, "precise").map(String::from);
    if precise.is_some() && pkg.is_none() {
        return Err("`precise` requires `package` — specify the crate name to pin".into());
    }
    if let Some(ref p) = pkg {
        argv.push("--package");
        argv.push(p);
    }
    if let Some(ref v) = precise {
        argv.push("--precise");
        argv.push(v);
    }
    let out = invoke::run_cargo(&argv, wd)?;
    Ok(format_text_output(&out, &argv, wd))
}

fn call_fix(args: &Value) -> Result<String, Box<dyn std::error::Error>> {
    let wd = opt_str(args, "working_dir");
    let mut argv: Vec<&str> = vec!["fix"];
    let pkg = opt_str(args, "package").map(String::from);
    if let Some(ref p) = pkg {
        argv.push("--package");
        argv.push(p);
    }
    if opt_bool(args, "allow_dirty") {
        argv.push("--allow-dirty");
    }
    if opt_bool(args, "allow_staged") {
        argv.push("--allow-staged");
    }
    if opt_bool(args, "clippy") {
        argv.push("--clippy");
    }
    let out = invoke::run_cargo(&argv, wd)?;
    Ok(format_text_output(&out, &argv, wd))
}

fn call_add(args: &Value) -> Result<String, Box<dyn std::error::Error>> {
    let wd = opt_str(args, "working_dir");
    let dep = args
        .get("dependency")
        .and_then(|v| v.as_str())
        .ok_or("cargo_add: `dependency` is required")?;
    let mut argv: Vec<&str> = vec!["add", dep];
    let pkg = opt_str(args, "package").map(String::from);
    let features = opt_str(args, "features").map(String::from);
    if opt_bool(args, "dev") {
        argv.push("--dev");
    }
    if opt_bool(args, "build") {
        argv.push("--build");
    }
    if let Some(ref p) = pkg {
        argv.push("--package");
        argv.push(p);
    }
    if let Some(ref f) = features {
        argv.push("--features");
        argv.push(f);
    }
    let out = invoke::run_cargo(&argv, wd)?;
    Ok(format_text_output(&out, &argv, wd))
}

fn call_remove(args: &Value) -> Result<String, Box<dyn std::error::Error>> {
    let wd = opt_str(args, "working_dir");
    let dep = args
        .get("dependency")
        .and_then(|v| v.as_str())
        .ok_or("cargo_remove: `dependency` is required")?;
    let mut argv: Vec<&str> = vec!["remove", dep];
    let pkg = opt_str(args, "package").map(String::from);
    if opt_bool(args, "dev") {
        argv.push("--dev");
    }
    if opt_bool(args, "build") {
        argv.push("--build");
    }
    if let Some(ref p) = pkg {
        argv.push("--package");
        argv.push(p);
    }
    let out = invoke::run_cargo(&argv, wd)?;
    Ok(format_text_output(&out, &argv, wd))
}

fn call_publish(args: &Value) -> Result<String, Box<dyn std::error::Error>> {
    let wd = opt_str(args, "working_dir");
    let mut argv: Vec<&str> = vec!["publish"];
    let pkg = opt_str(args, "package").map(String::from);
    if let Some(ref p) = pkg {
        argv.push("--package");
        argv.push(p);
    }
    if opt_bool(args, "dry_run") {
        argv.push("--dry-run");
    }
    if opt_bool(args, "allow_dirty") {
        argv.push("--allow-dirty");
    }
    let out = invoke::run_cargo(&argv, wd)?;
    Ok(format_text_output(&out, &argv, wd))
}

fn call_setup(_args: &Value) -> Result<String, Box<dyn std::error::Error>> {
    Ok(format!(
        "Add the following section to the appropriate Copilot instruction file \
         in this repository. Adapt the wording to fit the project's existing \
         conventions — the meaning matters, not the exact phrasing.\
         \n\n```markdown\n{}```",
        CARGO_MCP_INSTRUCTIONS
    ))
}

/// Build a structured diagnostic report about cargo/rustc resolution.
///
/// The report is intended for users investigating "wrong toolchain" problems.
/// It captures, in one shot, every piece of state cargo-mcp uses to decide
/// which `cargo` to invoke. No fields are masked — none are secret.
fn call_diagnostic(args: &Value) -> Result<String, Box<dyn std::error::Error>> {
    use std::process::Command;

    let wd_owned = match opt_str(args, "working_dir") {
        Some(s) => std::path::PathBuf::from(s),
        None => std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from(".")),
    };

    let (cargo_path, cargo_source) = invoke::resolve_cargo_binary();
    let (rustc_path, rustc_source) = invoke::resolve_rustc_binary();

    // Run `cargo --version --verbose`. Capture failure as a string instead of
    // failing the diagnostic — the whole point is to report the truth.
    let cargo_version = match Command::new(&cargo_path)
        .args(["--version", "--verbose"])
        .current_dir(&wd_owned)
        .output()
    {
        Ok(o) => serde_json::json!({
            "exit_code": o.status.code().unwrap_or(-1),
            "stdout": String::from_utf8_lossy(&o.stdout).to_string(),
            "stderr": String::from_utf8_lossy(&o.stderr).to_string(),
        }),
        Err(e) => serde_json::json!({ "error": e.to_string() }),
    };
    let rustc_version = match Command::new(&rustc_path)
        .args(["--version", "--verbose"])
        .current_dir(&wd_owned)
        .output()
    {
        Ok(o) => serde_json::json!({
            "exit_code": o.status.code().unwrap_or(-1),
            "stdout": String::from_utf8_lossy(&o.stdout).to_string(),
            "stderr": String::from_utf8_lossy(&o.stderr).to_string(),
        }),
        Err(e) => serde_json::json!({ "error": e.to_string() }),
    };

    let toolchain_file = invoke::find_toolchain_file(&wd_owned);
    let toolchain_contents = toolchain_file
        .as_ref()
        .and_then(|p| std::fs::read_to_string(p).ok());

    fn env_or_unset(key: &str) -> Value {
        match std::env::var(key) {
            Ok(v) => Value::String(v),
            Err(_) => Value::Null,
        }
    }

    let report = serde_json::json!({
        "working_dir": wd_owned.display().to_string(),
        "cargo": {
            "path": cargo_path.display().to_string(),
            "source": format!("{:?}", cargo_source),
            "step": cargo_source.step(),
            "version": cargo_version,
        },
        "rustc": {
            "path": rustc_path.display().to_string(),
            "source": format!("{:?}", rustc_source),
            "step": rustc_source.step(),
            "version": rustc_version,
        },
        "toolchain_file": {
            "path": toolchain_file.as_ref().map(|p| p.display().to_string()),
            "contents": toolchain_contents,
        },
        "env": {
            "PATH": env_or_unset("PATH"),
            "CARGO": env_or_unset("CARGO"),
            "RUSTC": env_or_unset("RUSTC"),
            "RUSTUP_TOOLCHAIN": env_or_unset("RUSTUP_TOOLCHAIN"),
            "RUSTUP_HOME": env_or_unset("RUSTUP_HOME"),
            "CARGO_HOME": env_or_unset("CARGO_HOME"),
        },
    });

    Ok(serde_json::to_string_pretty(&report)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_output(exit_code: i32, stdout: &str, stderr: &str) -> CargoOutput {
        CargoOutput {
            exit_code,
            stdout: stdout.to_owned(),
            stderr: stderr.to_owned(),
        }
    }

    /// Every non-blank line of a JSON-mode failure output must parse as a
    /// JSON object. Guards against regressions where a non-NDJSON appendix
    /// (e.g. a bare `[stderr]` sentinel) is reintroduced.
    fn assert_pure_ndjson(formatted: &str) {
        for (i, line) in formatted.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            serde_json::from_str::<Value>(line)
                .unwrap_or_else(|e| panic!("line {} is not JSON: {line:?} ({e})", i + 1));
        }
    }

    #[test]
    fn format_json_output_surfaces_stderr_holder_report_on_failure() {
        // Real-shape NDJSON line so filter_build_ndjson keeps it.
        let stdout = r#"{"reason":"compiler-message","package_id":"foo 0.1.0","target":{"name":"foo"},"message":{"rendered":"error: linking with `link.exe` failed"}}"#;
        let stderr = "Holders for `target\\debug\\foo.exe`:\n    PID 1234 - rm-hold-file.exe (C:\\path\\to\\rm-hold-file.exe) [console]\n";
        let out = fake_output(101, stdout, stderr);
        let formatted = format_json_output(&out, &["build"], None);
        assert_pure_ndjson(&formatted);
        assert!(
            formatted.contains("rm-hold-file.exe"),
            "stderr holder report missing from formatted output:\n{formatted}"
        );
        assert!(
            formatted.contains(STDERR_REASON),
            "expected stderr NDJSON record (reason={STDERR_REASON}); got:\n{formatted}"
        );
        assert!(
            formatted.contains(r#""status":"error""#),
            "status trailer missing:\n{formatted}"
        );
    }

    #[test]
    fn format_json_output_omits_stderr_record_when_empty() {
        let stdout = r#"{"reason":"compiler-message","package_id":"foo 0.1.0","target":{"name":"foo"},"message":{"rendered":"error"}}"#;
        let out = fake_output(101, stdout, "");
        let formatted = format_json_output(&out, &["build"], None);
        assert_pure_ndjson(&formatted);
        assert!(
            !formatted.contains(STDERR_REASON),
            "should not emit stderr NDJSON record when stderr is empty:\n{formatted}"
        );
    }

    #[test]
    fn format_json_output_success_omits_stderr_record() {
        let stdout =
            r#"{"reason":"compiler-artifact","package_id":"foo 0.1.0","target":{"name":"foo"}}"#;
        let out = fake_output(0, stdout, "noisy progress on stderr\n");
        let formatted = format_json_output(&out, &["build"], None);
        assert_pure_ndjson(&formatted);
        assert!(
            !formatted.contains(STDERR_REASON),
            "success path must not append stderr record:\n{formatted}"
        );
    }

    /// The header must be a single, parseable JSON line ending in `\n`,
    /// with the documented `reason` discriminator and the argv/cwd echoed
    /// back so consumers can scan the output as pure NDJSON.
    #[test]
    fn invocation_header_is_valid_ndjson_record() {
        let h = invocation_header(
            &["build", "--message-format=json", "--all-targets"],
            Some(r"C:\path\to\workspace"),
        );
        assert!(h.ends_with('\n'), "header must end in newline: {h:?}");
        assert_eq!(
            h.matches('\n').count(),
            1,
            "header must be exactly one line (got {h:?})"
        );
        let v: Value = serde_json::from_str(h.trim_end()).expect("header is valid JSON");
        assert_eq!(v["reason"], INVOCATION_REASON);
        assert_eq!(v["reason"], "x-cargo-mcp-invocation");
        assert_eq!(
            v["argv"],
            serde_json::json!(["build", "--message-format=json", "--all-targets"])
        );
        assert_eq!(v["cwd"], r"C:\path\to\workspace");
    }

    /// When no working directory was supplied, `cwd` defaults to `"."` so
    /// the field is always present and the consumer never has to special-case
    /// a missing key.
    #[test]
    fn invocation_header_defaults_cwd_to_dot() {
        let h = invocation_header(&["fmt", "--check"], None);
        let v: Value = serde_json::from_str(h.trim_end()).unwrap();
        assert_eq!(v["cwd"], ".");
    }

    // ── opt_timeout_explicit + default_test_timeout tests ────────────────────

    use std::sync::Mutex;
    /// Serializes tests that read/write the process-global DEFAULT_TEST_TIMEOUT_SECS.
    static DEFAULT_TIMEOUT_TEST_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn opt_timeout_explicit_absent_returns_none() {
        let args = serde_json::json!({});
        assert!(matches!(opt_timeout_explicit(&args), Ok(None)));
    }

    #[test]
    fn opt_timeout_explicit_null_returns_none() {
        let args = serde_json::json!({"timeout_secs": null});
        assert!(matches!(opt_timeout_explicit(&args), Ok(None)));
    }

    #[test]
    fn opt_timeout_explicit_zero_returns_some_none() {
        let args = serde_json::json!({"timeout_secs": 0});
        assert!(matches!(opt_timeout_explicit(&args), Ok(Some(None))));
    }

    #[test]
    fn opt_timeout_explicit_positive_returns_duration() {
        let args = serde_json::json!({"timeout_secs": 30});
        match opt_timeout_explicit(&args) {
            Ok(Some(Some(d))) => assert_eq!(d, std::time::Duration::from_secs(30)),
            other => panic!("expected Ok(Some(Some(30s))), got {other:?}"),
        }
    }

    #[test]
    fn opt_timeout_explicit_invalid_returns_err() {
        let args = serde_json::json!({"timeout_secs": "thirty"});
        assert!(opt_timeout_explicit(&args).is_err());
    }

    /// Verify the three-state timeout selection used by `call_test`:
    /// absent → server default; explicit 0 → no timeout; positive → caller budget.
    #[test]
    fn call_test_timeout_selection_covers_all_three_states() {
        let _g = DEFAULT_TIMEOUT_TEST_LOCK.lock().unwrap();
        set_default_test_timeout(Some(30));

        // State 1: absent → server default applies
        let absent = serde_json::json!({});
        let t = match opt_timeout_explicit(&absent).unwrap() {
            None => default_test_timeout(),
            Some(explicit) => explicit,
        };
        assert_eq!(t, Some(std::time::Duration::from_secs(30)));

        // State 2: explicit 0 → no timeout, even with server default set
        let zero = serde_json::json!({"timeout_secs": 0});
        let t = match opt_timeout_explicit(&zero).unwrap() {
            None => default_test_timeout(),
            Some(explicit) => explicit,
        };
        assert_eq!(t, None);

        // State 3: explicit positive → caller's budget wins over server default
        let sixty = serde_json::json!({"timeout_secs": 60});
        let t = match opt_timeout_explicit(&sixty).unwrap() {
            None => default_test_timeout(),
            Some(explicit) => explicit,
        };
        assert_eq!(t, Some(std::time::Duration::from_secs(60)));

        set_default_test_timeout(None); // restore
    }

    #[test]
    fn is_build_finished_line_matches_cargo_record() {
        // The exact compact JSON cargo emits with --message-format=json.
        assert!(is_build_finished_line(
            r#"{"reason":"build-finished","success":true}"#
        ));
        assert!(is_build_finished_line(
            r#"{"reason":"build-finished","success":false}"#
        ));
    }

    #[test]
    fn is_build_finished_line_rejects_other_records() {
        assert!(!is_build_finished_line(
            r#"{"reason":"compiler-message","message":{}}"#
        ));
        assert!(!is_build_finished_line(r#"{"reason":"compiler-artifact"}"#));
        assert!(!is_build_finished_line("running 3 tests"));
        assert!(!is_build_finished_line(""));
    }

    #[test]
    fn push_feature_flags_emits_selected_flags() {
        // --features takes the comma-separated value; the booleans are bare.
        let args = serde_json::json!({
            "features": "foo,bar",
            "all_features": true,
            "no_default_features": true,
        });
        let o = CommonOpts::from_args(&args);
        let mut argv: Vec<&str> = vec!["build"];
        push_feature_flags(&mut argv, &args, &o);
        assert_eq!(
            argv,
            vec![
                "build",
                "--features",
                "foo,bar",
                "--all-features",
                "--no-default-features",
            ]
        );
    }

    #[test]
    fn push_feature_flags_omits_absent_flags() {
        let args = serde_json::json!({});
        let o = CommonOpts::from_args(&args);
        let mut argv: Vec<&str> = vec!["check"];
        push_feature_flags(&mut argv, &args, &o);
        assert_eq!(argv, vec!["check"]);
    }

    #[test]
    fn push_target_selection_emits_all_flags() {
        let args = serde_json::json!({
            "lib": true,
            "bins": true,
            "bin": "mybin",
            "examples": true,
            "example": "myex",
            "tests": true,
            "test": "mytest",
            "benches": true,
            "bench": "mybench",
            "all_targets": true,
        });
        let o = CommonOpts::from_args(&args);
        let mut argv: Vec<&str> = vec!["check"];
        push_target_selection(&mut argv, &args, &o);
        assert_eq!(
            argv,
            vec![
                "check",
                "--lib",
                "--bins",
                "--bin",
                "mybin",
                "--examples",
                "--example",
                "myex",
                "--tests",
                "--test",
                "mytest",
                "--benches",
                "--bench",
                "mybench",
                "--all-targets",
            ]
        );
    }

    #[test]
    fn push_compilation_options_gates_keep_going() {
        // keep_going requested but the subcommand does not support it (false):
        // the flag must be suppressed.
        let args = serde_json::json!({
            "release": true,
            "profile": "dist",
            "jobs": 4,
            "keep_going": true,
            "target": "x86_64-unknown-linux-gnu",
            "target_dir": "out",
            "timings": true,
        });
        let o = CommonOpts::from_args(&args);
        let mut without = vec!["test"];
        push_compilation_options(&mut without, &args, &o, false);
        assert!(!without.contains(&"--keep-going"));
        assert!(without.contains(&"--release"));
        assert_eq!(without.iter().filter(|a| **a == "--jobs").count(), 1);

        let mut with = vec!["build"];
        push_compilation_options(&mut with, &args, &o, true);
        assert!(with.contains(&"--keep-going"));
    }

    #[test]
    fn push_manifest_options_gates_ignore_rust_version() {
        let args = serde_json::json!({
            "manifest_path": "Cargo.toml",
            "ignore_rust_version": true,
            "locked": true,
            "offline": true,
            "frozen": true,
        });
        let o = CommonOpts::from_args(&args);
        let mut without = vec!["tree"];
        push_manifest_options(&mut without, &args, &o, false);
        assert!(!without.contains(&"--ignore-rust-version"));
        assert!(without.contains(&"--locked"));
        assert!(without.contains(&"--offline"));
        assert!(without.contains(&"--frozen"));

        let mut with = vec!["check"];
        push_manifest_options(&mut with, &args, &o, true);
        assert!(with.contains(&"--ignore-rust-version"));
    }
}
