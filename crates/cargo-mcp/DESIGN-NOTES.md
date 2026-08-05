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

- **`Cargo ` prefix.** Lines now read `Cargo check: …` / `Cargo build [release]
  finished` rather than the bare `check:` / `cargo …`. The leading word is an
  unfortunate use of width but, without it, the collapsed history line loses
  too much context about which tool produced it.
- **Profile tag.** Every per-crate and `build-finished` line carries a
  bracketed marker naming the effective compilation profile verbatim:
  - `[dev]` — debug/dev (the default when neither `release` nor `profile` is set)
  - `[release]` — release (`release: true` or `profile: "release"`)
  - `[name]` — any other profile (e.g. `[test]`, `[bench]`, `[doc]`,
    `[my-profile]`), shown as-is rather than abbreviated
  An explicit `profile` argument wins over `release`, matching cargo's own
  precedence. Implemented in `profile_tag()` and threaded through
  `BuildTracker`. (Earlier revisions abbreviated the built-in profiles to
  single letters — `[D]`/`[R]`/`[T]`/`[B]` — with custom names in braces;
  that was dropped as needlessly cryptic in favour of the plain name.)
- **`Building` indicator and phase-aware `build-finished`.** Per-crate
  compile lines are prefixed with `Building` (e.g. `Building serde v1.0.228
  …`) so a progress line is never ambiguous about whether cargo is compiling
  or running something. `cargo_test` / `cargo_nextest_run` run in two
  `run_phase()` calls (build, then execution) that share one `BuildTracker`;
  an `x-cargo-mcp-phase` control record injected by `run_phase` before each
  phase lets the tracker label `build-finished` accordingly:
  - phase `build` — a real compile: `build phase [profile] finished/failed`
  - phase `test execution` — the near-instant cache-hit check cargo performs
    immediately before running tests (which would otherwise look like a
    second, confusing "finished"): `build cached [profile] — executing
    tests now`
  - phase `doc test` — the single-phase doctest path's `build-finished` is a
    *real* compile, not a pre-execution cache-hit check (doctests have no
    build/execute split to begin with), so it gets its own message rather
    than reusing the `test execution` cache-hit wording: `doc test [profile]
    finished/failed`

### Format

```
Cargo check: Building serde v1.0.228 (3/15) [dev] [crates.io]
Cargo build [release] (x86_64-pc-windows-msvc) finished
Cargo test: build phase [dev] finished
Cargo test: build cached [dev] — executing tests now
Cargo test: doc test [dev] finished
```

## Toolchain override (`+toolchain`)

### Why

A user shared an example where Copilot abandoned the cargo-mcp tools and ran
`cargo +ms-prod test -p firebird … | Select-String … | Select-Object -First 20`
in the terminal. Two gaps drove the fallback:

1. **Capability gap** — no tool parameter expressed `cargo +<toolchain> …`, so
   a custom toolchain (`ms-prod`) was simply not reachable through the tools.
2. **Habit gap** — the instructions never said that *filtering* output
   (`Select-String`/`grep`/`Select-Object`) is not a reason to shell out; the
   tools already return the full structured stream to filter in-agent.

This is the recurring "agent reached for the terminal because the tool surface
couldn't express the request" class: close it by making the capability
first-class *and* writing down the habit, not just fixing the one command.

### Decisions

- **Parameter, not signature thread.** A standalone `toolchain_arg()` helper in
  `tools.rs` normalises the `toolchain` string (trims, strips a redundant
  leading `+`, drops blanks) into `Some("+<name>")`. Each supported `call_*`
  reads it once and does `argv.insert(0, t)` so the token lands at index 0 —
  immediately after the binary name, where rustup expects a one-shot
  toolchain selection. No `invoke` signatures change.
- **Retry-safety skips the token.** `is_retry_safe` now judges the subcommand
  after an optional leading `+toolchain`, so a toolchain-pinned idempotent
  command (`+nightly test`) stays retry-eligible while `+nightly publish`
  does not.
- **Scope.** Wired into the eight build-relevant tools (`check`, `build`,
  `test`, `clippy`, `doc`, `tree`, `fmt`, `fmt_check`). Deliberately omitted
  from `metadata`/`clean`/`update`/`fix`/`add`/`remove`/`publish` as low-value;
  easy to extend later.
- **Consistency with RUSTC pinning.** `invoke` pins `RUSTC` to the resolved
  proxy path, which honours the `RUSTUP_TOOLCHAIN` that `+toolchain` sets, so
  the override stays consistent across cargo and rustc.

## cargo-nextest support

[`cargo-nextest`](https://nexte.st/) is exposed via two tools that sit
alongside `cargo_test` rather than replacing it: `cargo_nextest_run`
(wraps `cargo nextest run`) and `cargo_nextest_list` (wraps
`cargo nextest list`). Nextest cannot fully replace `cargo test`
because it does not support doctests
([nextest#16](https://github.com/nextest-rs/nextest/issues/16)), so
`cargo_test` remains the canonical tool and nextest is opt-in.

### Detection and install UX

Nextest ships as a separate `cargo-nextest` plugin binary; it is not
bundled with cargo or rustup. When either nextest tool is invoked and
the binary is not on PATH (probed via `cargo nextest --version`), the
tool returns `is_error: true` whose body is markdown containing the
install commands inside fenced shell code blocks:

```
cargo install cargo-nextest --locked
```

VS Code Copilot Chat renders fenced shell blocks with **Copy** and
**Run in Terminal** affordances automatically, so no additional MCP
machinery is needed to make the instructions actionable. The
non-existence of `cargo-nextest` is not cached across tool calls — a
user who installs mid-session should be able to retry immediately
without restarting the MCP server.

`cargo_setup` participates in the same UX. It writes a short "Optional:
cargo-nextest" subsection into the workspace's `copilot-instructions.md`
explaining when to prefer `cargo_nextest_run` over `cargo_test`. When
the binary is missing the setup tool also surfaces the same fenced
install commands in its result. If the workspace already contains a
`.config/nextest.toml`, the block is escalated from "optional" to
"recommended" because the workspace was authored expecting nextest.

### Output: wrap the human reporter, defer libtest-JSON

Nextest's `run` subcommand has two machine-readable output modes for
the test phase:

1. The **human reporter text** (default), which can be wrapped
   line-by-line as `x-cargo-mcp-nextest-output` NDJSON records — the
   exact pattern `cargo_test` already uses to wrap libtest harness
   text as `x-cargo-mcp-test-output`.
2. `--message-format libtest-json[-plus]`, which produces structured
   per-test events. This is gated behind
   `NEXTEST_EXPERIMENTAL_LIBTEST_JSON=1` and is **explicitly**
   subject to breaking changes (tracked by
   [nextest#1152](https://github.com/nextest-rs/nextest/issues/1152)).

We ship (1) only. Coupling our parser to an unstable upstream format
would impose a recurring maintenance tax for marginal benefit over
wrapping the text reporter, which already conveys per-test status with
ANSI stripped. Revisit when nextest stabilises the format. The build
phase of `cargo nextest run` is decoupled from this choice: nextest
forwards cargo's NDJSON when `--cargo-message-format=json` is set
(stable since [0.9.123](https://nexte.st/changelog/#0.9.123)), so
compiler diagnostics flow through the existing
`compiler-message` / `build-finished` pipeline unchanged.

`cargo_nextest_list` uses nextest's own `--message-format json`, which
**is** stable — the discovery result is returned as structured JSON
directly.

### Timeout model: overall cap, plus an execution-phase override

Nextest has its own per-test timeout machinery via profile config
(`slow-timeout`, `terminate-after`). To avoid two competing watchdogs
we expose only the overall `timeout_secs` wall-clock cap (deferred-arm
on `build-finished`, identical to `cargo_test`'s) and let nextest's
profile do the per-test work. The tool description says so explicitly
so callers do not expect a `per_test_timeout_secs` parameter.

`test_timeout_secs` (added alongside `cargo_test`'s equivalent — see
"`test_timeout_secs`: an execution-phase-only override" below) is the
one exception: it overrides the EXECUTION phase's budget specifically,
letting a caller leave the build phase unbounded while still capping
test execution, without introducing a second per-test watchdog. It is
resolved by the same `resolve_test_phase_timeouts` helper `cargo_test`
uses, so the two tools' semantics never drift apart. `bisect` rejects
`test_timeout_secs` for the same reason it isn't paired with
`per_test_timeout_secs` — bisection has its own
`group_timeout_secs`/`slow_threshold_secs` budget model.

### Flag remapping

A few nextest flags diverge from cargo test in ways that would
silently mis-route values if we reused `cargo_test`'s schema verbatim:

- `--profile <name>` selects the **nextest** profile in nextest, but
  the **cargo build** profile in cargo test. We split them into
  `nextest_profile` and `cargo_profile` on `cargo_nextest_run` so
  intent is unambiguous.
- `-j N` is build jobs in cargo test but test threads in nextest. We
  expose `build_jobs` (`--build-jobs`) and `test_threads`
  (`--test-threads`) as distinct parameters and never accept a bare
  `jobs`.
- `--doc` is not accepted (unsupported by nextest). Doctests stay on
  `cargo_test`.
- `test_filter` (our regex pipeline) IS accepted on
  `cargo_nextest_run`/`cargo_nextest_list` — see "`test_filter` translation
  on nextest" below — because nextest's filterset DSL supports regex
  matching natively (`test(/regex/)`), so the same `{ pattern,
  include_ignored }` shape can be translated into an equivalent
  `filter_expr` instead of forcing callers to learn a second syntax. We
  still also expose the native `filter_expr` (nextest's `-E` DSL, strictly
  more expressive) and a positional `filter` substring for parity with
  `cargo_test`'s `test_name`.
- `test_name`/`exact` (cargo_test's own substring/exact filter) are ALSO
  accepted on `cargo_nextest_run`/`cargo_nextest_list`, translated into
  `filter` (substring) or `test(=name)` (exact) — see "`test_name`/`exact`
  and `filter` translations" below. The reverse direction is handled too:
  `filter` is accepted on `cargo_test`, translated into `test_name`.

## Strict argument validation (unknown-key rejection)

### Context

A real session showed Copilot calling `cargo_nextest_run` with `test_filter`
and `per_test_timeout_secs` — parameters that belong to `cargo_test`, not the
nextest tool. Cargo's argument plumbing ignored the unknown keys, so each
"filtered" call silently ran the **entire** suite. On a ~10k-test workspace
that turned a spot-check into a multi-minute runaway that then tripped the
wall-clock cap with a confusing `TimeoutError`.

### Decision

`tools::call` validates arguments against the advertised schema **before**
dispatch, via `validate_known_args`:

- The allow-list is **derived from `list()`** (the same JSON the server
  advertises), cached in a `OnceLock<HashMap<tool, HashSet<key>>>`. Because the
  schemas are closed (no top-level `additionalProperties`), any key a
  conforming MCP client sends is already declared — only hallucinated /
  cross-tool keys are rejected, so strict validation is safe for real clients.
- An unknown key produces an actionable error listing the valid parameters,
  plus a curated **cross-tool hint** (`cross_tool_hint`) for the common
  confusions that have no safe automatic translation (`jobs`/`profile` are
  ambiguous which nextest knob they mean; `per_test_timeout_secs` and `doc`
  have no nextest equivalent at all), and a Levenshtein "did you mean
  `<closest>`?" suggestion for typos. (`test_filter` and `test_name`/`exact`
  used to be in this curated list too, but are now legitimate parameters on
  the nextest tools, translated automatically instead — see below. Likewise
  `filter` on `cargo_test` — see below.)
- Validation is centralised in the dispatcher rather than per-tool so it stays
  in lock-step with the schema automatically and cannot drift. Unit tests
  call the `call_*` functions directly, which is below the validation layer,
  so they are unaffected.

The failure mode this fixes is specifically "wrong knob silently runs
everything": a mismatched selection parameter now fails fast with a pointer to
the right one instead of quietly executing the full suite.

### `test_filter` translation on nextest

Even with the curated cross-tool hint in place, callers (LLMs in particular)
kept reaching for `test_filter` on `cargo_nextest_run` out of `cargo_test`
habit, hitting the same rejected-as-unknown error repeatedly across
sessions — a better error message did not stop the mistake from recurring.
Unlike `working_directory` (a pure alternate spelling — see below),
`test_filter` is a fundamentally different mechanism from `filter_expr`: on
`cargo_test` it drives a whole build/enumerate/`--exact` pipeline
(`test_filter.rs`), and on nextest that pipeline doesn't exist at all.

The fix is a genuine translation rather than a smarter rejection, made
possible by a fact this document previously used as the *reason not to*
support it: nextest's filterset DSL supports regex matching natively via
`test(/regex/)`. Both cargo-mcp's `test_filter.pattern` and nextest's
`test(/…/)` compile the pattern with the same underlying `regex` crate, so
the translation is a direct string substitution rather than a semantic
reinterpretation:

- `test_filter.pattern` → `filter_expr: "test(/{pattern}/)"`.
- `test_filter.include_ignored: true` → `run_ignored: "all"` (mirrors
  `cargo_test`'s `--include-ignored`: run both ignored and non-ignored
  tests, rather than nextest's `only`, which would run *just* ignored
  tests).
- A `pattern` containing a literal `/` is rejected up front rather than
  silently producing a broken filterset expression: nextest's `/…/`
  delimiter has no escape sequence for it. The error points the caller at
  `filter_expr` directly for that edge case.
- Supplying `test_filter` alongside `filter_expr`, `filter`, or
  `run_ignored` explicitly is rejected (ambiguous — which selection should
  win?) rather than silently overriding one with the other.

Implementation: `nextest::translate_test_filter` parses and validates the
`test_filter` object (reusing the same shape as `cargo_test`'s), and
`NextestOwnedOpts::apply_test_filter` folds the translated `filter_expr` /
`run_ignored` into the same struct fields the native parameters populate —
so the rest of `call_run` / `call_list`'s argv construction is unaware
`test_filter` was ever involved.

### `test_name`/`exact` and `filter` translations

The same recurring-mistake pattern showed up for `cargo_test`'s
`test_name`/`exact` pair (rejected as unknown on the nextest tools) and, in
the opposite direction, nextest's own `filter` (rejected as unknown on
`cargo_test`). Both are translated rather than just hinted at, for the same
reason as `test_filter`: the underlying semantics line up exactly.

On `cargo_nextest_run`/`cargo_nextest_list`:

- `test_name` alone (substring match) → nextest's own `filter` positional
  argument. No filterset expression is needed: nextest's plain `filter` is
  already a libtest-compatible substring filter, so this is a direct
  parameter rename, not a translation into a different mechanism.
- `test_name` + `exact: true` → `test(=name)`, the filterset DSL's equality
  matcher (`=string`), since a bare `filter` has no exact-match mode. The
  name is escaped per the filterset escape-sequence grammar (`\`, `/`, `)`,
  `,`) before being embedded — see `escape_filterset_matcher`.
- `exact: true` without `test_name` is silently ignored, mirroring
  `cargo_test`'s own behavior for the same case (see `build_doc_test_argv`):
  the flag is meaningless without a name to match.
- Supplying `test_name` alongside `filter`, `filter_expr`, or `test_filter`
  is rejected as ambiguous, the same guard pattern as `apply_test_filter`.

On `cargo_test`, the reverse: `filter` is accepted as an alias for
`test_name` (`resolve_test_name`), since both are the same plain substring
filter under different names. Supplying both `test_name` and `filter` is
rejected as ambiguous.

`filter_expr` is deliberately **not** given the same treatment in either
direction: nextest's filterset DSL (boolean combinators, `package()`/
`kind()`/`binary()` predicates, glob/regex matchers) cannot be mechanically
reduced to libtest's single substring/exact name filter, so it stays a
curated `cross_tool_hint` rejection on `cargo_test` rather than a
translation. Likewise, `jobs`, `profile`, `per_test_timeout_secs`, and `doc`
remain curated hints rather than translations — see "Flag remapping" above
for why each is either ambiguous or has no nextest equivalent at all.

Implementation: `nextest::translate_test_name` +
`NextestOwnedOpts::apply_test_name` mirror `translate_test_filter` /
`apply_test_filter`'s shape; `tools::resolve_test_name` handles the
`cargo_test`-side `filter` alias.

### `working_directory` alias

Strict validation had an unintended side effect: `working_directory` — the
name an LLM reaches for far more often than the actual `working_dir` key —
was rejected outright, and its Levenshtein distance from `working_dir` (6
edits) exceeds `closest_key`'s "did you mean" threshold, so callers got a
plain "unknown parameter" error with no useful correction. Unlike
`test_filter`/`per_test_timeout_secs`, there is no meaningfully different
parameter `working_directory` could be confused with — it is purely an
alternate spelling of the same concept for every tool that accepts it. Rather
than special-case a hint, `tools::call` renames the key to `working_dir` via
`normalize_working_directory_alias` before validation/dispatch ever see it
(stripping the alias either way, so it never leaks through as "unknown" even
when both keys are sent together — the existing `working_dir` value wins).

## `working_dir` manifest discoverability check

### Context

The cargo-mcp server's own process working directory is almost never the
user's workspace (on Windows it is typically the home folder or a system
directory). Tools default `working_dir` to that process cwd when the caller
omits it, so an omitted `working_dir` makes cargo run against the wrong — or
no — manifest. The same session above traced a class of failures to this.

### Decision

`ensure_manifest_discoverable` runs in `tools::call` right after argument
validation. For every manifest-requiring tool (all `cargo_*` except
`cargo_setup` and `cargo_diagnostic`, which intentionally tolerate a
manifest-less directory) it walks upward from the effective `working_dir`
(or the process cwd when omitted) looking for a `Cargo.toml`. If none is
found it returns an actionable error that names `working_dir` and explains
the default-to-server-cwd trap, instead of letting cargo fail opaquely or
operate on an unexpected manifest. When `manifest_path` is supplied the check
is skipped (the caller pinned the manifest explicitly).

## Metadata-derived build-progress denominator

### Context

The progress counter (`(3/15)`) previously used a **running lower bound** as
its denominator: `total_count` was the number of artifacts seen *so far*, so
early in a build it under-reports (`(1/1)`, `(2/5)`, …) and the denominator
visibly wobbles upward as more crates stream in. For a large dependency graph
this gives no sense of how far along the build actually is.

### Decision

Before each streamed build (`cargo_check`, `cargo_build`, `cargo_test`,
`cargo_clippy`, `cargo_doc`), `emit_progress_total` runs `cargo metadata
--format-version=1` and counts the resolved dependency-graph nodes
(`/resolve/nodes`, falling back to `packages`). The count is cached per
working directory (`OnceLock<Mutex<HashMap>>`) so repeated builds in a session
pay the metadata cost once. It is delivered to the streaming layer as an
in-band control record:

```
{"reason":"x-cargo-mcp-progress-total","total_units":120}
```

`BuildTracker::process_line` recognises that `reason` (a normal parsed-JSON
record, so it never collides with the `cargo-mcp:`-prefixed verbatim path),
stores it as `known_total`, and emits nothing visible for it. The per-crate
counter then renders `total_count / known_total` — real progress toward the
known graph size from the first crate. The denominator is clamped with
`known_total.max(total_count)` so it can never drop below what has already
streamed (defensive: the resolved graph should always be a superset of the
compiled units). When metadata is unavailable the tracker falls back to the
old running-lower-bound behaviour, so the single-artifact `(1/1)` case is
preserved.

## Per-crate `cargo fmt` fallback for long command lines

### Context

`cargo fmt` (the cargo-fmt wrapper) gathers every target across the whole
workspace and invokes rustfmt with all of their root paths on a **single**
command line. In a large workspace that command line can exceed the OS limit
before any file is formatted — Windows surfaces this as "The filename or
extension is too long. (os error 206)"; Unix as `E2BIG` "Argument list too
long". The single pass then fails outright.

### Decision

`call_fmt` and `call_fmt_check` share a `run_fmt(check)` implementation that
keeps the fast path and only splits when warranted:

1. Run the normal single invocation first (also the only path when the caller
   pinned a `package` — there is nothing to split).
2. On failure with no `package`, decide whether to fall back to a per-crate
   pass enumerated via `cargo metadata` (`workspace_member_names`):
   - **Apply mode** (`cargo fmt`): any non-zero exit is a real error, so fall
     back and let each `cargo fmt --package <member>` isolate the offending
     crate.
   - **Check mode** (`cargo fmt --check`): a non-zero exit is the *normal*
     "needs formatting" diff signal, so fall back **only** when the failure is
     definitively the command-line-length case, detected by
     `looks_like_command_too_long` (matching the os-error-206 / E2BIG
     markers). Otherwise the single pass's diff is returned verbatim.
3. The per-crate pass runs `cargo fmt [--check] --package <member>` for each
   workspace member and aggregates the output. When the original failure was
   the length-limit case its error text is suppressed (it was a spurious
   spawn failure, not a formatting problem); the aggregated per-crate result
   becomes authoritative.

This honours the guidance "suppress the error text only if you can
definitively detect the line-too-long case; otherwise fall back to per-crate
formatting anyway" while avoiding any extra process launches for the common
small-workspace case.

## Build/test phase split and phase-attributed timeouts

### Context

A single `cargo test` invocation interleaves compilation and test execution.
Arming the timeout watchdog on cargo's `build-finished` record already excludes
build time from the execution clock for the streaming path, but the resulting
`TimeoutError` could not say *which* activity overran — a slow compile and a
hung test produced the same opaque message. Callers also asked for build time
to be unambiguously excluded from the execution budget.

### Decision

`call_test_unfiltered` (cargo) and `call_run` (nextest) each run two cargo
invocations instead of one:

1. **Build phase** — `cargo test --no-run` / `cargo nextest run --no-run`,
   bounded by `timeout_secs` with the watchdog armed immediately (no deferred
   arm), so the clock covers the whole compile. A timeout here is labelled
   `build`. A non-zero build exit (or `no_run: true`) returns the build output
   directly and skips execution.
2. **Test execution phase** — the same command without `--no-run` (build now
   cached), bounded by `timeout_secs` with the watchdog armed on
   `build-finished` (which fires near-immediately because nothing recompiles).
   A timeout here is labelled `test execution`.

`timeout_secs` is therefore applied **independently** to each phase on its own
clock — build time never counts against the execution budget. The shared
`run_phase` helper threads `on_progress` through both calls (taking
`&mut Option<&mut dyn FnMut(&str)>` and reborrowing via `&mut **cb`, because
`&mut dyn FnMut` is invariant and cannot be moved into two sequential calls)
and maps any `TimeoutError` to `invoke::PhaseTimeoutError { elapsed, phase }`
via `invoke::label_timeout_phase`. `PhaseTimeoutError`'s `Display` names the
phase; `main.rs` already renders `error: {e}`, so the phase-attributed message
flows through with no special casing.

Splitting into two invocations means the cached execution phase emits no
`compiler-message` warnings (nothing recompiles). `combine_build_and_exec_output`
preserves them: it prepends the build phase's `compiler-message` lines to the
execution phase's stdout before the existing `format_test_output` /
`format_nextest_run_output` formats the combined stream. The execution phase's
exit code is authoritative. The pre-existing `test_filter` path already builds
(`--no-run` enumerate) before executing, so it was left unchanged.

Doctests are the one exception: cargo rejects `cargo test --doc --no-run`
(`can't skip running doc tests with --no-run`), so `doc: true` bypasses the
build/execute split entirely and runs `cargo test --doc ...` as a single
phase, still bounded by `timeout_secs` but on one clock. `no_run: true`
combined with `doc: true` is rejected up front, before either phase runs.
`test_filter` combined with `doc: true` is rejected the same way: doctests
have no `--list`/`--exact` support and are always excluded from filter
selection (see `test_filter.rs` module docs), so `call_test` errors instead
of silently running non-doctests. `bisect` combined with `doc: true` is
rejected for the same reason: the bisection engine builds with
`cargo test --no-run` and never adds `--doc`, so it would silently bisect
non-doctests instead of failing fast.

### `test_timeout_secs`: an execution-phase-only override

A user asked: given the build/execution phase split above, could the build
phase be left unbounded while only test execution is capped — and if both a
budget and this override are supplied, should the override be capped by the
overall budget? Both "yes"; `test_timeout_secs` implements exactly that.

`resolve_test_phase_timeouts(args)` is the single place that resolves the
`(build_timeout, test_timeout)` pair from `timeout_secs` and
`test_timeout_secs`, shared verbatim by `call_test_unfiltered` (cargo) and
`call_run` (nextest) so the two tools can never drift apart:

- Only `test_timeout_secs` set: the build phase is left **unbounded** (an
  explicit test-specific budget signals the caller only wants execution
  bounded), and execution gets `test_timeout_secs`.
- Both set: execution is **clamped** to never exceed `timeout_secs` — it can
  only tighten the budget, never loosen it beyond the overall cap.
- Only `timeout_secs` set, or neither: unchanged pre-existing behaviour —
  the same value (explicit or the server default) applies to both phases
  independently.
- Explicit `test_timeout_secs: 0` means "no override" (not "unbounded") —
  matching the `0` = disable convention `timeout_secs` and
  `per_test_timeout_secs` already use, so execution falls back to whatever
  the overall budget provides.

`test_timeout_secs` is rejected outright when combined with `doc: true`
(doctests have no separate build/execute split to override — there is only
one phase), `test_filter` (already has its own `timeout_secs` /
`per_test_timeout_secs` pair covering an analogous but distinct model), or
`bisect` (has its own `group_timeout_secs` / `slow_threshold_secs` model) —
each rejection fires before the incompatible pipeline runs, mirroring the
existing `doc`-combination rejections above rather than silently ignoring an
unusable parameter.

## Hang / slow-test bisection (`bisect`)

### Context

When a suite hangs or a single test runs pathologically long, the timeout
machinery kills the run but does not pinpoint the offender (in batched filter
mode the hung test is only inferable from the last output line). Locating it by
hand means repeatedly editing `--exact` lists. The user asked for an automated
bisection mode driven from the existing test tools.

### Decision

A new `bisect.rs` module implements a self-contained bisection engine, exposed
as an optional `bisect` object on **both** `cargo_test` and `cargo_nextest_run`
(a mode flag, not new tools). When `bisect::is_bisect_requested` is true the
dispatcher hands the whole call to `bisect::run` before the normal/`test_filter`
paths; both tools route to the same engine, which runs the **compiled libtest
binaries directly** (bypassing the cargo and nextest runtime), so the two tools
behave identically under bisection.

Pipeline:

1. **Build once** — `cargo test --no-run` (unbounded; build time is not the
   subject of bisection). A non-zero build exit returns the build output as an
   error.
2. **Enumerate** — reuse `test_filter`'s `parse_no_run_artifacts` (to find the
   test binaries) and `enumerate_tests` (libtest `--list`), filtered by the
   optional `pattern` regex and `include_ignored`.
3. **Bisect each binary** — form first-level groups (`initial_group_size` /
   `initial_groups`, default one group of all tests), then a depth-first stack
   of `(names, depth)`. Each group runs single-threaded under a
   `group_timeout_secs` kill-deadline (`invoke::run_subprocess_capture`; a
   `TimeoutError` marks the group hung). A group is **interesting** when it
   hangs or — if `slow_threshold_secs` is set — exceeds that threshold.
   Interesting groups are split into `split_factor` (or `ceil(100/split_percent)`)
   roughly-equal contiguous sub-groups and pushed back on the stack, until the
   group reaches `min_group_size` or `max_rounds` of depth, at which point its
   members are reported as culprits (`hung` if the leaf timed out, else `slow`).
   Group runs chunk their argv by `ARG_BYTE_BUDGET` so the `--exact <names…>`
   list never exceeds the OS limit, and short-circuit on the first hang.

Output is an NDJSON stream of `x-cargo-mcp-bisect-{config,group,culprit,summary}`
records, with progress lines streamed via `on_progress` during long runs. The
result is an error when any culprit (or enumeration error) is found.
`output_path` is honoured through the shared `tools::resolve_output_path` (made
`pub(crate)` for this): the full body is written to the file and a compact
summary (config + every culprit + summary + status + file pointer) is returned
inline.

The `bisect` schema is a single object property added to both tools' `list()`
schemas via the shared `bisect_schema()` helper; because the schemas are closed
and `validate_known_args` derives its allow-list from `list()`, adding the
property automatically makes `bisect` (and only `bisect`) a valid top-level key.
Nested `bisect.*` keys are validated inside `BisectOpts::from_args`, which also
enforces the mutual-exclusion and ordering constraints (`split_factor` vs
`split_percent`, `initial_group_size` vs `initial_groups`, `slow_threshold_secs`
< `group_timeout_secs`, `group_timeout_secs` required and positive).

## Suppressing harmless incremental-compilation-session stderr notes

### Context

On Windows, rustc can fail to finalize a `-working` incremental compilation
session directory when another process (antivirus, indexer, a lingering file
handle) briefly holds it open, and prints a plain-text note directly to
stderr:

```
note: error finalizing incremental compilation session directory `...-working`: Access is denied. (os error 5)
```

This is unrelated to the existing ReFS-specific advisory already covered
above (which arrives as a structured `compiler-message` diagnostic on
stdout and is handled by `--clear-incr-working` / diagnostic demotion). This
note instead lands verbatim on the child's **stderr** and is folded into the
`x-cargo-mcp-stderr` record by `format_json_output` / `format_test_output` /
`format_nextest_run_output`. It is pure noise: the compile still succeeds,
and the only effect is that rustc falls back to a full rebuild for that one
crate's incremental cache next time — an idempotent, self-correcting
condition. Left unfiltered, it clutters the stderr record (sometimes with
dozens of near-duplicate lines in a large workspace) and risks an agent
misreading it as a real problem.

### Decision

`tools::strip_incremental_notes` removes any stderr line containing the
substring `error finalizing incremental compilation session directory`
(matched as a substring, not anchored, since rustc prefixes it with `note: `
and appends the path and OS error text) along with the single blank
separator line rustc emits after each note. All other stderr content —
genuine errors, the Restart Manager holder report, `eprintln!` output — is
left untouched.

This is applied via `tools::stderr_for_display`, a single choke point used
by all three stderr-emitting formatters (`format_json_output`,
`format_test_output` in `tools.rs`, and `format_nextest_run_output` in
`nextest.rs`) in place of the previous bare `out.stderr.trim()`. Suppression
is **on by default** (matching the existing `--clear-incr-working` and
`--unsafe-windows-rm` opt-in pattern, but inverted: here the *quiet* behavior
is the default and showing the notes is the opt-in) and controlled by a
process-global `AtomicBool` set once at startup from the new
`--show-incremental-notes=<bool>` CLI flag, mirrored as the
`cargo-mcp.showIncrementalCompilationNotes` VS Code setting (default
`false`), whose description explains that the notes are harmless and the
build is idempotent. Passing the flag as `true` restores the raw stderr
text for diagnostic purposes.

### Addendum: the same advisory also arrives as a `compiler-message` JSON record

The suppression above only ever looked at plain-text **stderr**. But every
`--message-format=json` tool (`cargo_build`, `cargo_check`, `cargo_test`,
`cargo_nextest_run`) can *also* receive this exact advisory as a structured
`compiler-message` record on **stdout** — and until this addendum, that form
was forwarded to the caller completely unfiltered, regardless of the
`show_incremental_notes` setting. This was most visible in
`cargo_nextest_run`, whose `filter_nextest_run_ndjson` unconditionally kept
every `compiler-message` line. It also missed the newer wording rustc
switched to per rust-lang/rust#154110 (`did not finalize incremental
compilation session directory ...`), which ships already at `level: "note"`
from the compiler itself rather than `warning`/`error`.

`INCREMENTAL_NOTE_MARKERS` (a `[&str; 2]`, replacing the earlier single
`INCREMENTAL_NOTE_MARKER` constant) now holds both wordings, and
`tools::compiler_message_is_incremental_note` checks a parsed
`compiler-message` value's `message.rendered` (falling back to
`message.message`) against either marker — independent of `level`, since the
same advisory can appear at `warning`, `error` (via `-D warnings`), or `note`
depending on rustc version. `filter_build_ndjson` / `filter_test_ndjson`
(`tools.rs`) and `filter_nextest_run_ndjson` (`nextest.rs`) all drop a
matching `compiler-message` record when `show_incremental_notes_enabled()`
is `false` (the default), so the same CLI flag / VS Code setting now gates
both the stderr text and the JSON record forms consistently.


