// Copyright (c) Michael Grier. All rights reserved.

//! `cargo-mcp` — MCP (Model Context Protocol) server that exposes Cargo's
//! build system capabilities as tools callable by AI agents such as
//! GitHub Copilot.
//!
//! The server speaks JSON-RPC 2.0 over stdio using newline-delimited messages.
//! Each tool invocation spawns `cargo` as a subprocess, capturing stdout and
//! stderr. This keeps the MCP server as a thin dispatch layer — all build
//! logic lives in Cargo itself.

// The tool-definition table in `tools.rs` is built with a single large
// `serde_json::json!` literal; the default recursion limit (128) is not enough
// to expand it once every subcommand advertises its full option set.
#![recursion_limit = "512"]

mod busy_files;
mod elicit;
mod invoke;
mod line_reader;
mod rm;
mod suggest;
mod tools;

use std::io::{self, Write};

use line_reader::LineReader;
use serde::{Deserialize, Serialize};
use serde_json::Value;

// ── Configuration ─────────────────────────────────────────────────────────────

/// Controls how the server handles suggestions that need human approval.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ElicitationMode {
    /// Present a multi-select form to the user.
    Prompt,
    /// Automatically accept all suggestions without prompting.
    AlwaysAccept,
    /// Automatically skip all suggestions without prompting (default).
    AlwaysSkip,
}

impl ElicitationMode {
    fn from_str(s: &str) -> Option<Self> {
        match s {
            "prompt" => Some(Self::Prompt),
            "always-accept" => Some(Self::AlwaysAccept),
            "always-skip" => Some(Self::AlwaysSkip),
            _ => None,
        }
    }
}

/// Parsed startup configuration.
struct StartupConfig {
    elicitation_mode: ElicitationMode,
    progress_delay_ms: u64,
    retry_on_busy: bool,
    retry_delay_ms: u64,
    retry_max_attempts: u32,
    rm_lookup_enabled: bool,
    /// Default wall-clock timeout for `cargo_test` calls that do not supply
    /// an explicit `timeout_secs`. `None` means no default (wait forever).
    test_timeout_secs: Option<u64>,
    warnings: Vec<String>,
}

fn parse_bool_flag(value: &str) -> Option<bool> {
    match value.to_ascii_lowercase().as_str() {
        "true" | "1" | "yes" | "on" => Some(true),
        "false" | "0" | "no" | "off" => Some(false),
        _ => None,
    }
}

fn parse_config() -> StartupConfig {
    let mut cfg = StartupConfig {
        elicitation_mode: ElicitationMode::AlwaysSkip,
        progress_delay_ms: 0,
        retry_on_busy: true,
        retry_delay_ms: 500,
        retry_max_attempts: 3,
        rm_lookup_enabled: false,
        test_timeout_secs: None,
        warnings: Vec::new(),
    };
    for arg in std::env::args_os().skip(1) {
        let s = arg.to_string_lossy();
        if let Some(rest) = s.strip_prefix("--elicitation-mode=") {
            match ElicitationMode::from_str(rest) {
                Some(m) => cfg.elicitation_mode = m,
                None => {
                    cfg.warnings.push(format!(
                        "ignoring invalid --elicitation-mode value: {rest:?} \
                         (expected: prompt, always-accept, always-skip)"
                    ));
                }
            }
        } else if let Some(rest) = s.strip_prefix("--progress-delay-ms=") {
            match rest.parse::<u64>() {
                Ok(n) => cfg.progress_delay_ms = n,
                Err(_) => {
                    cfg.warnings.push(format!(
                        "ignoring invalid --progress-delay-ms value: {rest:?} \
                         (expected a non-negative integer)"
                    ));
                }
            }
        } else if let Some(rest) = s.strip_prefix("--retry-on-busy=") {
            match parse_bool_flag(rest) {
                Some(b) => cfg.retry_on_busy = b,
                None => {
                    cfg.warnings.push(format!(
                        "ignoring invalid --retry-on-busy value: {rest:?} \
                         (expected one of: true/false, 1/0, yes/no, on/off)"
                    ));
                }
            }
        } else if let Some(rest) = s.strip_prefix("--retry-delay-ms=") {
            match rest.parse::<u64>() {
                Ok(n) => cfg.retry_delay_ms = n,
                Err(_) => {
                    cfg.warnings.push(format!(
                        "ignoring invalid --retry-delay-ms value: {rest:?} \
                         (expected a non-negative integer)"
                    ));
                }
            }
        } else if let Some(rest) = s.strip_prefix("--retry-max-attempts=") {
            match rest.parse::<u32>() {
                Ok(n) if n >= 1 => cfg.retry_max_attempts = n,
                _ => {
                    cfg.warnings.push(format!(
                        "ignoring invalid --retry-max-attempts value: {rest:?} \
                         (expected a positive integer)"
                    ));
                }
            }
        } else if let Some(rest) = s.strip_prefix("--unsafe-windows-rm=") {
            match parse_bool_flag(rest) {
                Some(b) => cfg.rm_lookup_enabled = b,
                None => {
                    cfg.warnings.push(format!(
                        "ignoring invalid --unsafe-windows-rm value: {rest:?} \
                         (expected one of: true/false, 1/0, yes/no, on/off)"
                    ));
                }
            }
        } else if let Some(rest) = s.strip_prefix("--test-timeout-secs=") {
            match rest.parse::<u64>() {
                Ok(0) => cfg.test_timeout_secs = None,
                Ok(n) => cfg.test_timeout_secs = Some(n),
                Err(_) => {
                    cfg.warnings.push(format!(
                        "ignoring invalid --test-timeout-secs value: {rest:?} \
                         (expected a non-negative integer; 0 disables the default timeout)"
                    ));
                }
            }
        }
    }
    cfg
}

// ── JSON-RPC 2.0 wire types ───────────────────────────────────────────────────

/// An incoming JSON-RPC 2.0 message (request or notification).
#[derive(Deserialize)]
struct Message {
    #[allow(dead_code)]
    jsonrpc: String,
    /// Absent for notifications; present for requests.
    id: Option<Value>,
    method: String,
    params: Option<Value>,
}

/// An outgoing JSON-RPC 2.0 response.
#[derive(Serialize)]
struct Response {
    jsonrpc: &'static str,
    id: Value,
    #[serde(flatten)]
    body: ResponseBody,
}

#[derive(Serialize)]
#[serde(untagged)]
enum ResponseBody {
    Ok { result: Value },
    Err { error: RpcError },
}

#[derive(Serialize)]
struct RpcError {
    code: i32,
    message: String,
}

/// JSON-RPC 2.0 reserved error codes.
mod code {
    pub const PARSE_ERROR: i32 = -32700;
    pub const METHOD_NOT_FOUND: i32 = -32601;
    pub const INVALID_PARAMS: i32 = -32602;
}

// ── event loop ────────────────────────────────────────────────────────────────

fn main() {
    let cfg = parse_config();
    let elicitation_mode = cfg.elicitation_mode;
    let progress_delay_ms = cfg.progress_delay_ms;

    // Push retry configuration into the invoke layer's static config.
    invoke::set_retry_config(
        cfg.retry_on_busy,
        cfg.retry_delay_ms,
        cfg.retry_max_attempts,
    );
    invoke::set_rm_lookup_enabled(cfg.rm_lookup_enabled);
    tools::set_default_test_timeout(cfg.test_timeout_secs);

    let stdin = io::stdin();
    let stdout = io::stdout();
    // Channel-based reader enables timeout-bounded reads during elicitation.
    let line_reader = LineReader::new(stdin);
    let mut out = stdout.lock();

    // Emit a startup banner via the MCP logging-notification channel so it
    // appears in the client's MCP output pane as `info`, not `warning`.
    // (VS Code tags every line written to stderr as `[warning]`.)
    log_info(
        &mut out,
        format!(
            "cargo-mcp {ver} starting (pid={pid})",
            ver = env!("CARGO_PKG_VERSION"),
            pid = std::process::id(),
        ),
    );

    let names = tools::tool_names();
    let quoted: Vec<String> = names.iter().map(|n| format!("'{n}'")).collect();
    log_info(
        &mut out,
        format!("advertising {} tools: {}", names.len(), quoted.join(", ")),
    );
    log_info(&mut out, format!("elicitation mode: {elicitation_mode:?}"));
    if progress_delay_ms > 0 {
        log_info(
            &mut out,
            format!("progress delay: {progress_delay_ms}ms (developer mode)"),
        );
    }
    if cfg.retry_on_busy {
        log_info(
            &mut out,
            format!(
                "retry on transient busy errors: enabled (delay={delay}ms, max attempts={max})",
                delay = cfg.retry_delay_ms,
                max = cfg.retry_max_attempts,
            ),
        );
    } else {
        log_info(
            &mut out,
            "retry on transient busy errors: disabled".to_string(),
        );
    }
    if cfg.rm_lookup_enabled {
        if cfg!(windows) {
            log_info(
                &mut out,
                "Restart Manager process lookup: ENABLED (uses unsafe Win32 FFI in src/rm/)"
                    .to_string(),
            );
        } else {
            log_info(
                &mut out,
                "Restart Manager process lookup: requested but unavailable on this host \
                 (Windows-only; the Win32 FFI in src/rm/ is not compiled in here)"
                    .to_string(),
            );
        }
    }

    // Replay any warnings collected during CLI parsing through the MCP log
    // channel now that the stdout writer is available.
    for w in cfg.warnings {
        log_warn(&mut out, w);
    }

    // Whether the client declared `elicitation.form` capability.
    let mut can_elicit = false;

    while let Some(line) = line_reader.read_line() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let msg: Message = match serde_json::from_str(trimmed) {
            Ok(m) => m,
            Err(e) => {
                send_error(
                    &mut out,
                    Value::Null,
                    code::PARSE_ERROR,
                    format!("parse error: {e}"),
                );
                continue;
            }
        };

        // Notifications have no id — no response is sent.
        // Exception: `exit` must terminate the process so clients that keep
        // stdin open after `shutdown` are not left waiting indefinitely.
        let id = match msg.id {
            None => {
                if msg.method == "exit" || msg.method == "notifications/exit" {
                    std::process::exit(0);
                }
                continue;
            }
            Some(ref v) if v.is_null() => continue,
            Some(v) => v,
        };

        let body = match msg.method.as_str() {
            "initialize" => {
                // Check client capabilities for elicitation support.
                if let Some(ref params) = msg.params {
                    can_elicit = params
                        .get("capabilities")
                        .and_then(|c| c.get("elicitation"))
                        .and_then(|e| e.get("form"))
                        .is_some();
                    if can_elicit {
                        log_info(&mut out, "client supports elicitation (form)");
                    }
                }
                dispatch(msg.method.as_str(), msg.params)
            }
            "tools/call" => handle_tool_call(
                msg.params,
                can_elicit,
                elicitation_mode,
                progress_delay_ms,
                &line_reader,
                &mut out,
                &id,
            ),
            other => dispatch(other, msg.params),
        };

        send_response(
            &mut out,
            Response {
                jsonrpc: "2.0",
                id,
                body,
            },
        );
    }
}

// ── dispatch ──────────────────────────────────────────────────────────────────

fn dispatch(method: &str, params: Option<Value>) -> ResponseBody {
    match method {
        "initialize" => ResponseBody::Ok {
            result: serde_json::json!({
                "protocolVersion": "2024-11-05",
                "capabilities": { "tools": {}, "prompts": {} },
                "serverInfo": {
                    "name": "cargo-mcp",
                    "version": env!("CARGO_PKG_VERSION")
                }
            }),
        },

        "tools/list" => ResponseBody::Ok {
            result: serde_json::json!({ "tools": tools::list() }),
        },

        "prompts/list" => ResponseBody::Ok {
            result: serde_json::json!({
                "prompts": [{
                    "name": "setup",
                    "description":
                        "Check whether this repository's Copilot instruction files \
                         include the cargo-mcp section that directs Copilot to use \
                         MCP tools instead of running cargo commands in a terminal. \
                         If anything is missing or incomplete, add it in the \
                         appropriate instruction file and commit the change.",
                    "arguments": []
                }]
            }),
        },

        "prompts/get" => {
            let name = params
                .as_ref()
                .and_then(|p| p.get("name"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if name != "setup" {
                return ResponseBody::Err {
                    error: RpcError {
                        code: code::METHOD_NOT_FOUND,
                        message: format!("unknown prompt: {name:?}"),
                    },
                };
            }
            ResponseBody::Ok {
                result: serde_json::json!({
                    "description":
                        "Check whether this repository's Copilot instruction files \
                         include the cargo-mcp section that directs Copilot to use \
                         MCP tools instead of running cargo commands in a terminal. \
                         If anything is missing or incomplete, add it in the \
                         appropriate instruction file and commit the change.",
                    "messages": [{
                        "role": "user",
                        "content": {
                            "type": "text",
                            "text": "Run the cargo_setup tool and then, based on its output, \
                                     check whether this repository's Copilot instruction files \
                                     already direct Copilot to use the cargo-mcp MCP tools \
                                     instead of running cargo commands in a terminal. \
                                     If the instructions are missing or incomplete, add the \
                                     cargo-mcp section to the appropriate instruction file \
                                     (adapting the wording to fit the project's existing style) \
                                     and commit the change."
                        }
                    }]
                }),
            }
        }

        "ping" => ResponseBody::Ok {
            result: serde_json::json!({}),
        },

        "shutdown" => ResponseBody::Ok {
            result: Value::Null,
        },

        _ => ResponseBody::Err {
            error: RpcError {
                code: code::METHOD_NOT_FOUND,
                message: format!("method not found: {method}"),
            },
        },
    }
}

// ── tool call handling ─────────────────────────────────────────────────────

/// Handle a `tools/call` request, with optional elicitation for suggestions.
///
/// When the tool produces actionable suggestions (clippy/check) and the client
/// supports elicitation, a multi-select form is presented to the user. The
/// tool result then contains only the selected suggestions. When elicitation
/// is unavailable, suggestions are appended as a numbered text list.
fn handle_tool_call(
    params: Option<Value>,
    can_elicit: bool,
    elicitation_mode: ElicitationMode,
    progress_delay_ms: u64,
    reader: &LineReader,
    writer: &mut impl Write,
    request_id: &Value,
) -> ResponseBody {
    let params = match params {
        Some(p) => p,
        None => {
            return ResponseBody::Err {
                error: RpcError {
                    code: code::INVALID_PARAMS,
                    message: "tools/call requires params".into(),
                },
            };
        }
    };

    let name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
    let args = params
        .get("arguments")
        .cloned()
        .unwrap_or(serde_json::json!({}));
    let progress_token = params
        .get("_meta")
        .and_then(|m| m.get("progressToken"))
        .cloned();

    // Diagnostic trace of the raw tools/call as received over JSON-RPC,
    // logged BEFORE dispatch. This makes it possible to prove what `name`
    // the client actually sent, independent of any client-side UI label.
    log_info(
        writer,
        format!(
            "tools/call received: id={} name={:?} arguments={}",
            serde_json::to_string(request_id).unwrap_or_else(|_| "<unrepr>".into()),
            name,
            serde_json::to_string(&args).unwrap_or_else(|_| "<unrepr>".into()),
        ),
    );

    let result = if let Some(ref token) = progress_token {
        let mut notification_count: u32 = 0;
        // Derive a human-friendly verb ("check", "build", ...) from the tool
        // name so the final progress message includes it. Tools that don't
        // start with "cargo_" fall back to the raw name.
        let verb = name.strip_prefix("cargo_").unwrap_or(name).to_owned();
        let target = args
            .get("target")
            .and_then(|v| v.as_str())
            .map(str::to_owned);
        let mut tracker = BuildTracker::new(verb, target);
        let mut cb = |line: &str| {
            let msg = tracker.process_line(line);
            if !msg.is_empty() {
                send_progress_notification(writer, token, notification_count, &msg);
                notification_count += 1;
                if progress_delay_ms > 0 {
                    std::thread::sleep(std::time::Duration::from_millis(progress_delay_ms));
                }
            }
        };
        let cancel_token = reader.register_cancel(request_id.clone());
        let r = tools::call(name, &args, Some(&mut cb), Some(cancel_token));
        reader.clear_cancel();
        r
    } else {
        let cancel_token = reader.register_cancel(request_id.clone());
        let r = tools::call(name, &args, None, Some(cancel_token));
        reader.clear_cancel();
        r
    };

    match result {
        Ok(tools::ToolResult::Text(text)) => ResponseBody::Ok {
            result: serde_json::json!({
                "content": [{ "type": "text", "text": text }]
            }),
        },

        Ok(tools::ToolResult::WithSuggestions {
            output,
            suggestions,
        }) => {
            // Partition: MachineApplicable auto-reported, others need human approval.
            let (auto_apply, needs_approval): (Vec<_>, Vec<_>) = suggestions
                .into_iter()
                .partition(|s| s.applicability == suggest::Applicability::MachineApplicable);

            let auto_summary = if auto_apply.is_empty() {
                String::new()
            } else {
                let mut buf = format!(
                    "--- Auto-applicable ({n} fix{pl}, safe to apply) ---\n",
                    n = auto_apply.len(),
                    pl = if auto_apply.len() == 1 { "" } else { "es" },
                );
                buf.push_str(&elicit::format_selected_summary(&auto_apply));
                buf
            };

            if can_elicit
                && !needs_approval.is_empty()
                && elicitation_mode != ElicitationMode::AlwaysSkip
            {
                // AlwaysAccept: select all without prompting.
                // Prompt: present checkboxes for human-review suggestions.
                let selection = if elicitation_mode == ElicitationMode::AlwaysAccept {
                    Some(needs_approval.iter().map(|s| s.id).collect::<Vec<_>>())
                } else {
                    elicit::elicit_selection(reader, writer, &needs_approval)
                };
                match selection {
                    Some(ids) if !ids.is_empty() => {
                        let selected = elicit::filter_suggestions(&needs_approval, &ids);
                        let review_summary = elicit::format_selected_summary(&selected);
                        let combined = format!("{auto_summary}{review_summary}");
                        ResponseBody::Ok {
                            result: serde_json::json!({
                                "content": [
                                    { "type": "text", "text": combined },
                                    { "type": "text", "text": output },
                                ]
                            }),
                        }
                    }
                    _ => {
                        // User declined/cancelled — still report auto-applicable.
                        if auto_summary.is_empty() {
                            ResponseBody::Ok {
                                result: serde_json::json!({
                                    "content": [{ "type": "text", "text": output }]
                                }),
                            }
                        } else {
                            ResponseBody::Ok {
                                result: serde_json::json!({
                                    "content": [
                                        { "type": "text", "text": auto_summary },
                                        { "type": "text", "text": output },
                                    ]
                                }),
                            }
                        }
                    }
                }
            } else if !auto_apply.is_empty() || !needs_approval.is_empty() {
                // No elicitation — append numbered lists so LLM can act on them.
                let mut combined = output.clone();
                if !auto_apply.is_empty() {
                    combined.push_str(&format!(
                        "\n\n--- Auto-applicable ({n} fix{pl}, safe to apply) ---\n",
                        n = auto_apply.len(),
                        pl = if auto_apply.len() == 1 { "" } else { "es" },
                    ));
                    combined.push_str(&suggest::format_numbered_list(&auto_apply));
                }
                if !needs_approval.is_empty() {
                    combined.push_str(&format!(
                        "\n--- Needs review ({n} suggestion{pl}) ---\n",
                        n = needs_approval.len(),
                        pl = if needs_approval.len() == 1 { "" } else { "s" },
                    ));
                    combined.push_str(&suggest::format_numbered_list(&needs_approval));
                }
                ResponseBody::Ok {
                    result: serde_json::json!({
                        "content": [{ "type": "text", "text": combined }]
                    }),
                }
            } else {
                // No suggestions found.
                ResponseBody::Ok {
                    result: serde_json::json!({
                        "content": [{ "type": "text", "text": output }]
                    }),
                }
            }
        }

        Err(e) if e.downcast_ref::<invoke::CancelledError>().is_some() => ResponseBody::Ok {
            result: serde_json::json!({
                "content": [{ "type": "text", "text": "Operation cancelled by client request." }],
                "isError": true
            }),
        },

        Err(e) => {
            log_warn(writer, format!("tool '{name}' failed: {e}"));
            ResponseBody::Ok {
                result: serde_json::json!({
                    "content": [{ "type": "text", "text": format!("error: {e}") }],
                    "isError": true
                }),
            }
        }
    }
}

// ── I/O helpers ───────────────────────────────────────────────────────────────

/// Tracks per-invocation build progress so notifications can include counters.
///
/// - `compile_count` — crates actually being compiled (non-fresh artifacts).
/// - `total_count`   — all artifacts seen so far (fresh + non-fresh); a
///   running lower bound on the total number of crates in the build graph.
/// - `verb` / `target` — included in the terminal `build-finished` /
///   `build-failed` message so the chat-history summary line is unambiguous
///   (e.g. `cargo check (x86_64-pc-windows-msvc) finished` instead of just
///   `Build finished`).
struct BuildTracker {
    compile_count: u32,
    total_count: u32,
    verb: String,
    target: Option<String>,
}

impl BuildTracker {
    fn new(verb: String, target: Option<String>) -> Self {
        Self {
            compile_count: 0,
            total_count: 0,
            verb,
            target,
        }
    }

    /// Process one cargo `--message-format=json` NDJSON line.
    /// Returns a human-readable progress message, or an empty string if no
    /// notification should be sent for this line.
    fn process_line(&mut self, line: &str) -> String {
        // Synthetic out-of-band progress lines that the invoke layer injects
        // directly (e.g. retry-on-busy notices) are prefixed with
        // `cargo-mcp:` and should be forwarded verbatim — they aren't
        // cargo's JSON output and would otherwise be silently dropped by the
        // `serde_json::from_str` parse below.
        if let Some(rest) = line.strip_prefix("cargo-mcp:") {
            return format!("cargo-mcp:{rest}");
        }
        let v = match serde_json::from_str::<Value>(line) {
            Ok(v) => v,
            Err(_) => {
                // Non-JSON line from cargo test: libtest harness output
                // ("running N tests", "test foo ... ok", etc.) or captured
                // println! replays. Forward verbatim so the MCP client can
                // show test progress in real time.
                if line.is_empty() {
                    return String::new();
                }
                return line.to_owned();
            }
        };
        match v.get("reason").and_then(|r| r.as_str()) {
            Some("compiler-artifact") => {
                let fresh = v.get("fresh").and_then(|f| f.as_bool()).unwrap_or(true);
                self.total_count += 1;
                if fresh {
                    // Cached — counts toward the total but no notification.
                    return String::new();
                }
                self.compile_count += 1;

                let pkg_id = v.get("package_id").and_then(|p| p.as_str()).unwrap_or("");
                let registry_name = registry_label(pkg_id);
                let (name, version) = parse_package_id(pkg_id, &v);

                let counter = format!("({}/{})", self.compile_count, self.total_count);
                let verb = &self.verb;
                if let Some(reg) = registry_name {
                    format!("{verb}: {name} v{version} {counter} [{reg}]")
                } else {
                    format!("{verb}: {name} v{version} {counter}")
                }
            }
            Some("compiler-message") => {
                let msg = v
                    .get("message")
                    .and_then(|m| m.get("message"))
                    .and_then(|m| m.as_str())
                    .unwrap_or("");
                let level = v
                    .get("message")
                    .and_then(|m| m.get("level"))
                    .and_then(|l| l.as_str())
                    .unwrap_or("note");
                if msg.is_empty() {
                    return String::new();
                }
                let truncated: String = msg.chars().take(120).collect();
                format!("{}: [{level}] {truncated}", self.verb)
            }
            Some("build-finished") => {
                let ok = v.get("success").and_then(|s| s.as_bool()).unwrap_or(false);
                let target_suffix = match &self.target {
                    Some(t) => format!(" ({t})"),
                    None => String::new(),
                };
                let outcome = if ok { "finished" } else { "failed" };
                format!("cargo {}{} {}", self.verb, target_suffix, outcome)
            }
            _ => String::new(),
        }
    }
}

/// Parse a cargo package ID into `(name, version)`.
///
/// Cargo package ID formats:
/// - `registry+https://...#serde@1.0.228`          → (`serde`, `1.0.228`)
/// - `path+file:///path/to/crate#my-crate@0.1.0`   → (`my-crate`, `0.1.0`)
/// - `path+file:///path/to/crate#0.1.0`            → (target name, `0.1.0`)
fn parse_package_id(pkg_id: &str, artifact: &Value) -> (String, String) {
    let fragment = pkg_id.split('#').nth(1).unwrap_or("");
    if let Some(at) = fragment.rfind('@') {
        let name = fragment[..at].to_owned();
        let version = fragment[at + 1..].to_owned();
        (name, version)
    } else {
        // Fragment is a bare version — fall back to the target name.
        let name = artifact
            .get("target")
            .and_then(|t| t.get("name"))
            .and_then(|n| n.as_str())
            .unwrap_or("?")
            .to_owned();
        (name, fragment.to_owned())
    }
}

/// Extract a short registry label from a cargo package ID, or `None` for
/// local path crates.
///
/// - `registry+https://github.com/rust-lang/crates.io-index#...` → `Some("crates.io")`
/// - `registry+https://dl.cloudsmith.io/my-org/cargo/index.git#...` → `Some("index.git")`
/// - `path+file:///...` → `None`
fn registry_label(pkg_id: &str) -> Option<String> {
    let url = pkg_id.strip_prefix("registry+")?;
    let url_no_fragment = url.split('#').next().unwrap_or(url);
    let last_segment = url_no_fragment
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or(url_no_fragment);
    if last_segment == "crates.io-index" {
        Some("crates.io".to_owned())
    } else {
        Some(last_segment.to_owned())
    }
}

/// Send a JSON-RPC 2.0 `notifications/progress` message (no `id` field).
/// `progress` is a monotonically increasing counter; no `total` is provided
/// because cargo does not report total work units in JSON mode.
fn send_progress_notification(
    out: &mut impl io::Write,
    token: &Value,
    progress: u32,
    message: &str,
) {
    send_notification(
        out,
        "notifications/progress",
        serde_json::json!({
            "progressToken": token,
            "progress": progress,
            "message": message,
        }),
    );
}

fn send_response(out: &mut impl io::Write, response: Response) {
    match serde_json::to_string(&response) {
        Ok(mut s) => {
            s.push('\n');
            let _ = out.write_all(s.as_bytes());
            let _ = out.flush();
        }
        Err(e) => log_warn(out, format!("cargo-mcp: serialization error: {e}")),
    }
}

fn send_error(out: &mut impl io::Write, id: Value, code: i32, message: String) {
    send_response(
        out,
        Response {
            jsonrpc: "2.0",
            id,
            body: ResponseBody::Err {
                error: RpcError { code, message },
            },
        },
    );
}

/// Send a JSON-RPC 2.0 notification (no `id` field) on stdout.
///
/// Per the MCP spec the server may send `notifications/message` to surface
/// log output to the client. VS Code displays these in the per-server MCP
/// output channel at the supplied level. Writing to stderr instead causes
/// VS Code to tag every line as `[warning]` regardless of intent.
fn send_notification(out: &mut impl io::Write, method: &str, params: Value) {
    let msg = serde_json::json!({
        "jsonrpc": "2.0",
        "method": method,
        "params": params,
    });
    if let Ok(mut s) = serde_json::to_string(&msg) {
        s.push('\n');
        let _ = out.write_all(s.as_bytes());
        let _ = out.flush();
    }
}

/// Send an `info`-level log notification with `logger = "cargo-mcp"`.
fn log_info(out: &mut impl io::Write, message: impl Into<String>) {
    send_notification(
        out,
        "notifications/message",
        serde_json::json!({
            "level": "info",
            "logger": "cargo-mcp",
            "data": message.into(),
        }),
    );
}

/// Send a `warning`-level log notification with `logger = "cargo-mcp"`.
fn log_warn(out: &mut impl io::Write, message: impl Into<String>) {
    send_notification(
        out,
        "notifications/message",
        serde_json::json!({
            "level": "warning",
            "logger": "cargo-mcp",
            "data": message.into(),
        }),
    );
}
