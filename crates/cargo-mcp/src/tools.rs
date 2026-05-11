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
//! [`invocation_header`], shaped to look like another cargo NDJSON record so
//! consumers can parse the entire response with a single line-by-line JSON
//! parser:
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
//! even when the original `arguments` JSON is sparse, without breaking
//! NDJSON parsing in clients that scan for cargo's own `reason` discriminator.

use serde_json::Value;

use crate::{
    invoke::{self, CargoOutput},
    suggest::{self, Suggestion},
};

use std::sync::{Arc, atomic::AtomicBool};

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
| `cargo_diagnostic` | *(no terminal equivalent)* |\n";

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

/// Filter `--message-format=json` NDJSON output to keep only actionable lines.
///
/// Retains only `compiler-message` lines (errors and warnings) and the
/// `build-finished` summary. Everything else — artifacts, build-script events,
/// etc. — was already surfaced via streaming progress notifications and is not
/// useful in the final response.
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
                true // keep non-JSON lines (test harness events etc.)
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

/// Format a [`CargoOutput`] from a `--message-format=json` invocation.
///
/// Filters the NDJSON stream to remove dep-artifact and build-script noise
/// (already delivered as streaming progress), then returns the remainder.
/// On failure, prepends the exit code and appends stderr for context.
/// The result is prefixed with [`invocation_header`].
fn format_json_output(out: &CargoOutput, argv: &[&str], wd: Option<&str>) -> String {
    let header = invocation_header(argv, wd);
    let body = if out.exit_code == 0 {
        if out.stdout.is_empty() {
            r#"{"status":"success"}"#.to_owned()
        } else {
            filter_build_ndjson(&out.stdout)
        }
    } else if out.stdout.is_empty() {
        // No JSON at all — stderr has the error (e.g. bad args, missing toolchain).
        format!(
            r#"{{"status":"error","exit_code":{},"message":{}}}"#,
            out.exit_code,
            serde_json::to_string(out.stderr.trim()).unwrap_or_default(),
        )
    } else {
        // JSON stream contains the diagnostics; append a status trailer.
        let filtered = filter_build_ndjson(&out.stdout);
        format!(
            "{}\n{}",
            filtered.trim_end(),
            format_args!(r#"{{"status":"error","exit_code":{}}}"#, out.exit_code,),
        )
    };
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
    on_progress: Option<&mut dyn FnMut(&str)>,
) -> Result<CargoOutput, Box<dyn std::error::Error>> {
    match on_progress {
        Some(cb) => invoke::run_cargo_streaming(argv, wd, cb),
        None => invoke::run_cargo(argv, wd),
    }
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
                    "features": {
                        "type": "string",
                        "description":
                            "Comma-separated list of features to activate. \
                             Omit to use default features."
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
                    "features": {
                        "type": "string",
                        "description":
                            "Comma-separated list of features to activate. \
                             Omit to use default features."
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
            "name": "cargo_test",
            "description":
                "ALWAYS use this tool instead of running `cargo test` in a terminal \
                 when working in a Rust/Cargo project. Executes the project's test \
                 suite and returns compilation diagnostics as structured NDJSON plus \
                 the test harness output (pass/fail counts and failure details). \
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
                    "features": {
                        "type": "string",
                        "description":
                            "Comma-separated list of features to activate. \
                             Omit to use default features."
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
                    "features": {
                        "type": "string",
                        "description":
                            "Comma-separated list of features to activate. \
                             Omit to use default features."
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
                    "features": {
                        "type": "string",
                        "description":
                            "Comma-separated list of features to activate. \
                             Omit to use default features."
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
        let out = invoke::run_cargo_to_file(&argv, wd, dest)?;
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
    let pkg = opt_str(args, "package").map(String::from);
    let features = opt_str(args, "features").map(String::from);
    if let Some(ref p) = pkg {
        argv.push("--package");
        argv.push(p);
    }
    if opt_bool(args, "release") {
        argv.push("--release");
    }
    if opt_bool(args, "all_targets") {
        argv.push("--all-targets");
    }
    if let Some(ref f) = features {
        argv.push("--features");
        argv.push(f);
    }
    if opt_bool(args, "locked") {
        argv.push("--locked");
    }
    let out = run_cargo_maybe_streaming(&argv, wd, on_progress)?;
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
    let pkg = opt_str(args, "package").map(String::from);
    let features = opt_str(args, "features").map(String::from);
    if let Some(ref p) = pkg {
        argv.push("--package");
        argv.push(p);
    }
    if opt_bool(args, "release") {
        argv.push("--release");
    }
    if opt_bool(args, "all_targets") {
        argv.push("--all-targets");
    }
    if let Some(ref f) = features {
        argv.push("--features");
        argv.push(f);
    }
    if opt_bool(args, "locked") {
        argv.push("--locked");
    }
    let out = run_cargo_maybe_streaming(&argv, wd, on_progress)?;
    Ok(format_json_output(&out, &argv, wd))
}

fn call_test(
    args: &Value,
    on_progress: Option<&mut dyn FnMut(&str)>,
) -> Result<String, Box<dyn std::error::Error>> {
    let wd = opt_str(args, "working_dir");
    let mut argv: Vec<&str> = vec!["test", "--message-format=json"];
    let pkg = opt_str(args, "package").map(String::from);
    let features = opt_str(args, "features").map(String::from);
    let test_target = opt_str(args, "test").map(String::from);
    let test_name = opt_str(args, "test_name").map(String::from);
    if let Some(ref p) = pkg {
        argv.push("--package");
        argv.push(p);
    }
    if opt_bool(args, "release") {
        argv.push("--release");
    }
    if opt_bool(args, "no_fail_fast") {
        argv.push("--no-fail-fast");
    }
    if opt_bool(args, "lib") {
        argv.push("--lib");
    }
    if let Some(ref t) = test_target {
        argv.push("--test");
        argv.push(t);
    }
    if let Some(ref f) = features {
        argv.push("--features");
        argv.push(f);
    }
    if opt_bool(args, "locked") {
        argv.push("--locked");
    }
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
    let out = run_cargo_maybe_streaming(&argv, wd, on_progress)?;
    // Test output is a mix: JSON from compilation, text from the test harness.
    // Return both stdout (JSON + test results) and stderr on failure.
    Ok(format_json_output(&out, &argv, wd))
}

fn call_clippy(
    args: &Value,
    on_progress: Option<&mut dyn FnMut(&str)>,
) -> Result<ToolResult, Box<dyn std::error::Error>> {
    let wd = opt_str(args, "working_dir");
    let mut argv: Vec<&str> = vec!["clippy", "--message-format=json"];
    let pkg = opt_str(args, "package").map(String::from);
    let features = opt_str(args, "features").map(String::from);
    if let Some(ref p) = pkg {
        argv.push("--package");
        argv.push(p);
    }
    if opt_bool(args, "all_targets") {
        argv.push("--all-targets");
    }
    if let Some(ref f) = features {
        argv.push("--features");
        argv.push(f);
    }
    if opt_bool(args, "locked") {
        argv.push("--locked");
    }
    let out = run_cargo_maybe_streaming(&argv, wd, on_progress)?;
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
    let pkg = opt_str(args, "package").map(String::from);
    let features = opt_str(args, "features").map(String::from);
    let invert = opt_str(args, "invert").map(String::from);
    let depth_val: String;
    if let Some(ref p) = pkg {
        argv.push("--package");
        argv.push(p);
    }
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
    if let Some(ref f) = features {
        argv.push("--features");
        argv.push(f);
    }
    let out = invoke::run_cargo(&argv, wd)?;
    Ok(format_text_output(&out, &argv, wd))
}

fn call_doc(
    args: &Value,
    on_progress: Option<&mut dyn FnMut(&str)>,
) -> Result<String, Box<dyn std::error::Error>> {
    let wd = opt_str(args, "working_dir");
    let mut argv: Vec<&str> = vec!["doc", "--message-format=json"];
    let pkg = opt_str(args, "package").map(String::from);
    if let Some(ref p) = pkg {
        argv.push("--package");
        argv.push(p);
    }
    if opt_bool(args, "no_deps") {
        argv.push("--no-deps");
    }
    if opt_bool(args, "document_private_items") {
        argv.push("--document-private-items");
    }
    if opt_bool(args, "locked") {
        argv.push("--locked");
    }
    let out = run_cargo_maybe_streaming(&argv, wd, on_progress)?;
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
}
