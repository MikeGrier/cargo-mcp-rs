// Copyright (c) Michael Grier. All rights reserved.

//! MCP elicitation support — server-to-client request for user input.
//!
//! When the MCP client declares `elicitation.form` capability, the server can
//! present a multi-select form (checkboxes) to the user during tool execution.
//! This module builds the `TitledMultiSelectEnumSchema` from a list of
//! suggestions and handles the JSON-RPC round-trip.
//!
//! When the suggestion count reaches [`GROUPING_THRESHOLD`], suggestions are
//! presented grouped (by lint code or by file path) with "select all" headers
//! per group and a mode-switch entry to toggle between the two views.

use std::{
    collections::HashMap,
    io::Write,
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, Instant},
};

use serde_json::Value;

use crate::{
    line_reader::LineReader,
    suggest::{self, Suggestion},
};

/// Monotonically increasing counter for server-originated request IDs.
static NEXT_REQUEST_ID: AtomicU64 = AtomicU64::new(1);

/// How long to wait for the user to respond to an elicitation prompt before
/// automatically declining. This prevents the server from hanging indefinitely
/// when the client never sends a response.
const ELICITATION_TIMEOUT: Duration = Duration::from_secs(30);

/// Minimum number of suggestions before grouped presentation activates.
const GROUPING_THRESHOLD: usize = 5;

/// Minimum group size to receive a "select all" header entry.
const MIN_GROUP_FOR_HEADER: usize = 2;

/// How suggestions are grouped in the elicitation UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GroupMode {
    /// Group by lint/error code (e.g. all `clippy::needless_return` together).
    ByLint,
    /// Group by file path (e.g. all suggestions in `invoke.rs` together).
    ByPath,
}

impl GroupMode {
    fn toggled(self) -> Self {
        match self {
            Self::ByLint => Self::ByPath,
            Self::ByPath => Self::ByLint,
        }
    }
}

/// A group of suggestions sharing a common key (lint code or file path).
struct SuggestionGroup<'a> {
    /// Display label for the group header (e.g. "clippy::needless_return" or "invoke.rs").
    label: String,
    /// The key used in `all:<key>` const values.
    key: String,
    /// References to the grouped suggestions (ordered by their original id).
    members: Vec<&'a Suggestion>,
}

/// Result of parsing a grouped elicitation response.
enum GroupedSelection {
    /// User wants to switch to a different grouping view.
    ModeSwitch(GroupMode),
    /// User selected these suggestion IDs (after expanding any group-all entries).
    Selected(Vec<usize>),
}

/// Const value for the "skip all" option — allows the user to decline
/// without hitting the dialog close button.
const SKIP_ALL_CONST: &str = "skip:all";

/// Send a `notifications/message` log entry over the JSON-RPC writer.
///
/// Using the MCP notification channel instead of `eprintln!` ensures VS Code
/// displays the message at the correct severity level in the server output
/// channel rather than tagging every line as `[warning]`.
fn send_log_notification(writer: &mut impl Write, level: &str, message: &str) {
    let msg = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "notifications/message",
        "params": {
            "level": level,
            "logger": "cargo-mcp",
            "data": message,
        }
    });
    if let Ok(mut s) = serde_json::to_string(&msg) {
        s.push('\n');
        let _ = writer.write_all(s.as_bytes());
        let _ = writer.flush();
    }
}

/// Build a `TitledMultiSelectEnumSchema` for the given suggestions.
///
/// Each suggestion becomes a selectable option with:
/// - `const`: the stringified suggestion id (e.g. `"1"`, `"2"`)
/// - `title`: a human-readable label (file, description, trust level)
fn build_multi_select_schema(suggestions: &[Suggestion]) -> Value {
    let mut options: Vec<Value> = suggestions
        .iter()
        .map(|s| {
            serde_json::json!({
                "const": s.id.to_string(),
                "title": suggest::elicitation_label(s),
            })
        })
        .collect();

    // Add a "skip all" entry so the user can decline cleanly.
    options.push(serde_json::json!({
        "const": SKIP_ALL_CONST,
        "title": "\u{2205} Skip all \u{2014} apply none of these suggestions",
    }));

    serde_json::json!({
        "type": "object",
        "properties": {
            "selected": {
                "type": "array",
                "title": "Review these suggestions",
                "description": "These suggestions may change behaviour or contain \
                    placeholders. Check the ones you want applied.",
                "items": {
                    "anyOf": options
                }
            }
        },
        "required": ["selected"]
    })
}

/// Build the JSON-RPC request for `elicitation/create`.
fn build_elicit_request(request_id: u64, suggestions: &[Suggestion]) -> Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": format!("s-{request_id}"),
        "method": "elicitation/create",
        "params": {
            "message": format!(
                "Cargo found {} suggestion(s) that need human review. \
                 Safe, machine-verified fixes were already reported. \
                 Check the ones you\u{2019}d like applied:",
                suggestions.len()
            ),
            "requestedSchema": build_multi_select_schema(suggestions),
        }
    })
}

// ── Grouping ──────────────────────────────────────────────────────────────────

/// Group suggestions by lint/error code.
///
/// Suggestions with `None` code each form their own singleton group (keyed by id).
/// Groups are returned sorted largest-first; ties broken by first occurrence.
fn group_by_lint<'a>(suggestions: &'a [Suggestion]) -> Vec<SuggestionGroup<'a>> {
    group_by(suggestions, |s| {
        s.code.clone().unwrap_or_else(|| format!("_id:{}", s.id))
    })
}

/// Group suggestions by file path.
///
/// Groups are returned sorted largest-first; ties broken by first occurrence.
fn group_by_path<'a>(suggestions: &'a [Suggestion]) -> Vec<SuggestionGroup<'a>> {
    group_by(suggestions, |s| s.file.clone())
}

/// Generic grouping: partition suggestions by a key function, then sort groups
/// largest-first (stable sort preserves insertion order for ties).
fn group_by<'a>(
    suggestions: &'a [Suggestion],
    key_fn: impl Fn(&Suggestion) -> String,
) -> Vec<SuggestionGroup<'a>> {
    // Preserve insertion order via Vec of (key, members).
    let mut order: Vec<String> = Vec::new();
    let mut map: HashMap<String, Vec<&'a Suggestion>> = HashMap::new();

    for s in suggestions {
        let key = key_fn(s);
        if !map.contains_key(&key) {
            order.push(key.clone());
        }
        map.entry(key).or_default().push(s);
    }

    let mut groups: Vec<SuggestionGroup<'a>> = order
        .into_iter()
        .map(|key| {
            let members = map.remove(&key).unwrap();
            let label = if key.starts_with("_id:") {
                // Singleton with no code — use the suggestion's elicitation label.
                suggest::elicitation_label(members[0])
            } else {
                key.clone()
            };
            SuggestionGroup {
                label,
                key,
                members,
            }
        })
        .collect();

    // Sort largest group first (stable sort keeps insertion order for ties).
    groups.sort_by_key(|g| std::cmp::Reverse(g.members.len()));
    groups
}

/// Build a grouped `TitledMultiSelectEnumSchema`.
///
/// For each group with ≥ [`MIN_GROUP_FOR_HEADER`] members, a "select all" header
/// entry is prepended (const = `"all:<key>"`). A mode-switch entry is appended
/// at the end (const = `"view:by-path"` or `"view:by-lint"`).
fn build_grouped_schema(suggestions: &[Suggestion], mode: GroupMode) -> Value {
    let groups = match mode {
        GroupMode::ByLint => group_by_lint(suggestions),
        GroupMode::ByPath => group_by_path(suggestions),
    };

    let mut options: Vec<Value> = Vec::new();

    for group in &groups {
        if group.members.len() >= MIN_GROUP_FOR_HEADER {
            // Group header: "Select all: <label> (N instances)"
            options.push(serde_json::json!({
                "const": format!("all:{}", group.key),
                "title": format!(
                    "\u{25B6} Select all: {} ({} instances)",
                    group.label,
                    group.members.len()
                ),
            }));
        }
        // Individual items, indented with a dash when under a group header.
        for s in &group.members {
            let prefix = if group.members.len() >= MIN_GROUP_FOR_HEADER {
                "  \u{2013} "
            } else {
                ""
            };
            options.push(serde_json::json!({
                "const": s.id.to_string(),
                "title": format!("{prefix}{}", suggest::elicitation_label(s)),
            }));
        }
    }

    // Mode-switch entry at the end.
    let switch_mode = mode.toggled();
    let switch_label = match switch_mode {
        GroupMode::ByLint => "view:by-lint",
        GroupMode::ByPath => "view:by-path",
    };
    let switch_desc = match switch_mode {
        GroupMode::ByLint => "\u{21BB} Switch view: group by lint code",
        GroupMode::ByPath => "\u{21BB} Switch view: group by file path",
    };
    options.push(serde_json::json!({
        "const": switch_label,
        "title": switch_desc,
    }));

    // "Skip all" entry so the user can decline cleanly.
    options.push(serde_json::json!({
        "const": SKIP_ALL_CONST,
        "title": "\u{2205} Skip all \u{2014} apply none of these suggestions",
    }));

    serde_json::json!({
        "type": "object",
        "properties": {
            "selected": {
                "type": "array",
                "title": "Review these suggestions",
                "description": "These suggestions may change behaviour or contain \
                    placeholders. Check the ones you want applied.",
                "items": {
                    "anyOf": options
                }
            }
        },
        "required": ["selected"]
    })
}

/// Build a grouped elicitation request.
fn build_grouped_elicit_request(
    request_id: u64,
    suggestions: &[Suggestion],
    mode: GroupMode,
) -> Value {
    let mode_desc = match mode {
        GroupMode::ByLint => " (grouped by lint)",
        GroupMode::ByPath => " (grouped by file)",
    };
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": format!("s-{request_id}"),
        "method": "elicitation/create",
        "params": {
            "message": format!(
                "Cargo found {} suggestion(s) that need human review{mode_desc}. \
                 Safe, machine-verified fixes were already reported. \
                 Check the ones you\u{2019}d like applied:",
                suggestions.len()
            ),
            "requestedSchema": build_grouped_schema(suggestions, mode),
        }
    })
}

/// Parse a grouped elicitation response.
///
/// Examines the selected const values:
/// - `"view:by-lint"` or `"view:by-path"` → [`GroupedSelection::ModeSwitch`]
/// - `"all:<key>"` → expands to all member IDs of that group
/// - numeric strings → individual suggestion IDs
///
/// Returns deduplicated IDs in ascending order.
fn parse_grouped_response(
    selected: &[&str],
    suggestions: &[Suggestion],
    mode: GroupMode,
) -> GroupedSelection {
    // Check for mode switch first.
    for s in selected {
        if *s == "view:by-lint" {
            return GroupedSelection::ModeSwitch(GroupMode::ByLint);
        }
        if *s == "view:by-path" {
            return GroupedSelection::ModeSwitch(GroupMode::ByPath);
        }
    }

    // If the user chose "skip all", return an empty selection immediately
    // rather than falling through to ID expansion.
    if selected.contains(&SKIP_ALL_CONST) {
        return GroupedSelection::Selected(Vec::new());
    }

    let groups = match mode {
        GroupMode::ByLint => group_by_lint(suggestions),
        GroupMode::ByPath => group_by_path(suggestions),
    };

    // Build a map from group key → member IDs for expansion.
    let group_map: HashMap<&str, Vec<usize>> = groups
        .iter()
        .map(|g| (g.key.as_str(), g.members.iter().map(|s| s.id).collect()))
        .collect();

    let mut ids: Vec<usize> = Vec::new();
    for s in selected {
        if let Some(key) = s.strip_prefix("all:") {
            if let Some(members) = group_map.get(key) {
                ids.extend(members);
            }
        } else if let Ok(id) = s.parse::<usize>() {
            ids.push(id);
        }
    }

    // Deduplicate and sort.
    ids.sort_unstable();
    ids.dedup();
    GroupedSelection::Selected(ids)
}

/// Send an elicitation request and read the user's response.
///
/// When the suggestion count reaches [`GROUPING_THRESHOLD`], suggestions are
/// presented grouped (default: by lint code) with "select all" headers and a
/// mode-switch toggle. The user may switch view once before making a selection.
///
/// Returns the set of suggestion IDs the user selected, or `None` if the user
/// declined/cancelled or the round-trip failed.
pub fn elicit_selection(
    reader: &LineReader,
    writer: &mut impl Write,
    suggestions: &[Suggestion],
) -> Option<Vec<usize>> {
    if suggestions.len() < GROUPING_THRESHOLD {
        return elicit_flat(reader, writer, suggestions);
    }
    elicit_grouped(reader, writer, suggestions)
}

/// Flat (ungrouped) elicitation — used when below the grouping threshold.
fn elicit_flat(
    reader: &LineReader,
    writer: &mut impl Write,
    suggestions: &[Suggestion],
) -> Option<Vec<usize>> {
    let req_id = NEXT_REQUEST_ID.fetch_add(1, Ordering::Relaxed);
    let request = build_elicit_request(req_id, suggestions);
    let expected_id = format!("s-{req_id}");

    send_and_read_raw(reader, writer, &request, &expected_id)
}

/// Grouped elicitation with mode-switch support (max 1 re-elicit).
fn elicit_grouped(
    reader: &LineReader,
    writer: &mut impl Write,
    suggestions: &[Suggestion],
) -> Option<Vec<usize>> {
    let mut mode = GroupMode::ByLint;
    // Allow at most one mode switch to prevent infinite loops.
    for _ in 0..2 {
        let req_id = NEXT_REQUEST_ID.fetch_add(1, Ordering::Relaxed);
        let request = build_grouped_elicit_request(req_id, suggestions, mode);
        let expected_id = format!("s-{req_id}");

        let raw_selected = send_and_read_strings(reader, writer, &request, &expected_id)?;
        let str_refs: Vec<&str> = raw_selected.iter().map(|s| s.as_str()).collect();

        match parse_grouped_response(&str_refs, suggestions, mode) {
            GroupedSelection::ModeSwitch(new_mode) => {
                send_log_notification(
                    writer,
                    "info",
                    &format!(
                        "cargo-mcp: user switched elicitation view to {:?}",
                        new_mode
                    ),
                );
                mode = new_mode;
                continue;
            }
            GroupedSelection::Selected(ids) => {
                if ids.is_empty() {
                    return Some(Vec::new());
                }
                return Some(ids);
            }
        }
    }
    // Exhausted mode-switch attempts — should not normally reach here.
    None
}

/// Send a `notifications/cancelled` notification to tell the client to dismiss
/// an outstanding elicitation dialog.
fn send_cancel_notification(writer: &mut impl Write, request_id: &str) {
    let notification = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "notifications/cancelled",
        "params": {
            "requestId": request_id,
            "reason": "elicitation timed out"
        }
    });
    if let Ok(mut payload) = serde_json::to_string(&notification) {
        payload.push('\n');
        let _ = writer.write_all(payload.as_bytes());
        let _ = writer.flush();
    }
}

/// Low-level: send a JSON-RPC request and read raw selected ID strings.
///
/// Returns `None` on decline/cancel/error, or `Some(vec of selected const strings)`.
fn send_and_read_strings(
    reader: &LineReader,
    writer: &mut impl Write,
    request: &Value,
    expected_id: &str,
) -> Option<Vec<String>> {
    let mut payload = match serde_json::to_string(request) {
        Ok(s) => s,
        Err(e) => {
            send_log_notification(
                writer,
                "warning",
                &format!("cargo-mcp: elicitation serialization error: {e}"),
            );
            return None;
        }
    };
    payload.push('\n');
    if writer.write_all(payload.as_bytes()).is_err() {
        return None;
    }
    if writer.flush().is_err() {
        return None;
    }

    let deadline = Instant::now() + ELICITATION_TIMEOUT;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            send_log_notification(
                writer,
                "warning",
                &format!(
                    "cargo-mcp: elicitation timed out after {}s — declining automatically",
                    ELICITATION_TIMEOUT.as_secs()
                ),
            );
            send_cancel_notification(writer, expected_id);
            return None;
        }

        let line = match reader.read_line_timeout(remaining) {
            Some(l) => l,
            None => {
                send_log_notification(
                    writer,
                    "warning",
                    &format!(
                        "cargo-mcp: elicitation timed out after {}s — declining automatically",
                        ELICITATION_TIMEOUT.as_secs()
                    ),
                );
                send_cancel_notification(writer, expected_id);
                return None;
            }
        };

        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let msg: Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(_) => continue,
        };

        let msg_id = msg.get("id").and_then(|v| v.as_str());
        if msg_id != Some(expected_id) {
            let method = msg.get("method").and_then(|v| v.as_str()).unwrap_or("");

            // exit / notifications/exit are notifications (no id). Honour them
            // immediately: the message has been consumed from the LineReader so
            // the main loop will never see it, but process::exit is equivalent.
            if matches!(method, "exit" | "notifications/exit") {
                std::process::exit(0);
            }

            // shutdown is a request (has id). Send the required null-result
            // response before exiting so the client sees a clean shutdown.
            if method == "shutdown" {
                if let Some(id) = msg.get("id").cloned() {
                    let resp = serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": null
                    });
                    if let Ok(mut s) = serde_json::to_string(&resp) {
                        s.push('\n');
                        let _ = writer.write_all(s.as_bytes());
                        let _ = writer.flush();
                    }
                }
                std::process::exit(0);
            }

            // Any other request arrived while we were waiting. We cannot
            // re-queue it into the shared LineReader, so respond with a
            // server-error so the client is not left waiting indefinitely.
            if let Some(id) = msg.get("id").cloned() {
                let resp = serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": {
                        "code": -32000,
                        "message": format!(
                            "cargo-mcp: server is busy waiting for elicitation response \
                             to request {expected_id}; try again after the dialog is dismissed"
                        )
                    }
                });
                if let Ok(mut s) = serde_json::to_string(&resp) {
                    s.push('\n');
                    let _ = writer.write_all(s.as_bytes());
                    let _ = writer.flush();
                }
            }
            // Notifications (no id) other than exit/shutdown: log and continue.
            send_log_notification(
                writer,
                "info",
                &format!(
                    "cargo-mcp: received unrelated message while waiting for \
                     elicitation response: {}",
                    if method.is_empty() { "(response)" } else { method }
                ),
            );
            continue;
        }

        let result = match msg.get("result") {
            Some(r) => r,
            None => {
                send_log_notification(
                    writer,
                    "warning",
                    "cargo-mcp: elicitation request returned error",
                );
                return None;
            }
        };

        let action = result.get("action").and_then(|v| v.as_str());
        if action != Some("accept") {
            return None;
        }

        let selected = result
            .get("content")
            .and_then(|v| v.get("selected"))
            .and_then(|v| v.as_array());

        let strings: Vec<String> = selected
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str())
                    .map(|s| s.to_owned())
                    .collect()
            })
            .unwrap_or_default();

        return Some(strings);
    }
}

/// Low-level: send request, read response, parse as integer IDs (flat mode).
fn send_and_read_raw(
    reader: &LineReader,
    writer: &mut impl Write,
    request: &Value,
    expected_id: &str,
) -> Option<Vec<usize>> {
    let strings = send_and_read_strings(reader, writer, request, expected_id)?;
    // If the user chose "skip all", return an empty selection rather than
    // falling through to numeric parsing (which would silently discard it).
    if strings.contains(&SKIP_ALL_CONST.to_owned()) {
        return Some(Vec::new());
    }
    let ids: Vec<usize> = strings
        .iter()
        .filter_map(|s| s.parse::<usize>().ok())
        .collect();
    Some(ids)
}

/// Filter suggestions to only those with the given IDs.
pub fn filter_suggestions(suggestions: &[Suggestion], ids: &[usize]) -> Vec<Suggestion> {
    suggestions
        .iter()
        .filter(|s| ids.contains(&s.id))
        .cloned()
        .collect()
}

/// Format the selected suggestions as a structured summary for the tool result.
pub fn format_selected_summary(selected: &[Suggestion]) -> String {
    if selected.is_empty() {
        return "No suggestions selected.".to_owned();
    }

    let mut buf = format!("{} suggestion(s) selected:\n\n", selected.len());
    for s in selected {
        buf.push_str(&format!("{}. {}\n", s.id, suggest::suggestion_label(s)));
        if s.replacements.len() > 1 {
            buf.push_str(&format!(
                "   Multi-span fix ({} locations — apply all atomically):\n",
                s.replacements.len()
            ));
            for (k, r) in s.replacements.iter().enumerate() {
                buf.push_str(&format!(
                    "   ({}) {} lines {}-{}, cols {}-{}: {}\n",
                    k + 1,
                    r.span.file,
                    r.span.line_start,
                    r.span.line_end,
                    r.span.column_start,
                    r.span.column_end,
                    if r.text.is_empty() {
                        "(remove text)".to_owned()
                    } else {
                        format!("{:?}", r.text)
                    },
                ));
            }
        } else if let (Some(repl), Some(span)) = (&s.replacement, &s.replacement_span) {
            buf.push_str(&format!(
                "   File: {} lines {}-{}, columns {}-{}\n",
                span.file, span.line_start, span.line_end, span.column_start, span.column_end,
            ));
            if repl.is_empty() {
                buf.push_str("   Action: remove text in span\n");
            } else {
                buf.push_str(&format!("   Replace with: {repl:?}\n"));
            }
        }
        buf.push('\n');
    }
    buf
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use crate::line_reader::LineReader;

    /// Guards tests that depend on the global NEXT_REQUEST_ID counter.
    static ID_LOCK: Mutex<()> = Mutex::new(());

    fn make_suggestions(n: usize) -> Vec<Suggestion> {
        (1..=n)
            .map(|i| Suggestion {
                id: i,
                message: format!("suggestion {i}"),
                parent_message: format!("parent description for {i}"),
                code: Some(format!("W{i:03}")),
                level: "warning".into(),
                file: format!("src/f{i}.rs"),
                line_start: i * 10,
                column_start: 1,
                replacement: Some(format!("fix_{i}")),
                replacement_span: Some(crate::suggest::Span {
                    file: format!("src/f{i}.rs"),
                    line_start: i * 10,
                    line_end: i * 10,
                    column_start: 1,
                    column_end: 5,
                }),
                replacements: vec![crate::suggest::SpanReplacement {
                    span: crate::suggest::Span {
                        file: format!("src/f{i}.rs"),
                        line_start: i * 10,
                        line_end: i * 10,
                        column_start: 1,
                        column_end: 5,
                    },
                    text: format!("fix_{i}"),
                }],
                applicability: crate::suggest::Applicability::MachineApplicable,
            })
            .collect()
    }

    #[test]
    fn schema_has_all_options() {
        let suggestions = make_suggestions(3);
        let schema = build_multi_select_schema(&suggestions);
        let any_of = schema["properties"]["selected"]["items"]["anyOf"]
            .as_array()
            .unwrap();
        // 3 suggestions + 1 "skip all" entry.
        assert_eq!(any_of.len(), 4);
        assert_eq!(any_of[0]["const"], "1");
        assert_eq!(any_of[2]["const"], "3");
        // elicitation_label uses parent_message and filename.
        assert!(any_of[1]["title"].as_str().unwrap().contains("f2.rs:20"));
        // Last entry is the skip-all option.
        assert_eq!(any_of[3]["const"], SKIP_ALL_CONST);
    }

    #[test]
    fn request_has_correct_structure() {
        let suggestions = make_suggestions(2);
        let req = build_elicit_request(42, &suggestions);
        assert_eq!(req["jsonrpc"], "2.0");
        assert_eq!(req["id"], "s-42");
        assert_eq!(req["method"], "elicitation/create");
        assert!(req["params"]["message"].as_str().unwrap().contains("2"));
        assert!(req["params"]["requestedSchema"]["properties"]["selected"].is_object());
        // Updated title.
        assert_eq!(
            req["params"]["requestedSchema"]["properties"]["selected"]["title"],
            "Review these suggestions"
        );
    }

    #[test]
    fn filter_suggestions_by_id() {
        let suggestions = make_suggestions(5);
        let filtered = filter_suggestions(&suggestions, &[2, 4]);
        assert_eq!(filtered.len(), 2);
        assert_eq!(filtered[0].id, 2);
        assert_eq!(filtered[1].id, 4);
    }

    #[test]
    fn filter_with_no_ids_returns_empty() {
        let suggestions = make_suggestions(3);
        let filtered = filter_suggestions(&suggestions, &[]);
        assert!(filtered.is_empty());
    }

    #[test]
    fn filter_with_missing_ids_skips_them() {
        let suggestions = make_suggestions(3);
        let filtered = filter_suggestions(&suggestions, &[1, 99]);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].id, 1);
    }

    #[test]
    fn elicit_accept_response() {
        let _guard = ID_LOCK.lock().unwrap();
        let suggestions = make_suggestions(3);
        // Simulate client response.
        let response = serde_json::json!({
            "jsonrpc": "2.0",
            "id": "s-1",
            "result": {
                "action": "accept",
                "content": { "selected": ["1", "3"] }
            }
        });
        let resp_str = serde_json::to_string(&response).unwrap();
        let reader = LineReader::from_lines(&[&resp_str]);
        let mut writer = Vec::new();

        // Reset counter for deterministic test.
        NEXT_REQUEST_ID.store(1, Ordering::Relaxed);

        let ids = elicit_selection(&reader, &mut writer, &suggestions);
        assert_eq!(ids, Some(vec![1, 3]));

        // Verify the request was written.
        let written = String::from_utf8(writer).unwrap();
        let req: Value = serde_json::from_str(written.trim()).unwrap();
        assert_eq!(req["method"], "elicitation/create");
    }

    #[test]
    fn elicit_decline_response() {
        let _guard = ID_LOCK.lock().unwrap();
        let suggestions = make_suggestions(2);
        let response = serde_json::json!({
            "jsonrpc": "2.0",
            "id": "s-1",
            "result": { "action": "decline" }
        });
        let resp_str = serde_json::to_string(&response).unwrap();
        let reader = LineReader::from_lines(&[&resp_str]);
        let mut writer = Vec::new();

        NEXT_REQUEST_ID.store(1, Ordering::Relaxed);
        let ids = elicit_selection(&reader, &mut writer, &suggestions);
        assert!(ids.is_none());
    }

    #[test]
    fn elicit_cancel_response() {
        let _guard = ID_LOCK.lock().unwrap();
        let suggestions = make_suggestions(2);
        let response = serde_json::json!({
            "jsonrpc": "2.0",
            "id": "s-1",
            "result": { "action": "cancel" }
        });
        let resp_str = serde_json::to_string(&response).unwrap();
        let reader = LineReader::from_lines(&[&resp_str]);
        let mut writer = Vec::new();

        NEXT_REQUEST_ID.store(1, Ordering::Relaxed);
        let ids = elicit_selection(&reader, &mut writer, &suggestions);
        assert!(ids.is_none());
    }

    #[test]
    fn elicit_error_response() {
        let _guard = ID_LOCK.lock().unwrap();
        let suggestions = make_suggestions(1);
        let response = serde_json::json!({
            "jsonrpc": "2.0",
            "id": "s-1",
            "error": { "code": -32601, "message": "method not found" }
        });
        let resp_str = serde_json::to_string(&response).unwrap();
        let reader = LineReader::from_lines(&[&resp_str]);
        let mut writer = Vec::new();

        NEXT_REQUEST_ID.store(1, Ordering::Relaxed);
        let ids = elicit_selection(&reader, &mut writer, &suggestions);
        assert!(ids.is_none());
    }

    #[test]
    fn elicit_skips_unrelated_messages() {
        let _guard = ID_LOCK.lock().unwrap();
        let suggestions = make_suggestions(2);
        // A notification (no id) arrives before our response — logged, not responded to.
        let notification = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "notifications/cancelled",
            "params": { "requestId": 999 }
        });
        let response = serde_json::json!({
            "jsonrpc": "2.0",
            "id": "s-1",
            "result": {
                "action": "accept",
                "content": { "selected": ["2"] }
            }
        });
        let line1 = serde_json::to_string(&notification).unwrap();
        let line2 = serde_json::to_string(&response).unwrap();
        let reader = LineReader::from_lines(&[&line1, &line2]);
        let mut writer = Vec::new();

        NEXT_REQUEST_ID.store(1, Ordering::Relaxed);
        let ids = elicit_selection(&reader, &mut writer, &suggestions);
        assert_eq!(ids, Some(vec![2]));

        // Notification has no id — no JSON-RPC error response should have been
        // written; only the elicitation request and a log notification.
        let written = String::from_utf8(writer).unwrap();
        let lines: Vec<&str> = written.lines().collect();
        // None of the output lines should be an error response.
        for line in &lines {
            if let Ok(v) = serde_json::from_str::<Value>(line) {
                assert!(v.get("error").is_none(), "unexpected error response: {line}");
            }
        }
    }

    #[test]
    fn elicit_responds_to_concurrent_request_with_server_busy() {
        let _guard = ID_LOCK.lock().unwrap();
        let suggestions = make_suggestions(2);
        // A concurrent *request* (has id) arrives before our elicitation response.
        let concurrent_request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": "other-42",
            "method": "tools/list",
            "params": {}
        });
        let response = serde_json::json!({
            "jsonrpc": "2.0",
            "id": "s-1",
            "result": {
                "action": "accept",
                "content": { "selected": ["1"] }
            }
        });
        let line1 = serde_json::to_string(&concurrent_request).unwrap();
        let line2 = serde_json::to_string(&response).unwrap();
        let reader = LineReader::from_lines(&[&line1, &line2]);
        let mut writer = Vec::new();

        NEXT_REQUEST_ID.store(1, Ordering::Relaxed);
        let ids = elicit_selection(&reader, &mut writer, &suggestions);
        assert_eq!(ids, Some(vec![1]));

        // The concurrent request must have received a server-busy error response
        // so the client is not left waiting indefinitely.
        let written = String::from_utf8(writer).unwrap();
        let busy_response = written
            .lines()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .find(|v| v.get("error").is_some());
        let busy = busy_response.expect("expected a server-busy error response");
        assert_eq!(busy["id"], "other-42");
        assert_eq!(busy["error"]["code"], -32000);
        assert!(busy["error"]["message"]
            .as_str()
            .unwrap()
            .contains("busy waiting for elicitation"));
    }

    #[test]
    fn format_selected_summary_empty() {
        assert_eq!(format_selected_summary(&[]), "No suggestions selected.");
    }

    #[test]
    fn format_selected_summary_with_items() {
        let suggestions = make_suggestions(2);
        let text = format_selected_summary(&suggestions);
        assert!(text.contains("2 suggestion(s) selected"));
        assert!(text.contains("suggestion 1"));
        assert!(text.contains("Replace with: \"fix_2\""));
    }

    // ── Helpers for grouped tests ─────────────────────────────────────────

    /// Create suggestions where multiple share the same lint code.
    /// Produces `n_codes` distinct codes with `per_code` suggestions each,
    /// plus `singletons` suggestions with unique codes.
    fn make_grouped_suggestions(
        n_codes: usize,
        per_code: usize,
        singletons: usize,
    ) -> Vec<Suggestion> {
        let mut suggestions = Vec::new();
        let mut id = 1;
        for c in 0..n_codes {
            for j in 0..per_code {
                suggestions.push(Suggestion {
                    id,
                    message: format!("fix variant {j}"),
                    parent_message: format!("lint_{c} description"),
                    code: Some(format!("clippy::lint_{c}")),
                    level: "warning".into(),
                    file: format!("src/file_{}.rs", j % 3),
                    line_start: id * 10,
                    column_start: 1,
                    replacement: Some(format!("fix_{id}")),
                    replacement_span: None,
                    replacements: vec![],
                    applicability: crate::suggest::Applicability::MaybeIncorrect,
                });
                id += 1;
            }
        }
        for s in 0..singletons {
            suggestions.push(Suggestion {
                id,
                message: format!("singleton {s}"),
                parent_message: format!("singleton {s}"),
                code: Some(format!("W_solo_{s}")),
                level: "warning".into(),
                file: format!("src/solo_{s}.rs"),
                line_start: id * 10,
                column_start: 1,
                replacement: Some(format!("solo_fix_{id}")),
                replacement_span: None,
                replacements: vec![],
                applicability: crate::suggest::Applicability::MaybeIncorrect,
            });
            id += 1;
        }
        suggestions
    }

    // ── GE-7: group_by_lint tests ─────────────────────────────────────────

    #[test]
    fn group_by_lint_creates_groups_by_code() {
        // 2 codes × 3 each = 6 suggestions in 2 groups.
        let suggestions = make_grouped_suggestions(2, 3, 0);
        let groups = group_by_lint(&suggestions);
        assert_eq!(groups.len(), 2);
        // Both groups have 3 members.
        assert_eq!(groups[0].members.len(), 3);
        assert_eq!(groups[1].members.len(), 3);
    }

    #[test]
    fn group_by_lint_singletons_are_separate() {
        // 1 code × 3, plus 2 singletons = 3 groups.
        let suggestions = make_grouped_suggestions(1, 3, 2);
        let groups = group_by_lint(&suggestions);
        assert_eq!(groups.len(), 3);
        assert_eq!(groups[0].members.len(), 3); // The big group.
        assert_eq!(groups[1].members.len(), 1);
        assert_eq!(groups[2].members.len(), 1);
    }

    #[test]
    fn group_by_lint_sorted_largest_first() {
        // 3 groups: sizes 5, 2, 1.
        let mut suggestions = Vec::new();
        let mut id = 1usize;
        for _ in 0..5 {
            suggestions.push(Suggestion {
                id,
                message: "m".into(),
                parent_message: "p".into(),
                code: Some("big".into()),
                level: "warning".into(),
                file: "a.rs".into(),
                line_start: id * 10,
                column_start: 1,
                replacement: None,
                replacement_span: None,
                replacements: vec![],
                applicability: crate::suggest::Applicability::MaybeIncorrect,
            });
            id += 1;
        }
        for _ in 0..2 {
            suggestions.push(Suggestion {
                id,
                message: "m".into(),
                parent_message: "p".into(),
                code: Some("mid".into()),
                level: "warning".into(),
                file: "b.rs".into(),
                line_start: id * 10,
                column_start: 1,
                replacement: None,
                replacement_span: None,
                replacements: vec![],
                applicability: crate::suggest::Applicability::MaybeIncorrect,
            });
            id += 1;
        }
        suggestions.push(Suggestion {
            id,
            message: "m".into(),
            parent_message: "p".into(),
            code: Some("small".into()),
            level: "warning".into(),
            file: "c.rs".into(),
            line_start: id * 10,
            column_start: 1,
            replacement: None,
            replacement_span: None,
            replacements: vec![],
            applicability: crate::suggest::Applicability::MaybeIncorrect,
        });
        let groups = group_by_lint(&suggestions);
        assert_eq!(groups.len(), 3);
        assert_eq!(groups[0].key, "big");
        assert_eq!(groups[0].members.len(), 5);
        assert_eq!(groups[1].key, "mid");
        assert_eq!(groups[1].members.len(), 2);
        assert_eq!(groups[2].key, "small");
        assert_eq!(groups[2].members.len(), 1);
    }

    #[test]
    fn group_by_lint_none_code_keyed_by_id() {
        let suggestions = vec![
            Suggestion {
                id: 1,
                message: "a".into(),
                parent_message: "a".into(),
                code: None,
                level: "warning".into(),
                file: "x.rs".into(),
                line_start: 10,
                column_start: 1,
                replacement: None,
                replacement_span: None,
                replacements: vec![],
                applicability: crate::suggest::Applicability::MaybeIncorrect,
            },
            Suggestion {
                id: 2,
                message: "b".into(),
                parent_message: "b".into(),
                code: None,
                level: "warning".into(),
                file: "y.rs".into(),
                line_start: 20,
                column_start: 1,
                replacement: None,
                replacement_span: None,
                replacements: vec![],
                applicability: crate::suggest::Applicability::MaybeIncorrect,
            },
        ];
        let groups = group_by_lint(&suggestions);
        // Each None-code suggestion is its own group.
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].members.len(), 1);
        assert_eq!(groups[1].members.len(), 1);
    }

    // ── GE-8: group_by_path tests ─────────────────────────────────────────

    #[test]
    fn group_by_path_creates_groups_by_file() {
        let suggestions = vec![
            Suggestion {
                id: 1,
                message: "a".into(),
                parent_message: "a".into(),
                code: Some("L1".into()),
                level: "warning".into(),
                file: "src/a.rs".into(),
                line_start: 10,
                column_start: 1,
                replacement: None,
                replacement_span: None,
                replacements: vec![],
                applicability: crate::suggest::Applicability::MaybeIncorrect,
            },
            Suggestion {
                id: 2,
                message: "b".into(),
                parent_message: "b".into(),
                code: Some("L2".into()),
                level: "warning".into(),
                file: "src/a.rs".into(),
                line_start: 20,
                column_start: 1,
                replacement: None,
                replacement_span: None,
                replacements: vec![],
                applicability: crate::suggest::Applicability::MaybeIncorrect,
            },
            Suggestion {
                id: 3,
                message: "c".into(),
                parent_message: "c".into(),
                code: Some("L1".into()),
                level: "warning".into(),
                file: "src/b.rs".into(),
                line_start: 30,
                column_start: 1,
                replacement: None,
                replacement_span: None,
                replacements: vec![],
                applicability: crate::suggest::Applicability::MaybeIncorrect,
            },
        ];
        let groups = group_by_path(&suggestions);
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].key, "src/a.rs");
        assert_eq!(groups[0].members.len(), 2);
        assert_eq!(groups[1].key, "src/b.rs");
        assert_eq!(groups[1].members.len(), 1);
    }

    #[test]
    fn group_by_path_sorted_largest_first() {
        // 3 in file_a, 1 in file_b → file_a first.
        let suggestions = make_grouped_suggestions(1, 3, 0);
        // All have file_0, file_1, file_2 cycling.
        let groups = group_by_path(&suggestions);
        // 3 suggestions across 3 files → 3 singletons, no dominant group.
        assert_eq!(groups.len(), 3);
        assert_eq!(groups[0].members.len(), 1);
    }

    // ── GE-9: build_grouped_schema tests ──────────────────────────────────

    #[test]
    fn grouped_schema_has_group_all_entries() {
        // 2 codes × 3 each = 6 suggestions.
        let suggestions = make_grouped_suggestions(2, 3, 0);
        let schema = build_grouped_schema(&suggestions, GroupMode::ByLint);
        let any_of = schema["properties"]["selected"]["items"]["anyOf"]
            .as_array()
            .unwrap();
        // 2 group headers + 6 individual + 1 mode switch + 1 skip-all = 10.
        assert_eq!(any_of.len(), 10);
        // First entry should be a group-all header.
        let first_const = any_of[0]["const"].as_str().unwrap();
        assert!(first_const.starts_with("all:"));
        // Title should mention "Select all" and instance count.
        let first_title = any_of[0]["title"].as_str().unwrap();
        assert!(first_title.contains("Select all"));
        assert!(first_title.contains("3 instances"));
    }

    #[test]
    fn grouped_schema_has_mode_switch_entry() {
        let suggestions = make_grouped_suggestions(1, 5, 0);
        let schema = build_grouped_schema(&suggestions, GroupMode::ByLint);
        let any_of = schema["properties"]["selected"]["items"]["anyOf"]
            .as_array()
            .unwrap();
        // Second-to-last entry should be the mode switch (last is skip-all).
        let second_last = &any_of[any_of.len() - 2];
        assert_eq!(second_last["const"].as_str().unwrap(), "view:by-path");
        assert!(
            second_last["title"]
                .as_str()
                .unwrap()
                .contains("Switch view")
        );
        // Last entry should be skip-all.
        let last = any_of.last().unwrap();
        assert_eq!(last["const"].as_str().unwrap(), SKIP_ALL_CONST);
    }

    #[test]
    fn grouped_schema_by_path_switch_shows_lint() {
        let suggestions = make_grouped_suggestions(1, 5, 0);
        let schema = build_grouped_schema(&suggestions, GroupMode::ByPath);
        let any_of = schema["properties"]["selected"]["items"]["anyOf"]
            .as_array()
            .unwrap();
        // Mode switch is second-to-last (skip-all is last).
        let second_last = &any_of[any_of.len() - 2];
        assert_eq!(second_last["const"].as_str().unwrap(), "view:by-lint");
        assert!(
            second_last["title"]
                .as_str()
                .unwrap()
                .contains("group by lint")
        );
    }

    #[test]
    fn grouped_schema_small_groups_no_header() {
        // 1 singleton should not get a group-all header.
        let suggestions = make_grouped_suggestions(0, 0, 5);
        let schema = build_grouped_schema(&suggestions, GroupMode::ByLint);
        let any_of = schema["properties"]["selected"]["items"]["anyOf"]
            .as_array()
            .unwrap();
        // 5 singletons + 0 group headers + 1 mode switch + 1 skip-all = 7.
        assert_eq!(any_of.len(), 7);
        // No "all:" entries.
        for opt in any_of {
            let c = opt["const"].as_str().unwrap();
            assert!(
                !c.starts_with("all:"),
                "singleton groups should not have all: header"
            );
        }
    }

    #[test]
    fn grouped_schema_individual_items_under_group_have_dash_prefix() {
        let suggestions = make_grouped_suggestions(1, 3, 0);
        let schema = build_grouped_schema(&suggestions, GroupMode::ByLint);
        let any_of = schema["properties"]["selected"]["items"]["anyOf"]
            .as_array()
            .unwrap();
        // Items at index 1, 2, 3 are individual items under the group.
        let title = any_of[1]["title"].as_str().unwrap();
        assert!(
            title.starts_with("  \u{2013} "),
            "individual items under a group should have dash prefix, got: {title}"
        );
    }

    // ── GE-10: parse_grouped_response tests ───────────────────────────────

    #[test]
    fn parse_response_individual_selection() {
        let suggestions = make_grouped_suggestions(2, 3, 0);
        let selected = ["1", "4"];
        match parse_grouped_response(&selected, &suggestions, GroupMode::ByLint) {
            GroupedSelection::Selected(ids) => assert_eq!(ids, vec![1, 4]),
            _ => panic!("expected Selected"),
        }
    }

    #[test]
    fn parse_response_group_all_expands() {
        let suggestions = make_grouped_suggestions(2, 3, 0);
        // "all:clippy::lint_0" should expand to IDs 1, 2, 3.
        let selected = ["all:clippy::lint_0"];
        match parse_grouped_response(&selected, &suggestions, GroupMode::ByLint) {
            GroupedSelection::Selected(ids) => assert_eq!(ids, vec![1, 2, 3]),
            _ => panic!("expected Selected"),
        }
    }

    #[test]
    fn parse_response_group_all_plus_individual_deduplicates() {
        let suggestions = make_grouped_suggestions(2, 3, 0);
        // Select all of group 0 (IDs 1,2,3) plus individual 2 → deduplicated.
        let selected = ["all:clippy::lint_0", "2"];
        match parse_grouped_response(&selected, &suggestions, GroupMode::ByLint) {
            GroupedSelection::Selected(ids) => assert_eq!(ids, vec![1, 2, 3]),
            _ => panic!("expected Selected"),
        }
    }

    #[test]
    fn parse_response_mode_switch_by_path() {
        let suggestions = make_grouped_suggestions(2, 3, 0);
        let selected = ["view:by-path"];
        match parse_grouped_response(&selected, &suggestions, GroupMode::ByLint) {
            GroupedSelection::ModeSwitch(m) => assert_eq!(m, GroupMode::ByPath),
            _ => panic!("expected ModeSwitch"),
        }
    }

    #[test]
    fn parse_response_mode_switch_by_lint() {
        let suggestions = make_grouped_suggestions(2, 3, 0);
        let selected = ["view:by-lint"];
        match parse_grouped_response(&selected, &suggestions, GroupMode::ByPath) {
            GroupedSelection::ModeSwitch(m) => assert_eq!(m, GroupMode::ByLint),
            _ => panic!("expected ModeSwitch"),
        }
    }

    #[test]
    fn parse_response_mode_switch_takes_priority_over_selections() {
        let suggestions = make_grouped_suggestions(2, 3, 0);
        // Mode switch + individual selection → mode switch wins.
        let selected = ["1", "view:by-path", "2"];
        match parse_grouped_response(&selected, &suggestions, GroupMode::ByLint) {
            GroupedSelection::ModeSwitch(m) => assert_eq!(m, GroupMode::ByPath),
            _ => panic!("expected ModeSwitch"),
        }
    }

    #[test]
    fn parse_response_empty_selection() {
        let suggestions = make_grouped_suggestions(2, 3, 0);
        let selected: [&str; 0] = [];
        match parse_grouped_response(&selected, &suggestions, GroupMode::ByLint) {
            GroupedSelection::Selected(ids) => assert!(ids.is_empty()),
            _ => panic!("expected Selected"),
        }
    }

    #[test]
    fn parse_response_unknown_group_key_ignored() {
        let suggestions = make_grouped_suggestions(1, 3, 0);
        let selected = ["all:nonexistent_lint", "1"];
        match parse_grouped_response(&selected, &suggestions, GroupMode::ByLint) {
            GroupedSelection::Selected(ids) => assert_eq!(ids, vec![1]),
            _ => panic!("expected Selected"),
        }
    }

    #[test]
    fn parse_response_by_path_group_all_expands() {
        // Two suggestions in the same file.
        let suggestions = vec![
            Suggestion {
                id: 1,
                message: "a".into(),
                parent_message: "a".into(),
                code: Some("L1".into()),
                level: "warning".into(),
                file: "src/foo.rs".into(),
                line_start: 10,
                column_start: 1,
                replacement: None,
                replacement_span: None,
                replacements: vec![],
                applicability: crate::suggest::Applicability::MaybeIncorrect,
            },
            Suggestion {
                id: 2,
                message: "b".into(),
                parent_message: "b".into(),
                code: Some("L2".into()),
                level: "warning".into(),
                file: "src/foo.rs".into(),
                line_start: 20,
                column_start: 1,
                replacement: None,
                replacement_span: None,
                replacements: vec![],
                applicability: crate::suggest::Applicability::MaybeIncorrect,
            },
        ];
        let selected = ["all:src/foo.rs"];
        match parse_grouped_response(&selected, &suggestions, GroupMode::ByPath) {
            GroupedSelection::Selected(ids) => assert_eq!(ids, vec![1, 2]),
            _ => panic!("expected Selected"),
        }
    }

    // ── GE-11: Full elicit_selection flow with grouping ───────────────────

    #[test]
    fn elicit_selection_below_threshold_uses_flat() {
        let _guard = ID_LOCK.lock().unwrap();
        // 3 suggestions — below GROUPING_THRESHOLD (5).
        let suggestions = make_suggestions(3);
        let response = serde_json::json!({
            "jsonrpc": "2.0",
            "id": "s-1",
            "result": {
                "action": "accept",
                "content": { "selected": ["2"] }
            }
        });
        let resp_str = serde_json::to_string(&response).unwrap();
        let reader = LineReader::from_lines(&[&resp_str]);
        let mut writer = Vec::new();
        NEXT_REQUEST_ID.store(1, Ordering::Relaxed);

        let ids = elicit_selection(&reader, &mut writer, &suggestions);
        assert_eq!(ids, Some(vec![2]));

        // The written request should NOT contain "all:" or "view:" entries.
        let written = String::from_utf8(writer).unwrap();
        assert!(!written.contains("\"all:"));
        assert!(!written.contains("\"view:"));
    }

    #[test]
    fn elicit_selection_above_threshold_uses_grouped() {
        let _guard = ID_LOCK.lock().unwrap();
        // 6 suggestions — above GROUPING_THRESHOLD.
        let suggestions = make_grouped_suggestions(2, 3, 0);
        let response = serde_json::json!({
            "jsonrpc": "2.0",
            "id": "s-1",
            "result": {
                "action": "accept",
                "content": { "selected": ["all:clippy::lint_0"] }
            }
        });
        let resp_str = serde_json::to_string(&response).unwrap();
        let reader = LineReader::from_lines(&[&resp_str]);
        let mut writer = Vec::new();
        NEXT_REQUEST_ID.store(1, Ordering::Relaxed);

        let ids = elicit_selection(&reader, &mut writer, &suggestions);
        assert_eq!(ids, Some(vec![1, 2, 3]));

        // The written request should contain grouped entries.
        let written = String::from_utf8(writer).unwrap();
        assert!(written.contains("\"all:"));
        assert!(written.contains("\"view:by-path\""));
    }

    #[test]
    fn elicit_selection_mode_switch_sends_second_request() {
        let _guard = ID_LOCK.lock().unwrap();
        let suggestions = make_grouped_suggestions(2, 3, 0);
        // First response: user selects mode switch.
        let switch_response = serde_json::json!({
            "jsonrpc": "2.0",
            "id": "s-1",
            "result": {
                "action": "accept",
                "content": { "selected": ["view:by-path"] }
            }
        });
        // Second response: user selects individual items.
        let final_response = serde_json::json!({
            "jsonrpc": "2.0",
            "id": "s-2",
            "result": {
                "action": "accept",
                "content": { "selected": ["4", "5"] }
            }
        });
        let line1 = serde_json::to_string(&switch_response).unwrap();
        let line2 = serde_json::to_string(&final_response).unwrap();
        let reader = LineReader::from_lines(&[&line1, &line2]);
        let mut writer = Vec::new();
        NEXT_REQUEST_ID.store(1, Ordering::Relaxed);

        let ids = elicit_selection(&reader, &mut writer, &suggestions);
        assert_eq!(ids, Some(vec![4, 5]));

        // Two elicitation requests should have been written (log notifications
        // from send_log_notification may also appear but are not counted).
        let written = String::from_utf8(writer).unwrap();
        let requests: Vec<&str> = written
            .trim()
            .split('\n')
            .filter(|l| l.contains("\"elicitation/create\""))
            .collect();
        assert_eq!(requests.len(), 2, "expected 2 requests for mode switch");
        // First request: by-lint (default), switch entry = view:by-path.
        assert!(requests[0].contains("view:by-path"));
        // Second request: by-path, switch entry = view:by-lint.
        assert!(requests[1].contains("view:by-lint"));
    }

    #[test]
    fn elicit_selection_mode_switch_decline_second_returns_none() {
        let _guard = ID_LOCK.lock().unwrap();
        let suggestions = make_grouped_suggestions(2, 3, 0);
        let switch_response = serde_json::json!({
            "jsonrpc": "2.0",
            "id": "s-1",
            "result": {
                "action": "accept",
                "content": { "selected": ["view:by-path"] }
            }
        });
        let decline_response = serde_json::json!({
            "jsonrpc": "2.0",
            "id": "s-2",
            "result": { "action": "decline" }
        });
        let line1 = serde_json::to_string(&switch_response).unwrap();
        let line2 = serde_json::to_string(&decline_response).unwrap();
        let reader = LineReader::from_lines(&[&line1, &line2]);
        let mut writer = Vec::new();
        NEXT_REQUEST_ID.store(1, Ordering::Relaxed);

        let ids = elicit_selection(&reader, &mut writer, &suggestions);
        assert!(ids.is_none());
    }
}
