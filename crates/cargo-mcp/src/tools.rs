// Copyright (c) Michael Grier. All rights reserved.

//! Tool definitions and dispatch for the `cargo-mcp` MCP server.
//!
//! Each tool invokes `cargo` as a subprocess via [`crate::invoke::run_cargo`].
//! The server is a thin dispatch layer — all build logic lives in Cargo.
//!
//! ## Tool set
//!
//! - `cargo_metadata`  — project structure and dependency info (JSON)
//! - `cargo_check`     — fast error checking without producing binaries
//! - `cargo_build`     — compile the project
//! - `cargo_test`      — run tests with optional filters
//! - `cargo_clippy`    — run lint checks
//! - `cargo_fmt_check` — check formatting without modifying files
//! - `cargo_fmt`       — format source code
//! - `cargo_tree`      — display dependency tree
//! - `cargo_doc`       — build documentation
//! - `cargo_clean`     — remove build artefacts
//! - `cargo_update`    — update Cargo.lock
//! - `cargo_fix`       — auto-apply compiler fixes
//! - `cargo_add`       — add a dependency
//! - `cargo_remove`    — remove a dependency
//! - `cargo_publish`   — publish to crates.io
//! - `cargo_setup`     — check/create .github/copilot-instructions.md

use serde_json::Value;

use crate::{
    invoke::{self, CargoOutput},
    suggest::{self, Suggestion},
};

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
| `cargo_setup` | *(no terminal equivalent)* |\n";

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

/// Format a [`CargoOutput`] from a `--message-format=json` invocation.
///
/// Filters the NDJSON stream to remove dep-artifact and build-script noise
/// (already delivered as streaming progress), then returns the remainder.
/// On failure, prepends the exit code and appends stderr for context.
fn format_json_output(out: &CargoOutput) -> String {
    if out.exit_code == 0 {
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
    }
}

/// Format a [`CargoOutput`] from a command with no JSON mode (fmt, tree, clean).
///
/// Combines stdout and stderr into a single text block.
fn format_text_output(out: &CargoOutput) -> String {
    let combined = if out.stderr.is_empty() {
        out.stdout.clone()
    } else if out.stdout.is_empty() {
        out.stderr.clone()
    } else {
        format!("{}\n{}", out.stdout, out.stderr)
    };
    if out.exit_code == 0 {
        if combined.is_empty() {
            "(success, no output)".to_owned()
        } else {
            combined
        }
    } else {
        format!("(exit code {})\n{}", out.exit_code, combined)
    }
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
                 of reading Cargo.toml files manually.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "working_dir": {
                        "type": "string",
                        "description":
                            "Absolute path to the directory containing the Cargo.toml \
                             (or a workspace member). Defaults to the current directory."
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
                 spans, error codes, and message text that you can act on directly.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "working_dir": {
                        "type": "string",
                        "description":
                            "Absolute path to the directory containing the Cargo.toml. \
                             Defaults to the current directory."
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
                 error-only checking when binaries are not needed.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "working_dir": {
                        "type": "string",
                        "description":
                            "Absolute path to the directory containing the Cargo.toml. \
                             Defaults to the current directory."
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
                 failures with no_fail_fast.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "working_dir": {
                        "type": "string",
                        "description":
                            "Absolute path to the directory containing the Cargo.toml. \
                             Defaults to the current directory."
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
                 source to catch non-idiomatic patterns and common mistakes.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "working_dir": {
                        "type": "string",
                        "description":
                            "Absolute path to the directory containing the Cargo.toml. \
                             Defaults to the current directory."
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
                 formatted. Use this to check formatting before using cargo_fmt to fix it.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "working_dir": {
                        "type": "string",
                        "description":
                            "Absolute path to the directory containing the Cargo.toml. \
                             Defaults to the current directory."
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
                 formatting.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "working_dir": {
                        "type": "string",
                        "description":
                            "Absolute path to the directory containing the Cargo.toml. \
                             Defaults to the current directory."
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
                 cargo_metadata instead.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "working_dir": {
                        "type": "string",
                        "description":
                            "Absolute path to the directory containing the Cargo.toml. \
                             Defaults to the current directory."
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
                 diagnostics for any warnings or errors encountered during the build.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "working_dir": {
                        "type": "string",
                        "description":
                            "Absolute path to the directory containing the Cargo.toml. \
                             Defaults to the current directory."
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
                 on the next build command. Use when builds are in an inconsistent state.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "working_dir": {
                        "type": "string",
                        "description":
                            "Absolute path to the directory containing the Cargo.toml. \
                             Defaults to the current directory."
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
                 compatible dependency updates.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "working_dir": {
                        "type": "string",
                        "description":
                            "Absolute path to the directory containing the Cargo.toml. \
                             Defaults to the current directory."
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
                 to apply safe fixes in bulk. Returns plain text output.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "working_dir": {
                        "type": "string",
                        "description":
                            "Absolute path to the directory containing the Cargo.toml. \
                             Defaults to the current directory."
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
                 the `version` parameter or let Cargo choose the latest compatible release.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "working_dir": {
                        "type": "string",
                        "description":
                            "Absolute path to the directory containing the Cargo.toml. \
                             Defaults to the current directory."
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
                 Cargo.toml and updates Cargo.lock.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "working_dir": {
                        "type": "string",
                        "description":
                            "Absolute path to the directory containing the Cargo.toml. \
                             Defaults to the current directory."
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
                 validate the package before publishing for real.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "working_dir": {
                        "type": "string",
                        "description":
                            "Absolute path to the directory containing the Cargo.toml. \
                             Defaults to the current directory."
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
    let out = invoke::run_cargo(&argv, wd)?;
    if out.exit_code != 0 {
        return Err(format!(
            "cargo metadata failed (exit {}): {}",
            out.exit_code,
            out.stderr.trim()
        )
        .into());
    }
    if let Some(ref path) = output_file {
        // Constrain to relative paths under the working directory — an AI agent
        // could otherwise be tricked via prompt injection into overwriting
        // arbitrary user files (e.g. /home/user/.ssh/authorized_keys).
        let pb = std::path::Path::new(path);
        if pb.is_absolute() {
            return Err(
                "output_file must be a relative path; absolute paths are not permitted".into(),
            );
        }
        if pb.components().any(|c| c == std::path::Component::ParentDir) {
            return Err(
                "output_file must not contain '..' path traversal components".into(),
            );
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
        std::fs::write(path, &out.stdout)?;
        Ok(format!(
            "Metadata written to {path} ({} bytes)",
            out.stdout.len()
        ))
    } else {
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
    let output = format_json_output(&out);
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
    Ok(format_json_output(&out))
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
    Ok(format_json_output(&out))
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
    let output = format_json_output(&out);
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
    Ok(format_text_output(&out))
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
    Ok(format_text_output(&out))
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
    Ok(format_text_output(&out))
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
    Ok(format_json_output(&out))
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
    Ok(format_text_output(&out))
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
    Ok(format_text_output(&out))
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
    Ok(format_text_output(&out))
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
    Ok(format_text_output(&out))
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
    Ok(format_text_output(&out))
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
    Ok(format_text_output(&out))
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
