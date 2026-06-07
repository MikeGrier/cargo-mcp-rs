# cargo-mcp Design Notes

## Architecture

`cargo-mcp` is an MCP (Model Context Protocol) server that exposes Cargo's build system
functionality as tools callable by AI agents such as GitHub Copilot. It follows the same
architecture as `tpu-mcp`:

- Speaks JSON-RPC 2.0 over stdio using newline-delimited messages
- Each tool invocation spawns `cargo` as a subprocess
- All build logic lives in Cargo — the MCP server is a thin dispatch layer

## Key differences from tpu-mcp

- **No response files**: Cargo's CLI arguments are short command-line flags and paths,
  not multi-kilobyte file content. Standard argv passing is appropriate.
- **No sibling binary**: Unlike tpu-mcp which locates a co-built `tpu` binary, cargo-mcp
  invokes `cargo` from the PATH (it's a system tool, not a workspace-built binary).
- **Working directory**: Most tools accept an optional `manifest_path` or `working_dir`
  parameter so the caller can target a specific crate within a workspace.

## Design decisions

### Subprocess, not library

Cargo's public API is unstable and not intended for library use. The recommended integration
method is subprocess invocation, which is what we do.

### Hang prevention

All subprocess invocations:
- Close stdin (Stdio::null)
- Capture both stdout and stderr
- Never invoke interactive modes

Cargo does not use a pager by default, but we set `CARGO_TERM_COLOR=never` to avoid
ANSI escape sequences that would be noise in MCP responses.

### Structured output

All tools that support it use `--message-format=json`, producing NDJSON (one JSON
object per line) on stdout. This includes `check`, `build`, `test`, `clippy`, and `doc`.
`cargo metadata` natively produces JSON via `--format-version=1`.

Tools without a stable JSON mode (`fmt`, `tree`, `clean`) return plain text — the
server does not attempt to parse this text; it is forwarded as-is. The tool descriptions
explicitly note which output format each tool uses so the consumer knows what to expect.

For JSON-mode tools, stderr (which contains human-readable progress messages like
"Compiling foo...") is discarded in favour of the structured stdout. On failure, the
exit code is included in the response so the consumer can distinguish success from
error without parsing text.

### Tool naming

All tools are prefixed with `cargo_` to namespace them within the MCP tool registry,
consistent with `tpu_` prefix used by tpu-mcp.

## Changing any tool name, parameter name, or schema is a breaking change

MCP tool names and schemas form a contract with the AI agent. Changing them requires
updating the agent's configuration (e.g., copilot-instructions.md) and any prompts
that reference specific tools.

## Elicitation-based suggestion selection

Tools that produce compiler/clippy diagnostics (`cargo_check`, `cargo_clippy`) parse the
NDJSON output to extract actionable suggestions — those with `MachineApplicable`,
`MaybeIncorrect`, or `HasPlaceholders` applicability.

### Architecture

```
tools/call ──► tools::call() ──► ToolResult::WithSuggestions
                                      │
                                      ▼
                              main.rs handle_tool_call
                                      │
                          ┌───────────┼──────────┐
                     can_elicit   can_elicit   no suggestions
                     + suggestions  but none     │
                          │           │          ▼
                          ▼           ▼       return output
               elicitation/create   return
               multi-select form    output
                          │
                     user selects
                          │
                          ▼
               return selected summary + full output
```

### Modules

- **`suggest.rs`** — Parses NDJSON output and extracts `Suggestion` structs with file location,
  message, code, and machine-applicable replacement text. Pure data transformation, no I/O.
- **`elicit.rs`** — Builds the `TitledMultiSelectEnumSchema` from suggestions, sends the
  `elicitation/create` JSON-RPC request to the client, and reads the response. Handles
  accept/decline/cancel actions.
- **`tools.rs`** — `ToolResult` enum (`Text` or `WithSuggestions`) allows the dispatch layer
  to handle suggestions without the tool implementations knowing about elicitation.
- **`main.rs`** — `handle_tool_call` orchestrates the flow: call the tool, check for
  suggestions, optionally elicit, and build the response.

### Capability negotiation

During `initialize`, the server checks the client's `capabilities.elicitation.form` field.
If present, the server will send `elicitation/create` requests for tools with suggestions.
If absent, suggestions are appended as a numbered text list in the tool output, allowing
the LLM to present them conversationally.

### Elicitation mode (`--elicitation-mode`)

The `--elicitation-mode=<mode>` CLI argument controls how the server handles suggestions
that need human approval. Three modes are supported:

| Mode | Behaviour |
|---|---|
| `always-skip` (default) | Automatically skip all suggestions without prompting |
| `prompt` | Present a multi-select form to the user |
| `always-accept` | Automatically accept all suggestions without prompting |

The mode is parsed at startup and applies to all `tools/call` invocations for the
lifetime of the server. It requires the client to support elicitation (`prompt` and
`always-accept` modes use the elicitation capability to structure results); when the
client lacks elicitation support, suggestions fall back to the numbered text list
regardless of mode.

#### VS Code configuration

The mode is configured via the `args` array in `.vscode/mcp.json`:

```json
{
    "servers": {
        "cargo-mcp": {
            "type": "stdio",
            "command": "cargo-mcp.exe",
            "args": ["--elicitation-mode=always-accept"]
        }
    }
}
```

To make the value settable per-user via VS Code settings UI, define a setting in
`settings.json` and reference it via variable substitution:

```jsonc
// settings.json (user or workspace)
{ "cargo-mcp.elicitationMode": "prompt" }

// mcp.json
{ "args": ["--elicitation-mode=${config:cargo-mcp.elicitationMode}"] }
```

### Graceful degradation

The elicitation feature is strictly additive:
- Clients without elicitation support see the same NDJSON output as before, plus a numbered
  summary of actionable suggestions split by trust level.
- If the user declines or cancels the elicitation form, the full unfiltered output is returned
  (auto-applicable fixes are still reported).
- If no actionable suggestions are found, the tool output is returned unchanged.

### Tiered applicability

Suggestions are partitioned by their `suggestion_applicability` trust level:

| Level | Behaviour | Rationale |
|---|---|---|
| `MachineApplicable` | Auto-reported (no human approval needed) | Compiler-verified, safe to apply |
| `MaybeIncorrect` | Presented via elicitation for human approval | May not be correct |
| `HasPlaceholders` | Presented via elicitation for human approval | Contains placeholders user must fill in |
| `Unspecified` | Skipped entirely | Not reliably auto-applicable |

```
tools/call ──► ToolResult::WithSuggestions
                    │
                    ▼
            partition by applicability
            ┌───────┴───────┐
     MachineApplicable   MaybeIncorrect
     (auto-report)       HasPlaceholders
            │            (elicitation)
            ▼                │
  "Auto-applicable:       checkbox form
   N fixes, safe          │
   to apply"              user selects
            │                │
            └───────┬────────┘
                    ▼
              combined response
```

The `Applicability` enum in `suggest.rs` mirrors rustc's values. The `trust_tag()` method
provides a short UI label (empty for `MachineApplicable`, `[maybe-incorrect]` or
`[has-placeholders]` for the others) so the agent and user can see the trust level at a glance.

### Grouped elicitation

When the number of suggestions reaches or exceeds GROUPING_THRESHOLD (5), the flat
multi-select list is replaced by a grouped presentation that organises suggestions and
adds per-group "select all" headers.

#### Grouping modes

- **By lint code** (default) — suggestions sharing the same `code` field form a group.
  Suggestions with `code: None` each become singleton groups keyed by `_id:<id>`.
- **By file path** — suggestions sharing the same `file` field form a group.

Groups are sorted largest-first (stable sort preserves insertion order for ties).
Only groups with ≥ MIN_GROUP_FOR_HEADER (2) members get a "select all" header.

#### Const naming scheme

The schema `anyOf` entries use a structured naming convention chosen to be easily
parseable in `parse_grouped_response`:

| Entry type | `const` value | Example |
|---|---|---|
| Individual item | `"<id>"` | `"3"` |
| Group select-all | `"all:<group-key>"` | `"all:clippy::needless_return"` |
| Mode switch | `"view:by-lint"` or `"view:by-path"` | `"view:by-path"` |
| Skip all | `"skip:all"` | `"skip:all"` |

#### Mode switching

A single synthetic entry at the end of the option list (prefixed with ↻) lets the user
toggle between by-lint and by-path views. When selected, the server sends a second
`elicitation/create` request with the alternate grouping. A maximum of one mode switch
is allowed to prevent infinite loops.

#### Visual prefixes

Schema option titles use Unicode prefixes for quick scanning:
- `▶` — group-all header ("Select all 5 instances · clippy::needless_return")
- `–` (en-dash) — individual items nested under a group
- `↻` — mode-switch entry
- `∅` — skip-all entry ("Skip all — apply none of these suggestions")

#### Skip-all option

Both flat and grouped schemas include a "Skip all" entry (`const: "skip:all"`) that
lets the user decline all suggestions without closing the dialog via "x". Closing with
"x" causes VS Code/Copilot to interpret the MCP server as non-functional and fall back
to running cargo directly for the rest of the session. The skip-all entry avoids this
by producing a normal `accept` action with an empty selection set.

#### Timeout and cancellation

The server waits up to `ELICITATION_TIMEOUT` (30 s) for the user to respond. On timeout,
the server sends a `notifications/cancelled` notification referencing the outstanding
`elicitation/create` request ID. This tells the client to dismiss the dialog rather than
leaving it on screen indefinitely.

#### Flow

```
elicit_selection(suggestions)
    |
    ├── count < GROUPING_THRESHOLD → elicit_flat() (unchanged flat list)
    └── count >= GROUPING_THRESHOLD → elicit_grouped()
                |
                ├── build_grouped_schema(mode=ByLint)
                ├── send elicitation/create
                ├── parse_grouped_response
                │       ├── ModeSwitch(ByPath) → loop once more with ByPath
                │       └── Selected(ids) → return expanded IDs
                └── on second pass: same steps with mode=ByPath, no further switch
```

## Progress notification label for registry crates

### Context

`notifications/progress` messages are sent for each non-fresh `compiler-artifact` line
in the `--message-format=json` output. Each artifact includes a `package_id` field
identifying its source.

### Why not use the registry alias name

Cargo's `package_id` format encodes the index URL, not the alias:

```
registry+https://github.com/rust-lang/crates.io-index#serde@1.0.228
```

The alias (`my-registry`) lives in `.cargo/config.toml` or `Cargo.toml`'s `[registries]`
table and is **not** written into the artifact metadata emitted by `--message-format=json`.
`cargo metadata` has the same limitation — `packages[].source` is also the raw URL.

To resolve alias → URL you would need to parse `.cargo/config.toml` and all workspace
`Cargo.toml` files, then do a reverse lookup. That is significantly more complexity and
fragility for marginal benefit.

### Chosen approach

Derive a short label from the URL's last path segment:

| URL | Label |
|---|---|
| `https://github.com/rust-lang/crates.io-index` | `crates.io` |
| `https://dl.cloudsmith.io/my-org/cargo/index.git` | `index.git` |
| `path+file:///...` | *(no label)* |

`crates.io-index` is special-cased to the friendlier `crates.io`. For private registries
the last segment of the index URL is at least meaningful and matches what users see in
their registry configuration. This is the same heuristic used by `cargo tree`.

### Format

```
serde v1.0.228 (3/15) [crates.io]
my-crate v0.1.0 (4/15)
```

## Progress-line prefix and profile tag

### Context

The progress text shown by VS Code is the `message` field of an MCP
`notifications/progress` message — a **plain string**. VS Code renders it as
status text and does *not* interpret markdown, so bold/code/links/colour are
unavailable; the only levers are the literal text and the numeric counter.

### Decisions

- **`Cargo ` prefix.** Lines now read `Cargo check: …` / `Cargo build [R]
  finished` rather than the bare `check:` / `cargo …`. The leading word is an
  unfortunate use of width but, without it, the collapsed history line loses
  too much context about which tool produced it.
- **Profile tag.** Every per-crate and `build-finished` line carries a short
  bracketed marker for the effective compilation profile:
  - `[D]` — debug/dev (the default when neither `release` nor `profile` is set)
  - `[R]` — release (`release: true` or `profile: "release"`)
  - `[T]` — test (`profile: "test"`)
  - `[B]` — bench (`profile: "bench"`)
  - `[doc]` — doc (`profile: "doc"`)
  - `{name}` — any other custom profile, shown verbatim in braces to set it
    apart from the abbreviated built-in markers
  An explicit `profile` argument wins over `release`, matching cargo's own
  precedence. Implemented in `profile_tag()` and threaded through
  `BuildTracker`.

### Format

```
Cargo check: serde v1.0.228 (3/15) [D] [crates.io]
Cargo build [R] (x86_64-pc-windows-msvc) finished
```

