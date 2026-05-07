// Copyright (c) Michael Grier. All rights reserved.

//! Parse Cargo NDJSON output and extract actionable suggestions.
//!
//! Cargo's `--message-format=json` produces one JSON object per line. Objects
//! with `"reason":"compiler-message"` carry a `message` block that may include
//! machine-applicable `span.suggested_replacement` fields. This module extracts
//! those into a flat [`Suggestion`] list suitable for presentation in an MCP
//! elicitation form.

use serde_json::Value;

/// Trust level of a compiler/clippy suggestion.
///
/// These mirror rustc's `Applicability` values. Changing any variant's meaning
/// is a breaking change for the tiered auto-apply logic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Applicability {
    /// The fix is machine-verified — safe to apply automatically.
    MachineApplicable,
    /// The fix may be incorrect — requires human review.
    MaybeIncorrect,
    /// The fix contains placeholders the user must fill in.
    HasPlaceholders,
}

impl Applicability {
    /// Parse from the `suggestion_applicability` string in Cargo JSON output.
    fn from_str(s: &str) -> Option<Self> {
        match s {
            "MachineApplicable" => Some(Self::MachineApplicable),
            "MaybeIncorrect" => Some(Self::MaybeIncorrect),
            "HasPlaceholders" => Some(Self::HasPlaceholders),
            _ => None,
        }
    }

    /// Short tag for UI display (empty for MachineApplicable since it's the default).
    pub fn trust_tag(self) -> &'static str {
        match self {
            Self::MachineApplicable => "",
            Self::MaybeIncorrect => " [maybe-incorrect]",
            Self::HasPlaceholders => " [has-placeholders]",
        }
    }

    /// Natural-language label for human-facing UI (empty for MachineApplicable).
    pub fn human_label(self) -> &'static str {
        match self {
            Self::MachineApplicable => "",
            Self::MaybeIncorrect => "may be incorrect",
            Self::HasPlaceholders => "has placeholders",
        }
    }
}

/// A single actionable suggestion extracted from a compiler/clippy diagnostic.
#[derive(Debug, Clone)]
pub struct Suggestion {
    /// 1-based index, assigned after extraction.
    pub id: usize,
    /// Human-readable summary (the compiler/clippy message text).
    /// When extracted from a child span, this is the child's message (often terse).
    pub message: String,
    /// The root diagnostic message (more descriptive than `message` for child spans).
    /// For root-level spans this equals `message`.
    pub parent_message: String,
    /// Lint or error code, e.g. `"clippy::redundant_clone"` or `"E0308"`.
    pub code: Option<String>,
    /// Severity level: `"error"`, `"warning"`, `"note"`, etc.
    #[allow(dead_code)] // Used for filtering/display in future; kept for completeness.
    pub level: String,
    /// File path as reported by the compiler (workspace-relative or absolute).
    pub file: String,
    /// 1-based starting line.
    pub line_start: usize,
    /// 1-based starting column.
    pub column_start: usize,
    /// The replacement text, if the compiler provides a machine-applicable fix.
    pub replacement: Option<String>,
    /// Span of the text to replace (only present when `replacement` is `Some`).
    pub replacement_span: Option<Span>,
    /// All replacement spans for this suggestion.
    ///
    /// When this has more than one entry the spans form a multi-span suggestion
    /// that must be applied atomically. `replacement` and `replacement_span`
    /// point to the first span for backward compatibility with single-span
    /// display code.
    pub replacements: Vec<SpanReplacement>,
    /// Trust level of this suggestion.
    pub applicability: Applicability,
}

/// A source span (byte-level range in a file).
#[derive(Debug, Clone)]
pub struct Span {
    pub file: String,
    pub line_start: usize,
    pub line_end: usize,
    pub column_start: usize,
    pub column_end: usize,
}

/// A single replacement action within a multi-span suggestion.
///
/// Multi-span suggestions contain several [`SpanReplacement`]s that must be
/// applied atomically; applying only a subset leaves the source invalid.
#[derive(Debug, Clone)]
pub struct SpanReplacement {
    /// The source location to replace.
    pub span: Span,
    /// Text to write into the span (empty string = deletion).
    pub text: String,
}

/// Parse NDJSON output and return all actionable suggestions.
///
/// An "actionable" suggestion is a `compiler-message` whose primary span has a
/// `suggested_replacement` with `applicability` of `"MachineApplicable"`,
/// `"MaybeIncorrect"`, or `"HasPlaceholders"`. We skip `"Unspecified"` because
/// those are not reliably auto-applicable.
///
/// Suggestions are numbered starting from 1.
pub fn extract_suggestions(ndjson: &str) -> Vec<Suggestion> {
    let mut suggestions = Vec::new();

    for line in ndjson.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        let obj: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };

        if obj.get("reason").and_then(|v| v.as_str()) != Some("compiler-message") {
            continue;
        }

        let msg = match obj.get("message") {
            Some(m) => m,
            None => continue,
        };

        collect_suggestions_from_message(msg, &mut suggestions);
    }

    // Assign 1-based IDs.
    for (i, s) in suggestions.iter_mut().enumerate() {
        s.id = i + 1;
    }

    suggestions
}

/// Recursively walk a diagnostic message and its children to find suggestions.
fn collect_suggestions_from_message(msg: &Value, out: &mut Vec<Suggestion>) {
    let level = msg
        .get("level")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_owned();
    let message_text = msg
        .get("message")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_owned();
    let code = msg
        .get("code")
        .and_then(|v| v.get("code"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_owned());

    // Collect all root-level replacement spans into a single Suggestion.
    // Rustc can represent a single multipart fix as several root spans with
    // suggested_replacement; splitting them into separate Suggestions would
    // let the caller apply only part of the fix and leave the source invalid.
    if let Some(spans) = msg.get("spans").and_then(|v| v.as_array()) {
        let mut replacements: Vec<SpanReplacement> = Vec::new();
        let mut applicability_opt: Option<Applicability> = None;

        for span in spans {
            let replacement_text = match span.get("suggested_replacement").and_then(|v| v.as_str())
            {
                Some(t) => t,
                None => continue,
            };
            let applicability_str = span
                .get("suggestion_applicability")
                .and_then(|v| v.as_str())
                .unwrap_or("Unspecified");
            let applicability = match Applicability::from_str(applicability_str) {
                Some(a) => a,
                None => continue,
            };
            let file = span
                .get("file_name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_owned();
            let line_start = span.get("line_start").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
            let line_end = span.get("line_end").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
            let column_start = span
                .get("column_start")
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as usize;
            let column_end = span.get("column_end").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
            if applicability_opt.is_none() {
                applicability_opt = Some(applicability);
            }
            replacements.push(SpanReplacement {
                span: Span {
                    file,
                    line_start,
                    line_end,
                    column_start,
                    column_end,
                },
                text: replacement_text.to_owned(),
            });
        }

        if !replacements.is_empty() {
            let applicability = applicability_opt.unwrap();
            let first = &replacements[0];
            out.push(Suggestion {
                id: 0, // assigned later
                message: message_text.clone(),
                parent_message: message_text.clone(),
                code: code.clone(),
                level: level.clone(),
                file: first.span.file.clone(),
                line_start: first.span.line_start,
                column_start: first.span.column_start,
                replacement: Some(first.text.clone()),
                replacement_span: Some(first.span.clone()),
                replacements,
                applicability,
            });
        }
    }

    // Recurse into child diagnostics (e.g. "help: consider removing this").
    if let Some(children) = msg.get("children").and_then(|v| v.as_array()) {
        for child in children {
            collect_from_child(child, &message_text, &code, &level, &message_text, out);
        }
    }
}

/// Extract suggestions from a child diagnostic (e.g. "help:" messages).
///
/// Children often carry the actual suggested replacement while the parent has
/// the high-level message. We use the child's message but fall back to the
/// parent code/level if the child doesn't have them.
///
/// A single child may carry multiple spans that together form one atomic
/// suggestion (e.g. a rename that must touch both ends of a pair). We collect
/// all replacement spans into a single [`Suggestion`] so they are presented as
/// one unit and must be applied together. Splitting them into separate
/// suggestions and applying only some would leave the source invalid.
fn collect_from_child(
    child: &Value,
    parent_message: &str,
    parent_code: &Option<String>,
    parent_level: &str,
    origin_message: &str,
    out: &mut Vec<Suggestion>,
) {
    let child_message = child
        .get("message")
        .and_then(|v| v.as_str())
        .unwrap_or(parent_message);
    let child_code = child
        .get("code")
        .and_then(|v| v.get("code"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_owned())
        .or_else(|| parent_code.clone());
    let child_level = child
        .get("level")
        .and_then(|v| v.as_str())
        .unwrap_or(parent_level);

    if let Some(spans) = child.get("spans").and_then(|v| v.as_array()) {
        // Collect all replacement spans for this child into a single Suggestion.
        let mut replacements: Vec<SpanReplacement> = Vec::new();
        let mut applicability_opt: Option<Applicability> = None;

        for span in spans {
            let replacement_text = match span.get("suggested_replacement").and_then(|v| v.as_str())
            {
                Some(t) => t,
                None => continue,
            };
            let applicability_str = span
                .get("suggestion_applicability")
                .and_then(|v| v.as_str())
                .unwrap_or("Unspecified");
            let applicability = match Applicability::from_str(applicability_str) {
                Some(a) => a,
                None => continue,
            };
            let file = span
                .get("file_name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_owned();
            let line_start = span.get("line_start").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
            let line_end = span.get("line_end").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
            let column_start = span
                .get("column_start")
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as usize;
            let column_end = span.get("column_end").and_then(|v| v.as_u64()).unwrap_or(0) as usize;

            if applicability_opt.is_none() {
                applicability_opt = Some(applicability);
            }
            replacements.push(SpanReplacement {
                span: Span {
                    file,
                    line_start,
                    line_end,
                    column_start,
                    column_end,
                },
                text: replacement_text.to_owned(),
            });
        }

        if !replacements.is_empty() {
            let applicability = applicability_opt.unwrap();
            let first = &replacements[0];
            out.push(Suggestion {
                id: 0, // assigned later
                message: child_message.to_owned(),
                parent_message: origin_message.to_owned(),
                code: child_code.clone(),
                level: child_level.to_owned(),
                file: first.span.file.clone(),
                line_start: first.span.line_start,
                column_start: first.span.column_start,
                replacement: Some(first.text.clone()),
                replacement_span: Some(first.span.clone()),
                replacements,
                applicability,
            });
        }
    }

    // Children can have children too.
    if let Some(grandchildren) = child.get("children").and_then(|v| v.as_array()) {
        for gc in grandchildren {
            collect_from_child(
                gc,
                child_message,
                &child_code,
                child_level,
                origin_message,
                out,
            );
        }
    }
}

/// Format a suggestion as a short one-line label for UI display.
pub fn suggestion_label(s: &Suggestion) -> String {
    let code_part = match &s.code {
        Some(c) => format!("[{c}] "),
        None => String::new(),
    };
    format!(
        "{code_part}{msg}{trust} ({file}:{line}:{col})",
        msg = s.message,
        trust = s.applicability.trust_tag(),
        file = s.file,
        line = s.line_start,
        col = s.column_start,
    )
}

/// Extract just the filename from a workspace-relative or absolute path.
fn short_filename(path: &str) -> &str {
    // DEMO ONLY -- revert before commit. Backslash separator removed so the
    // Windows-path test fails, producing a single nicely-formatted failure
    // for the README screenshot.
    path.rsplit('/').next().unwrap_or(path)
}

/// Format a suggestion as a human-readable label for elicitation checkboxes.
///
/// Compared to [`suggestion_label`], this:
/// - Uses the descriptive parent message instead of the terse child message
/// - Shows just the filename, not the full relative path
/// - Uses natural language for the trust level
pub fn elicitation_label(s: &Suggestion) -> String {
    let filename = short_filename(&s.file);
    // Prefer the parent (root diagnostic) message — it's more descriptive.
    let desc = if !s.parent_message.is_empty() && s.parent_message != s.message {
        &s.parent_message
    } else {
        &s.message
    };
    let trust = s.applicability.human_label();
    if trust.is_empty() {
        format!("{filename}:{line} \u{2014} {desc}", line = s.line_start)
    } else {
        format!(
            "{filename}:{line} \u{2014} {desc} ({trust})",
            line = s.line_start
        )
    }
}

/// Format all suggestions as a numbered text list (fallback when elicitation
/// is not available).
pub fn format_numbered_list(suggestions: &[Suggestion]) -> String {
    let mut buf = String::new();
    for s in suggestions {
        buf.push_str(&format!("  {}. {}\n", s.id, suggestion_label(s)));
        if let Some(ref repl) = s.replacement {
            let display = if repl.is_empty() {
                "(remove text)".to_owned()
            } else if repl.len() <= 80 {
                format!("     replacement: {repl:?}")
            } else {
                let truncated: String = repl.chars().take(77).collect();
                format!("     replacement: {truncated:?}...")
            };
            buf.push_str(&display);
            buf.push('\n');
        }
    }
    buf
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A realistic NDJSON snippet from `cargo clippy --message-format=json`.
    const CLIPPY_NDJSON: &str = r#"{"reason":"compiler-message","package_id":"test","manifest_path":"test","target":{"kind":["lib"],"crate_types":["lib"],"name":"test","src_path":"src/lib.rs","edition":"2021","doc":true,"doctest":true,"test":true},"message":{"rendered":"warning: redundant clone\n","children":[{"children":[],"code":null,"level":"help","message":"remove this","rendered":null,"spans":[{"byte_end":100,"byte_start":95,"column_end":10,"column_start":5,"expansion":null,"file_name":"src/lib.rs","is_primary":true,"label":null,"line_end":10,"line_start":10,"suggested_replacement":"","suggestion_applicability":"MachineApplicable","text":[]}]}],"code":{"code":"clippy::redundant_clone","explanation":null},"level":"warning","message":"redundant clone","spans":[{"byte_end":100,"byte_start":90,"column_end":10,"column_start":1,"expansion":null,"file_name":"src/lib.rs","is_primary":true,"label":null,"line_end":10,"line_start":10,"suggested_replacement":null,"suggestion_applicability":null,"text":[]}]}}"#;

    const ERROR_NDJSON: &str = r#"{"reason":"compiler-message","package_id":"test","manifest_path":"test","target":{"kind":["bin"],"crate_types":["bin"],"name":"test","src_path":"src/main.rs","edition":"2021","doc":true,"doctest":false,"test":true},"message":{"rendered":"error: expected `,`\n","children":[{"children":[],"code":null,"level":"help","message":"add a comma","rendered":null,"spans":[{"byte_end":50,"byte_start":49,"column_end":20,"column_start":19,"expansion":null,"file_name":"src/main.rs","is_primary":true,"label":null,"line_end":15,"line_start":15,"suggested_replacement":",","suggestion_applicability":"MachineApplicable","text":[]}]}],"code":{"code":"E0Expected","explanation":null},"level":"error","message":"expected one of `,`, `::`, `as`, or `}`","spans":[{"byte_end":55,"byte_start":50,"column_end":25,"column_start":20,"expansion":null,"file_name":"src/main.rs","is_primary":true,"label":null,"line_end":15,"line_start":15,"suggested_replacement":null,"suggestion_applicability":null,"text":[]}]}}"#;

    #[test]
    fn extract_clippy_suggestion() {
        let suggestions = extract_suggestions(CLIPPY_NDJSON);
        assert_eq!(suggestions.len(), 1);
        let s = &suggestions[0];
        assert_eq!(s.id, 1);
        assert_eq!(s.code.as_deref(), Some("clippy::redundant_clone"));
        assert_eq!(s.file, "src/lib.rs");
        assert_eq!(s.line_start, 10);
        assert_eq!(s.replacement.as_deref(), Some(""));
        assert!(s.replacement_span.is_some());
    }

    #[test]
    fn extract_error_suggestion() {
        let suggestions = extract_suggestions(ERROR_NDJSON);
        assert_eq!(suggestions.len(), 1);
        let s = &suggestions[0];
        assert_eq!(s.replacement.as_deref(), Some(","));
        assert_eq!(s.file, "src/main.rs");
        assert_eq!(s.line_start, 15);
        assert_eq!(s.column_start, 19);
    }

    #[test]
    fn no_suggestions_from_artifact_lines() {
        let ndjson = concat!(
            r#"{"reason":"compiler-artifact","package_id":"serde"}"#,
            "\n",
            r#"{"reason":"build-finished","success":true}"#,
            "\n",
        );
        let suggestions = extract_suggestions(ndjson);
        assert!(suggestions.is_empty());
    }

    #[test]
    fn empty_input() {
        assert!(extract_suggestions("").is_empty());
        assert!(extract_suggestions("\n\n").is_empty());
    }

    #[test]
    fn malformed_lines_skipped() {
        let input = "not json\n{\"reason\":\"build-finished\"}\n";
        assert!(extract_suggestions(input).is_empty());
    }

    #[test]
    fn suggestion_label_with_code() {
        let s = Suggestion {
            id: 1,
            message: "redundant clone".into(),
            parent_message: "redundant clone".into(),
            code: Some("clippy::redundant_clone".into()),
            level: "warning".into(),
            file: "src/lib.rs".into(),
            line_start: 42,
            column_start: 5,
            replacement: Some(String::new()),
            replacement_span: None,
            replacements: vec![],
            applicability: Applicability::MachineApplicable,
        };
        let label = suggestion_label(&s);
        assert!(label.contains("clippy::redundant_clone"));
        assert!(label.contains("src/lib.rs:42:5"));
    }

    #[test]
    fn suggestion_label_without_code() {
        let s = Suggestion {
            id: 1,
            message: "expected comma".into(),
            parent_message: "expected comma".into(),
            code: None,
            level: "error".into(),
            file: "src/main.rs".into(),
            line_start: 15,
            column_start: 20,
            replacement: Some(",".into()),
            replacement_span: None,
            replacements: vec![],
            applicability: Applicability::MachineApplicable,
        };
        let label = suggestion_label(&s);
        assert!(!label.contains("[]"));
        assert!(label.contains("expected comma"));
        assert!(label.contains("src/main.rs:15:20"));
    }

    #[test]
    fn numbered_list_format() {
        let suggestions = vec![
            Suggestion {
                id: 1,
                message: "warning A".into(),
                parent_message: "warning A".into(),
                code: Some("W001".into()),
                level: "warning".into(),
                file: "a.rs".into(),
                line_start: 1,
                column_start: 1,
                replacement: Some("fix_a".into()),
                replacement_span: None,
                replacements: vec![],
                applicability: Applicability::MachineApplicable,
            },
            Suggestion {
                id: 2,
                message: "warning B".into(),
                parent_message: "warning B".into(),
                code: None,
                level: "warning".into(),
                file: "b.rs".into(),
                line_start: 5,
                column_start: 3,
                replacement: None,
                replacement_span: None,
                replacements: vec![],
                applicability: Applicability::MaybeIncorrect,
            },
        ];
        let text = format_numbered_list(&suggestions);
        assert!(text.contains("1. [W001] warning A"));
        assert!(text.contains("2. warning B"));
        assert!(text.contains("replacement: \"fix_a\""));
    }

    #[test]
    fn multiple_suggestions_from_one_message() {
        // A message with two children, each with a suggested replacement.
        let ndjson = r#"{"reason":"compiler-message","package_id":"test","manifest_path":"test","target":{"kind":["lib"],"crate_types":["lib"],"name":"test","src_path":"src/lib.rs","edition":"2021","doc":true,"doctest":true,"test":true},"message":{"rendered":"warning\n","children":[{"children":[],"code":null,"level":"help","message":"fix A","rendered":null,"spans":[{"byte_end":10,"byte_start":5,"column_end":6,"column_start":1,"expansion":null,"file_name":"src/lib.rs","is_primary":true,"label":null,"line_end":1,"line_start":1,"suggested_replacement":"A","suggestion_applicability":"MachineApplicable","text":[]}]},{"children":[],"code":null,"level":"help","message":"fix B","rendered":null,"spans":[{"byte_end":20,"byte_start":15,"column_end":6,"column_start":1,"expansion":null,"file_name":"src/lib.rs","is_primary":true,"label":null,"line_end":2,"line_start":2,"suggested_replacement":"B","suggestion_applicability":"MachineApplicable","text":[]}]}],"code":{"code":"test_lint","explanation":null},"level":"warning","message":"test warning","spans":[]}}"#;
        let suggestions = extract_suggestions(ndjson);
        assert_eq!(suggestions.len(), 2);
        assert_eq!(suggestions[0].id, 1);
        assert_eq!(suggestions[0].message, "fix A");
        assert_eq!(suggestions[1].id, 2);
        assert_eq!(suggestions[1].message, "fix B");
    }

    #[test]
    fn unspecified_applicability_skipped() {
        let ndjson = r#"{"reason":"compiler-message","package_id":"test","manifest_path":"test","target":{"kind":["lib"],"crate_types":["lib"],"name":"test","src_path":"src/lib.rs","edition":"2021","doc":true,"doctest":true,"test":true},"message":{"rendered":"note\n","children":[{"children":[],"code":null,"level":"note","message":"try this","rendered":null,"spans":[{"byte_end":10,"byte_start":5,"column_end":6,"column_start":1,"expansion":null,"file_name":"src/lib.rs","is_primary":true,"label":null,"line_end":1,"line_start":1,"suggested_replacement":"x","suggestion_applicability":"Unspecified","text":[]}]}],"code":null,"level":"note","message":"note msg","spans":[]}}"#;
        let suggestions = extract_suggestions(ndjson);
        assert!(suggestions.is_empty());
    }

    #[test]
    fn removal_suggestion_display() {
        let s = Suggestion {
            id: 1,
            message: "remove this".into(),
            parent_message: "remove this".into(),
            code: None,
            level: "help".into(),
            file: "x.rs".into(),
            line_start: 1,
            column_start: 1,
            replacement: Some(String::new()),
            replacement_span: None,
            replacements: vec![],
            applicability: Applicability::MachineApplicable,
        };
        let text = format_numbered_list(&[s]);
        assert!(text.contains("(remove text)"));
    }

    #[test]
    fn applicability_from_str_known_values() {
        assert_eq!(
            Applicability::from_str("MachineApplicable"),
            Some(Applicability::MachineApplicable)
        );
        assert_eq!(
            Applicability::from_str("MaybeIncorrect"),
            Some(Applicability::MaybeIncorrect)
        );
        assert_eq!(
            Applicability::from_str("HasPlaceholders"),
            Some(Applicability::HasPlaceholders)
        );
    }

    #[test]
    fn applicability_from_str_unknown_returns_none() {
        assert_eq!(Applicability::from_str("Unspecified"), None);
        assert_eq!(Applicability::from_str(""), None);
        assert_eq!(Applicability::from_str("machineapplicable"), None);
    }

    #[test]
    fn trust_tag_machine_applicable_is_empty() {
        assert_eq!(Applicability::MachineApplicable.trust_tag(), "");
    }

    #[test]
    fn trust_tag_maybe_incorrect() {
        assert!(
            Applicability::MaybeIncorrect
                .trust_tag()
                .contains("maybe-incorrect")
        );
    }

    #[test]
    fn trust_tag_has_placeholders() {
        assert!(
            Applicability::HasPlaceholders
                .trust_tag()
                .contains("has-placeholders")
        );
    }

    #[test]
    fn extracted_suggestion_has_correct_applicability() {
        let suggestions = extract_suggestions(CLIPPY_NDJSON);
        assert_eq!(suggestions.len(), 1);
        assert_eq!(
            suggestions[0].applicability,
            Applicability::MachineApplicable
        );
    }

    #[test]
    fn maybe_incorrect_applicability_extracted() {
        let ndjson = r#"{"reason":"compiler-message","package_id":"test","manifest_path":"test","target":{"kind":["lib"],"crate_types":["lib"],"name":"test","src_path":"src/lib.rs","edition":"2021","doc":true,"doctest":true,"test":true},"message":{"rendered":"warning\n","children":[{"children":[],"code":null,"level":"help","message":"try this","rendered":null,"spans":[{"byte_end":10,"byte_start":5,"column_end":6,"column_start":1,"expansion":null,"file_name":"src/lib.rs","is_primary":true,"label":null,"line_end":1,"line_start":1,"suggested_replacement":"maybe","suggestion_applicability":"MaybeIncorrect","text":[]}]}],"code":{"code":"test_lint","explanation":null},"level":"warning","message":"test warning","spans":[]}}"#;
        let suggestions = extract_suggestions(ndjson);
        assert_eq!(suggestions.len(), 1);
        assert_eq!(suggestions[0].applicability, Applicability::MaybeIncorrect);
        assert_eq!(suggestions[0].replacement.as_deref(), Some("maybe"));
    }

    #[test]
    fn has_placeholders_applicability_extracted() {
        let ndjson = r#"{"reason":"compiler-message","package_id":"test","manifest_path":"test","target":{"kind":["lib"],"crate_types":["lib"],"name":"test","src_path":"src/lib.rs","edition":"2021","doc":true,"doctest":true,"test":true},"message":{"rendered":"warning\n","children":[{"children":[],"code":null,"level":"help","message":"add type","rendered":null,"spans":[{"byte_end":10,"byte_start":5,"column_end":6,"column_start":1,"expansion":null,"file_name":"src/lib.rs","is_primary":true,"label":null,"line_end":1,"line_start":1,"suggested_replacement":"T","suggestion_applicability":"HasPlaceholders","text":[]}]}],"code":{"code":"test_lint","explanation":null},"level":"warning","message":"test warning","spans":[]}}"#;
        let suggestions = extract_suggestions(ndjson);
        assert_eq!(suggestions.len(), 1);
        assert_eq!(suggestions[0].applicability, Applicability::HasPlaceholders);
    }

    #[test]
    fn label_includes_trust_tag_for_maybe_incorrect() {
        let s = Suggestion {
            id: 1,
            message: "try this".into(),
            parent_message: "use map_or_else instead".into(),
            code: Some("W001".into()),
            level: "warning".into(),
            file: "a.rs".into(),
            line_start: 1,
            column_start: 1,
            replacement: Some("x".into()),
            replacement_span: None,
            replacements: vec![],
            applicability: Applicability::MaybeIncorrect,
        };
        let label = suggestion_label(&s);
        assert!(label.contains("[maybe-incorrect]"));
        assert!(label.contains("[W001]"));
    }

    #[test]
    fn label_no_trust_tag_for_machine_applicable() {
        let s = Suggestion {
            id: 1,
            message: "fix".into(),
            parent_message: "fix".into(),
            code: None,
            level: "help".into(),
            file: "b.rs".into(),
            line_start: 1,
            column_start: 1,
            replacement: Some("y".into()),
            replacement_span: None,
            replacements: vec![],
            applicability: Applicability::MachineApplicable,
        };
        let label = suggestion_label(&s);
        assert!(!label.contains("[maybe-incorrect]"));
        assert!(!label.contains("[has-placeholders]"));
    }

    #[test]
    fn short_filename_unix_path() {
        assert_eq!(short_filename("src/lib.rs"), "lib.rs");
    }

    #[test]
    fn short_filename_windows_path() {
        assert_eq!(
            short_filename("crates\\cargo-mcp\\src\\invoke.rs"),
            "invoke.rs"
        );
    }

    #[test]
    fn short_filename_just_filename() {
        assert_eq!(short_filename("main.rs"), "main.rs");
    }

    #[test]
    fn short_filename_empty() {
        assert_eq!(short_filename(""), "");
    }

    #[test]
    fn human_label_machine_applicable_is_empty() {
        assert_eq!(Applicability::MachineApplicable.human_label(), "");
    }

    #[test]
    fn human_label_maybe_incorrect() {
        assert_eq!(
            Applicability::MaybeIncorrect.human_label(),
            "may be incorrect"
        );
    }

    #[test]
    fn human_label_has_placeholders() {
        assert_eq!(
            Applicability::HasPlaceholders.human_label(),
            "has placeholders"
        );
    }

    #[test]
    fn elicitation_label_uses_parent_message_when_different() {
        let s = Suggestion {
            id: 1,
            message: "try".into(),
            parent_message: "use Option::map_or_else instead of an if let/else".into(),
            code: Some("clippy::option_if_let_else".into()),
            level: "warning".into(),
            file: "crates/cargo-mcp/src/invoke.rs".into(),
            line_start: 74,
            column_start: 5,
            replacement: Some("map_or_else(...)".into()),
            replacement_span: None,
            replacements: vec![],
            applicability: Applicability::MaybeIncorrect,
        };
        let label = elicitation_label(&s);
        assert!(label.contains("invoke.rs:74"));
        assert!(label.contains("use Option::map_or_else"));
        assert!(label.contains("may be incorrect"));
        // Should NOT contain the terse child message "try" — parent is preferred.
        assert!(!label.contains(" try "));
    }

    #[test]
    fn elicitation_label_falls_back_to_message_when_same() {
        let s = Suggestion {
            id: 1,
            message: "redundant clone".into(),
            parent_message: "redundant clone".into(),
            code: Some("clippy::redundant_clone".into()),
            level: "warning".into(),
            file: "src/lib.rs".into(),
            line_start: 10,
            column_start: 5,
            replacement: Some(String::new()),
            replacement_span: None,
            replacements: vec![],
            applicability: Applicability::MachineApplicable,
        };
        let label = elicitation_label(&s);
        assert!(label.contains("lib.rs:10"));
        assert!(label.contains("redundant clone"));
        // MachineApplicable — no trust suffix.
        assert!(!label.contains("may be incorrect"));
        assert!(!label.contains("has placeholders"));
    }

    #[test]
    fn elicitation_label_has_placeholders_trust() {
        let s = Suggestion {
            id: 1,
            message: "add type annotation".into(),
            parent_message: "missing type for item".into(),
            code: None,
            level: "error".into(),
            file: "src/main.rs".into(),
            line_start: 42,
            column_start: 1,
            replacement: Some("T".into()),
            replacement_span: None,
            replacements: vec![],
            applicability: Applicability::HasPlaceholders,
        };
        let label = elicitation_label(&s);
        assert!(label.contains("main.rs:42"));
        assert!(label.contains("missing type for item"));
        assert!(label.contains("has placeholders"));
    }

    #[test]
    fn parent_message_threaded_from_ndjson_extraction() {
        // The root message "test warning" is the descriptive parent; child says "fix A".
        let ndjson = r#"{"reason":"compiler-message","package_id":"test","manifest_path":"test","target":{"kind":["lib"],"crate_types":["lib"],"name":"test","src_path":"src/lib.rs","edition":"2021","doc":true,"doctest":true,"test":true},"message":{"rendered":"warning\n","children":[{"children":[],"code":null,"level":"help","message":"fix A","rendered":null,"spans":[{"byte_end":10,"byte_start":5,"column_end":6,"column_start":1,"expansion":null,"file_name":"src/lib.rs","is_primary":true,"label":null,"line_end":1,"line_start":1,"suggested_replacement":"A","suggestion_applicability":"MachineApplicable","text":[]}]}],"code":{"code":"test_lint","explanation":null},"level":"warning","message":"test warning","spans":[]}}"#;
        let suggestions = extract_suggestions(ndjson);
        assert_eq!(suggestions.len(), 1);
        assert_eq!(suggestions[0].message, "fix A");
        assert_eq!(suggestions[0].parent_message, "test warning");
    }

    #[test]
    fn parent_message_same_for_root_level_span() {
        // When the suggestion is on the root span (not a child), both messages should match.
        let ndjson = r#"{"reason":"compiler-message","package_id":"test","manifest_path":"test","target":{"kind":["lib"],"crate_types":["lib"],"name":"test","src_path":"src/lib.rs","edition":"2021","doc":true,"doctest":true,"test":true},"message":{"rendered":"warning\n","children":[],"code":{"code":"test_lint","explanation":null},"level":"warning","message":"root message","spans":[{"byte_end":10,"byte_start":5,"column_end":6,"column_start":1,"expansion":null,"file_name":"src/lib.rs","is_primary":true,"label":null,"line_end":1,"line_start":1,"suggested_replacement":"fix","suggestion_applicability":"MachineApplicable","text":[]}]}}"#;
        let suggestions = extract_suggestions(ndjson);
        assert_eq!(suggestions.len(), 1);
        assert_eq!(suggestions[0].message, "root message");
        assert_eq!(suggestions[0].parent_message, "root message");
    }

    #[test]
    fn root_multispan_forms_single_atomic_suggestion() {
        // A rename fix that touches two locations: rustc emits both as root spans
        // with suggested_replacement. They must be one atomic Suggestion, not two.
        let ndjson = r#"{"reason":"compiler-message","package_id":"test","manifest_path":"test","target":{"kind":["lib"],"crate_types":["lib"],"name":"test","src_path":"src/lib.rs","edition":"2021","doc":true,"doctest":true,"test":true},"message":{"rendered":"warning\n","children":[],"code":{"code":"test_rename","explanation":null},"level":"warning","message":"rename foo to bar","spans":[{"byte_end":10,"byte_start":7,"column_end":4,"column_start":1,"expansion":null,"file_name":"src/lib.rs","is_primary":true,"label":null,"line_end":1,"line_start":1,"suggested_replacement":"bar","suggestion_applicability":"MachineApplicable","text":[]},{"byte_end":30,"byte_start":27,"column_end":4,"column_start":1,"expansion":null,"file_name":"src/lib.rs","is_primary":false,"label":null,"line_end":5,"line_start":5,"suggested_replacement":"bar","suggestion_applicability":"MachineApplicable","text":[]}]}}"#;
        let suggestions = extract_suggestions(ndjson);
        // Both spans belong to the same atomic rename — must be a single Suggestion.
        assert_eq!(
            suggestions.len(),
            1,
            "multipart root-span fix must not be split"
        );
        assert_eq!(suggestions[0].replacements.len(), 2);
        // First replacement is from span 1 (line 1).
        assert_eq!(suggestions[0].replacements[0].span.line_start, 1);
        // Second replacement is from span 2 (line 5).
        assert_eq!(suggestions[0].replacements[1].span.line_start, 5);
    }

    #[test]
    fn mixed_applicability_from_multiple_children() {
        // One MachineApplicable and one MaybeIncorrect in the same message.
        let ndjson = r#"{"reason":"compiler-message","package_id":"test","manifest_path":"test","target":{"kind":["lib"],"crate_types":["lib"],"name":"test","src_path":"src/lib.rs","edition":"2021","doc":true,"doctest":true,"test":true},"message":{"rendered":"warning\n","children":[{"children":[],"code":null,"level":"help","message":"fix A","rendered":null,"spans":[{"byte_end":10,"byte_start":5,"column_end":6,"column_start":1,"expansion":null,"file_name":"src/lib.rs","is_primary":true,"label":null,"line_end":1,"line_start":1,"suggested_replacement":"A","suggestion_applicability":"MachineApplicable","text":[]}]},{"children":[],"code":null,"level":"help","message":"fix B","rendered":null,"spans":[{"byte_end":20,"byte_start":15,"column_end":6,"column_start":1,"expansion":null,"file_name":"src/lib.rs","is_primary":true,"label":null,"line_end":2,"line_start":2,"suggested_replacement":"B","suggestion_applicability":"MaybeIncorrect","text":[]}]}],"code":{"code":"test_lint","explanation":null},"level":"warning","message":"test warning","spans":[]}}"#;
        let suggestions = extract_suggestions(ndjson);
        assert_eq!(suggestions.len(), 2);
        assert_eq!(
            suggestions[0].applicability,
            Applicability::MachineApplicable
        );
        assert_eq!(suggestions[1].applicability, Applicability::MaybeIncorrect);
    }
}
