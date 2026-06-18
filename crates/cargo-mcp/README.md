# cargo-mcp — Cargo tools for GitHub Copilot

When GitHub Copilot needs to compile, test, lint, format, or inspect a Rust
project, it normally types `cargo` commands into a terminal and reads the
output as plain text. **cargo-mcp** replaces that with a structured channel:
Copilot calls `cargo_check`, `cargo_build`, `cargo_test`, `cargo_clippy`,
`cargo_fmt`, `cargo_doc`, `cargo_tree`, and friends as first-class tools and
gets back machine-readable diagnostics with exact file paths and line numbers
it can act on immediately. Builds stream live progress, suggested fixes can be
reviewed and applied with one click, and transient Windows file-in-use errors
are retried automatically so they don't derail a multi-step task.

It's a [Model Context Protocol (MCP)](https://modelcontextprotocol.io/) server
under the hood — the protocol that lets editors and agents share tools — but
once installed it's invisible: Copilot just gets noticeably better at Rust.

---

## Quick start

### 1. Install the VS Code extension

> **Windows only:** Pre-built VSIXes are provided for `win32-x64` and
> `win32-arm64`. Users on other platforms should build from source (see
> [Building from source](#building-from-source)) and configure the server
> manually.

Download the `.vsix` for your platform from the
[latest GitHub Release](https://github.com/MikeGrier/cargo-mcp-rs/releases/latest)
and install it:

```sh
code --install-extension cargo-mcp-win32-x64-<version>.vsix
```

Or install directly from the VS Code Extensions Marketplace (search **cargo-mcp**) — Windows only; no Linux/macOS builds are published yet.

Reload VS Code. The extension registers the MCP server automatically — no
additional configuration required.

### 2. Tell Copilot to use the tools

The extension makes the tools available, but Copilot still needs to know it should
prefer them over running `cargo` in a terminal. The most reliable way to do this is
a short section in your repository's Copilot instruction file.

**Open Copilot Agent mode and type:**

```
/cargo-mcp:setup
```

This invokes the `setup` prompt, which tells Copilot to call `cargo_setup` and then
find the right place in your repository's instruction files —
`.github/copilot-instructions.md`, a sub-instructions file, or wherever your
project keeps them — adapt the wording to match your existing style, add the section,
and commit the change.

Once that commit is in place, Copilot will use the MCP tools for all Cargo
operations in that repository, in every future session.

> **Note:** After installing or updating the extension, reload VS Code
> (`Ctrl+Shift+P` → **Developer: Reload Window**) so the slash command appears
> in autocomplete.

> **Why is this step necessary?**
> Tool descriptions are only visible to Copilot *after* it has already decided how
> to carry out a task. A repository instruction file is loaded *before* that
> decision, so it reliably intercepts the choice before Copilot reaches for a
> terminal. Without it, Copilot may work correctly in isolation but fall back to
> the terminal once it has started a multi-step workflow there for unrelated reasons.

---

## Prerequisites

| Requirement | Notes |
|---|---|
| VS Code | 1.101 or later |
| GitHub Copilot | Chat extension with Agent mode enabled |
| Rust toolchain | stable — `cargo` must be on `PATH` (see [Toolchain resolution](#toolchain-resolution)) |
| `rustup` | not required, but **strongly recommended** if your repository uses a `rust-toolchain.toml`. Without rustup, the toolchain file has no effect on *any* cargo invocation — this is a property of cargo itself, not specific to cargo-mcp. |
| `cargo clippy` | `rustup component add clippy` (for `cargo_clippy`) |
| `cargo fmt` | `rustup component add rustfmt` (for `cargo_fmt` / `cargo_fmt_check`) |

---

## Manual server configuration

If you install the binary without the VS Code extension (or want to use
`cargo-mcp` in a different MCP client), configure it via `.vscode/mcp.json`
in your project root:

```jsonc
{
  "servers": {
    "cargo": {
      "type": "stdio",
      "command": "${userHome}/bin/cargo-mcp"
    }
  }
}
```

Or globally in VS Code user `settings.json`:

```jsonc
{
  "mcp": {
    "servers": {
      "cargo": {
        "type": "stdio",
        "command": "${userHome}/bin/cargo-mcp"
      }
    }
  }
}
```

`${userHome}` is resolved by VS Code. On Windows the binary is `cargo-mcp.exe`
on disk but VS Code appends `.exe` automatically, so the same config works
cross-platform.

---

## Output format

Every tool result begins with a one-line **JSON invocation header** that
records the *effective* command cargo-mcp ran, including any flags the
dispatch layer added implicitly (e.g. `--message-format=json`). The header
is shaped as a cargo-style NDJSON record:

```json
{"reason":"x-cargo-mcp-invocation","argv":["check","--message-format=json","--all-targets"],"cwd":"/path/to/project"}
```

The `reason` value uses an `x-` prefix so it can never collide with
cargo's own record types (`compiler-message`, `build-finished`,
`compiler-artifact`, etc.). This lets you reconstruct exactly what was
invoked from the tool-result panel even when the JSON `arguments` shown
by the MCP client are sparse (for example, only `working_dir` and a
single boolean flag).

For **JSON-mode tools** (`check`, `build`, `clippy`, `doc`,
`metadata`) the *entire* response is a strict NDJSON stream — the
invocation header followed by one JSON object per line — filtered to
keep only `compiler-message` and `build-finished` records. On failure
cargo-mcp appends a `{"status":"error","exit_code":N}` trailer record,
and when the cargo child wrote anything to stderr (where the Restart
Manager "who holds this file" report and other side-channel diagnostics
land) a separate `{"reason":"x-cargo-mcp-stderr","text":...}` record is
appended after the trailer, so the whole response stays parseable
end-to-end with a single line-by-line JSON parser.

**`cargo_test`** is a special case. The test execution phase produces
plain-text libtest output (harness lines like `test foo ... ok` and
pass/fail summaries, plus captured `println!` replays on failure) that
is not valid JSON. Each such line is wrapped in a custom NDJSON record:

```json
{"reason":"x-cargo-mcp-test-output","text":"test foo::bar ... ok"}
```

`eprintln!` from test code bypasses libtest capture and goes directly to
stderr; it is always included — even on success — as an
`{"reason":"x-cargo-mcp-stderr","text":"..."}` record. The complete
output shape for `cargo_test` is therefore:

```
{x-cargo-mcp-invocation}          ← effective command + cwd
{compiler-message} ...             ← zero or more compile errors/warnings
{build-finished}                   ← build phase outcome
{x-cargo-mcp-test-output} ...      ← zero or more test harness lines
{"status":"success"|"error",...}   ← always present
{x-cargo-mcp-stderr}               ← optional, when stderr non-empty
```

While the build runs, streaming progress notifications are also emitted;
the final notification reads `Cargo <verb> [D] finished` (or `failed`),
where the profile tag marks the effective profile — `[D]` dev/debug, `[R]`
release, `[T]` test, `[B]` bench, `[doc]` doc, or `{name}` (in braces) for
any other custom profile — and the optional target triplet is appended when
one is supplied. This is what appears as the collapsed summary line in the
VS Code chat history.

For **text-mode tools** (`fmt`, `tree`, `clean`, `update`, `fix`, `add`,
`remove`, `publish`) only the first line (the invocation header) is
JSON; the body that follows is the cargo child's combined stdout/stderr
and is not guaranteed to be JSON.

When `cargo_check` or `cargo_clippy` produce machine-applicable suggestions
and the MCP client supports elicitation, a multi-select form is presented
to the user; otherwise suggestions are appended to the result as a numbered
list.

---

## Tool reference

| Tool | Description |
|---|---|
| `cargo_metadata` | Workspace/package structure, dependencies, features, resolved graph (JSON) |
| `cargo_check` | Fast compile-error checking without producing binaries (NDJSON) |
| `cargo_build` | Full compilation; diagnostics + build status (NDJSON) |
| `cargo_test` | Run tests with optional filters; compilation JSON + harness output |
| `cargo_clippy` | Lint analysis with suggested fixes (NDJSON) |
| `cargo_fmt_check` | Verify formatting without modifying files; returns diff |
| `cargo_fmt` | Auto-format source code in place |
| `cargo_tree` | Dependency tree as text |
| `cargo_doc` | Build HTML documentation; returns warnings (NDJSON) |
| `cargo_clean` | Remove build artefacts |
| `cargo_update` | Update Cargo.lock to latest compatible versions |
| `cargo_fix` | Auto-apply compiler and Clippy machine-applicable fixes |
| `cargo_add` | Add a dependency to Cargo.toml |
| `cargo_remove` | Remove a dependency from Cargo.toml |
| `cargo_publish` | Package and upload to crates.io (irreversible — use `dry_run: true` first) |
| `cargo_setup` | Return the canonical cargo-mcp instruction text for Copilot setup |
| `cargo_diagnostic` | Report which cargo/rustc binary will be invoked, plus toolchain-file and env state |

### Common parameters

Most tools accept these optional parameters. Not every option applies to
every tool — `cargo doc` has no `--tests`/`--benches`/`--all-targets`,
`cargo test` has no `--keep-going` (but adds `doc` and `no_run`), and
`cargo tree` takes only `target` from the compilation group. Each tool's
schema advertises exactly the options it accepts.

**Toolchain override**

| Parameter | Type | Description |
|---|---|---|
| `toolchain` | string | Rustup toolchain to run the command with, passed as a leading `+<toolchain>` token (e.g. `cargo +nightly ...`). Accepts any rustup toolchain name — `nightly`, `stable`, `1.78`, or a custom toolchain such as `ms-prod`. Requires rustup. Supported on `cargo_check`, `cargo_build`, `cargo_test`, `cargo_clippy`, `cargo_doc`, `cargo_tree`, `cargo_fmt`, and `cargo_fmt_check`. Omit to use the toolchain selected by `rust-toolchain.toml` or the environment. |

**Package selection**

| Parameter | Type | Description |
|---|---|---|
| `working_dir` | string | Absolute path to the directory containing the workspace `Cargo.toml`. **Strongly recommended to pass explicitly.** If omitted, defaults to the cargo-mcp server process's working directory — typically not your workspace, and usually fatal to manifest or `rust-toolchain.toml` resolution. See [Toolchain resolution](#toolchain-resolution) and `cargo_diagnostic`. |
| `package` | string | Operate on a specific package (`-p`/`--package`) |
| `workspace` | boolean | Operate on all packages in the workspace (`--workspace`) |
| `exclude` | string | Exclude a package from a `workspace` operation (`--exclude`) |

**Target selection** (check, build, test, clippy; reduced set for doc)

| Parameter | Type | Description |
|---|---|---|
| `lib` | boolean | Only the library target (`--lib`) |
| `bins` / `bin` | boolean / string | All binaries (`--bins`) or one named binary (`--bin`) |
| `examples` / `example` | boolean / string | All examples (`--examples`) or one named example (`--example`) |
| `tests` / `test` | boolean / string | All test targets (`--tests`) or one named test (`--test`) |
| `benches` / `bench` | boolean / string | All benches (`--benches`) or one named bench (`--bench`) |
| `all_targets` | boolean | All targets (`--all-targets`) |

**Feature selection**

| Parameter | Type | Description |
|---|---|---|
| `features` | string | Comma-separated list of features to activate (`--features`) |
| `all_features` | boolean | Activate all features (`--all-features`) |
| `no_default_features` | boolean | Do not activate the `default` feature (`--no-default-features`) |

**Compilation options**

| Parameter | Type | Description |
|---|---|---|
| `release` | boolean | Use the release profile (`--release`) |
| `profile` | string | Build with a named profile (`--profile`) |
| `jobs` | integer | Number of parallel jobs (`--jobs`) |
| `keep_going` | boolean | Build as many targets as possible on error (`--keep-going`; not on test) |
| `target` | string | Build for a target triple (`--target`) |
| `target_dir` | string | Directory for generated artifacts (`--target-dir`) |
| `timings` | boolean | Emit an HTML build-timing report (`--timings`) |

**Manifest options**

| Parameter | Type | Description |
|---|---|---|
| `manifest_path` | string | Path to `Cargo.toml` (`--manifest-path`) |
| `ignore_rust_version` | boolean | Ignore the `rust-version` field (`--ignore-rust-version`; not on tree) |
| `locked` | boolean | Require `Cargo.lock` to remain unchanged (`--locked`) |
| `offline` | boolean | Run without accessing the network (`--offline`) |
| `frozen` | boolean | Equivalent to `--locked` plus `--offline` (`--frozen`) |

**Environment variables (`env`)**

Every tool that spawns cargo accepts an optional `env` object that sets or
unsets environment variables on the cargo subprocess for that one call.
Keys are env var names; values are either a string (set the variable) or
`null` (remove it from the child's environment). The map is layered on top
of cargo-mcp's built-in defaults (`CARGO_TERM_COLOR`, `NO_COLOR`, `RUSTC`),
so a caller-supplied value wins, and the resulting block is what cargo-mcp
hands to the OS as the child's environment (`env_clear()` + `envs(...)`).

```jsonc
{ "env": { "RUSTFLAGS": "-C debuginfo=2", "FIREBIRD_DUMP_MIR": "1" } }
```

Use it for one-shot debug knobs (`RUSTFLAGS`, `RUST_LOG`, `RUST_BACKTRACE`,
`RUSTC_BOOTSTRAP`, compiler-internal dumps) that only this single call
needs. Do **not** use it for permanent / project-wide configuration (put
that in `Cargo.toml`, `.cargo/config.toml`, or `rust-toolchain.toml`) or
for secrets — the env block is passed verbatim to the cargo child process
(visible via OS-level process inspection) and may be captured by future
logging additions, so treat it as not confidential.

**Timeouts (`timeout_secs` and `per_test_timeout_secs`)**

`cargo_test` has two independent timeout knobs that can be combined
freely. Both apply only to the test **execution** phase: each clock
arms when compilation and linking finish (cargo's `build-finished`
record), so a slow build never trips either of them.

- **`timeout_secs`** is a hard OVERALL wall-clock cap on the whole
  execution phase. Same meaning whether or not `test_filter` is set;
  in filter mode it bounds all per-binary launches together. Use it to
  keep throughput going on a slow system. Defaults:
    - Unfiltered: server default applies
      (`cargo-mcp.test.timeoutSecs`, 30s via the VS Code extension;
      none otherwise).
    - Filter mode: **no default** — omit to let a long matched run
      complete unbounded, pass an explicit positive value to cap it.
    - Pass `0` to disable for this call regardless of the server
      default.
- **`per_test_timeout_secs`** is a per-test idle watchdog — ONLY
  meaningful when `test_filter` is set; ignored otherwise. The clock
  arms on each per-binary `build-finished` record and resets on every
  libtest boundary line (`running N tests` or
  `test <name> ... ok|FAILED|ignored`). A long suite of fast tests
  never trips it; a single hung test does. If the watchdog fires for
  one binary, that binary's cargo process tree is killed and the
  orchestrator records `exit_code: -1` plus an inline
  `cargo-mcp test_filter: per-binary run failed: TimeoutError` body,
  then moves on to the next matched binary. Defaults to the server
  setting (30s via the VS Code extension) so hung-test protection is
  on by default in filter mode; when the server default is also
  absent or set to `0`, filter mode still applies a hard-coded
  **30-second** fallback. The only way to fully disable per-test
  protection for a call is to pass `per_test_timeout_secs: 0`
  explicitly.

Raise either (or pass `0` to disable) for runs you know are slow —
long integration suites, benchmark-style tests, tests that internally
poll. Lower either when sanity-checking a fix to fail fast on an
infinite loop. When both are set in filter mode, whichever fires
first kills the launch.

**Regex-based test selection (`test_filter`)**

`cargo_test` accepts an optional `test_filter` object that runs only the
tests whose names match a regular expression — the equivalent of a
hand-curated `cargo test --exact <name1> <name2> …` invocation, generated
from the regex. The orchestrator does a single `--no-run` build, lists
tests with libtest's `--list`, matches names against the regex, then
launches one cargo process per test binary that has matches (additional
launches only when the OS argv length limit forces chunking the name list).

```jsonc
{
  "test_filter": {
    "pattern": "tests::parser::(commas|braces)$", // required: RE2-style regex
    "include_ignored": false                        // optional, default false
  }
}
```

- `pattern` uses the [`regex`](https://docs.rs/regex) crate (RE2-style:
  linear-time, no backreferences, no lookaround). It is matched against
  the libtest test name — typically `module::path::test_name`. Use `^` /
  `$` if you want a full-name match; otherwise it matches as a substring.
  IMPORTANT: integration tests (under `tests/`) enumerate **without** a
  `module::` prefix, while unit tests inside the crate enumerate as
  `mod::sub::test_name`. A leading `^` anchor binds to that prefix —
  `^foo` matches integration test `foo` but NOT unit test
  `tools::tests::foo`. To span both, either drop the anchor (substring
  match) or include both forms in the alternation
  (e.g. `^(foo|tools::tests::foo)$`).
- When `include_ignored: true`, ignored tests are enumerated via
  `--list --ignored` and run with `--include-ignored`. Default `false`
  excludes them, matching plain `cargo test` semantics.
- Selection is across all test binaries built for the package selection
  (lib, integration tests, `--bin` targets compiled with tests, examples,
  benches with `profile.test`). Doctests are **not** selectable via
  `test_filter` in this revision — they are a separate cargo target with
  no libtest `--list` analogue.
- Hung-test protection is provided by `per_test_timeout_secs` (see
  *Timeouts* above), which defaults on under filter mode. A hard
  overall cap is available as `timeout_secs`; in filter mode it
  defaults off, so by default a long matched run is allowed to
  complete as long as each individual test makes progress.
- The response includes two filter-mode trailers in addition to the
  usual records: an `x-cargo-mcp-test-filter-discovery` record that
  reports how many tests each binary enumerated vs. matched, and an
  `x-cargo-mcp-test-filter-summary` trailer with the totals (enumerated,
  matched, launches, exit code).

Use it when you have a focused fix and want to re-run just the
relevant tests without retyping a `--exact` list, or when you want to
run a regex-defined slice of a suite (e.g. "all tests under one
module"). Don't use it for first-time runs of an unfamiliar suite
(plain `cargo_test` is faster when you want everything) or for
doctests.

**Redirecting full output to a file (`output_path`)**

`cargo_check`, `cargo_build`, `cargo_test`, `cargo_clippy`, and
`cargo_doc` accept an optional `output_path`: a relative path (under the
working directory; no `..` components; parent must already exist) that
receives the **complete** NDJSON output. When set, the tool result is a
compact summary instead of the full transcript.

| Always kept in the returned summary | Dropped from the summary (still in the file) |
|---|---|
| `x-cargo-mcp-invocation` (header) | `compiler-artifact`, `build-script-executed` |
| `x-cargo-mcp-output-file` pointer (`path`, `bytes`, `lines`) | `compiler-message` with `level: warning` |
| `compiler-message` with `level: error` (incl. ICE) | passing-test lines (`test foo ... ok`) |
| `build-finished` | captured `println!` replay bodies |
| `x-cargo-mcp-stderr` (when present) | |
| status trailer (`{"status":...}`) | |
| **`cargo_test` only:** libtest summary/failure markers — `running N tests`, ` ... FAILED`, `failures:`, `---- name stdout ----`, `panicked at`, `note: run with`, `test result:` | |

Use it whenever you would otherwise pipe to a temp file (`> build.log`,
`Out-File test-out.txt`) just to keep the response small. Don't use it
for small interactive checks where you want to act on the diagnostics
inline; `cargo_metadata` has its own `output_file` parameter with the
same intent. Workflow: read the summary first; only open the file when
the summary indicates failures worth drilling into.

```jsonc
{ "output_path": "target/cargo-mcp/test-run.ndjson" }
```

### `cargo_metadata`

```jsonc
{
  "working_dir": "/path/to/project",  // optional
  "no_deps": true,                    // optional: omit resolved dependency graph (faster)
  "output_file": "target/meta.json"   // optional: relative path to write metadata JSON to
}
```

### `cargo_test`

```jsonc
{
  "working_dir": "/path/to/project",  // optional
  "package": "my-crate",              // optional
  "test_name": "test_parse",          // optional: filter by name substring
  "exact": true,                      // optional: exact match instead of substring
  "lib": true,                        // optional: only library (unit) tests
  "test": "integration_tests",        // optional: specific integration test target name
  "no_fail_fast": true,               // optional: run all tests even if some fail
  "timeout_secs": 0,                  // optional: overall wall-clock cap; 0 disables
  "per_test_timeout_secs": 30,        // optional: per-test idle watchdog (filter mode only); 0 disables
  "env": { "RUST_BACKTRACE": "1" },   // optional: one-shot env for this call only
  // optional: regex-based selection.
  "test_filter": {
    "pattern": "tests::parser::(commas|braces)$", // required: RE2-style regex
    "include_ignored": false                        // optional, default false
  },
  "output_path": "target/cargo-mcp/test.ndjson" // optional: full NDJSON to file; result is a summary
}
```

### `cargo_tree`

```jsonc
{
  "working_dir": "/path/to/project",  // optional
  "package": "my-crate",              // optional
  "depth": 2,                         // optional: max depth
  "invert": "serde",                  // optional: show what depends on this crate
  "duplicates": true                  // optional: only show duplicate versions
}
```

### `cargo_doc`

```jsonc
{
  "working_dir": "/path/to/project",  // optional
  "no_deps": true,                    // optional: skip dependency docs
  "document_private_items": true      // optional: include private items
}
```

### `cargo_clean`

```jsonc
{
  "working_dir": "/path/to/project",  // optional
  "package": "my-crate",              // optional: clean only this package
  "release": true                     // optional: clean only release artefacts
}
```

### `cargo_update`

```jsonc
{
  "working_dir": "/path/to/project",  // optional
  "package": "serde",                 // optional: update only this dependency
  "precise": "1.0.195"               // optional: pin to an exact version (requires package)
}
```

### `cargo_fix`

```jsonc
{
  "working_dir": "/path/to/project",  // optional
  "package": "my-crate",              // optional
  "allow_dirty": true,                // optional: fix even with uncommitted changes
  "allow_staged": true,               // optional: fix even with staged changes
  "clippy": true                      // optional: also apply Clippy suggestions
}
```

### `cargo_add`

```jsonc
{
  "working_dir": "/path/to/project",  // optional
  "dependency": "serde",              // required: crate name, optionally with version ("serde@1.0")
  "features": "derive",              // optional: comma-separated feature list
  "dev": false,                       // optional: add to [dev-dependencies]
  "build": false,                     // optional: add to [build-dependencies]
  "package": "my-crate"              // optional: target a specific workspace member
}
```

### `cargo_remove`

```jsonc
{
  "working_dir": "/path/to/project",  // optional
  "dependency": "serde",              // required: crate name to remove
  "dev": false,                       // optional: remove from [dev-dependencies]
  "build": false,                     // optional: remove from [build-dependencies]
  "package": "my-crate"              // optional: target a specific workspace member
}
```

### `cargo_publish`

> **Important:** Publishing to crates.io is permanent — a version cannot be deleted.
> Always run with `dry_run: true` first.

```jsonc
{
  "working_dir": "/path/to/project",  // optional
  "package": "my-crate",              // optional: publish a specific workspace member
  "dry_run": true,                    // recommended: validate without uploading
  "allow_dirty": false               // optional: publish with uncommitted changes
}
```

### `cargo_setup`

Takes no parameters. Returns the canonical instruction text that should be added to
your repository's Copilot instruction files so that Copilot prefers MCP tools over
the terminal. See [Quick start](#quick-start) for the recommended workflow.

```jsonc
{}  // no parameters
```

### `cargo_diagnostic`

Returns a structured JSON report describing exactly which `cargo` and `rustc`
binaries cargo-mcp will invoke for the given working directory, why those were
chosen (resolution step), whether a `rust-toolchain.toml` is in effect (and its
contents), and the relevant environment variables (`PATH`, `CARGO`, `RUSTC`,
`RUSTUP_TOOLCHAIN`, `RUSTUP_HOME`, `CARGO_HOME`).

Use this whenever a cargo command appears to use the wrong toolchain.

```jsonc
{
  "working_dir": "/path/to/project"  // optional
}
```

---

## Toolchain resolution

When cargo-mcp spawns `cargo`, it resolves the binary in this priority order
(first match wins):

1. **`CARGO` environment variable** — if set and points to an existing file,
   that path is used. This matches cargo's own behaviour for nested
   invocations and lets you override the choice explicitly.
2. **Rustup proxy** at `$CARGO_HOME/bin/cargo[.exe]` (default
   `~/.cargo/bin/cargo[.exe]` — `%USERPROFILE%\.cargo\bin\cargo.exe` on
   Windows). When found **with** a sibling `rustup` binary, this is the rustup
   proxy and invoking it honours `rust-toolchain.toml` regardless of `PATH`
   ordering. If the sibling rustup is missing, cargo-mcp logs a warning and
   falls back to using the binary anyway.
3. **`PATH` lookup** — if neither of the above produces a result, cargo-mcp
   spawns the bare name `cargo` and lets the OS resolve it.

The resolved path and resolution step are written to the cargo-mcp server's
stderr (visible in VS Code's *MCP Logs: cargo* output channel) before each
invocation.

### `rustc` is pinned alongside `cargo`

Resolving only `cargo` is not enough on systems where a non-rustup directory
containing a stray `rustc` is prepended to `PATH` ahead of the rustup proxy
bin dir. In that situation cargo (even when correctly invoked as the rustup
proxy) would still spawn the stray `rustc` via its own `PATH` lookup,
silently bypassing `rust-toolchain.toml`.

To prevent this, cargo-mcp resolves `rustc` (unless `RUSTC` is already set
in cargo-mcp's environment) with the same env → proxy → PATH order, and
injects `RUSTC=<resolved rustc>` into the spawned cargo's environment. The
injection is also skipped if resolution fell through to the PATH-lookup tier
— there is no concrete path to pin, so the spawned cargo's normal PATH-based
`rustc` lookup applies.

When `RUSTC` is pinned to the rustup proxy `rustc`, the proxy itself defers
toolchain selection to `rust-toolchain.toml`.

If you suspect the wrong toolchain is being used, call the `cargo_diagnostic`
tool — it returns the resolved paths, `cargo --version --verbose` output, the
location and contents of any `rust-toolchain.toml` found by walking ancestor
directories, and the relevant env vars in a single report.

---

## Transient "file in use" failures (Windows)

On Windows, Cargo builds frequently fail mid-flight with messages like:

```
error: failed to remove file `target\debug\foo.exe`:
  The process cannot access the file because it is being used by another
  process. (os error 32)
```

```
error: failed to write `target\debug\foo.pdb`: Access is denied. (os error 5)
```

These are virtually always transient — an antivirus scanner, file indexer, or
the previous `rustc` invocation has briefly grabbed an open handle on a file
in `target\` and will release it within a fraction of a second. Re-running the
exact same cargo command immediately succeeds.

cargo-mcp detects these errors automatically and retries the cargo invocation.
The retry is gated to commands that are inherently idempotent (`check`,
`build`, `test`, `clippy`, `fmt`, `doc`, `tree`, `clean`, `metadata`) and
only fires when cargo's combined output contains a recognised file-busy
pattern: the phrases *being used by another process*, *access is denied*, or
*sharing violation*, or — only when running on Windows — the parenthesised
error codes `(os error 32)` and `(os error 5)`. (The bare codes are gated to
Windows because errno 32 / 5 mean *broken pipe* / *I/O error* on POSIX,
which are not retry-worthy.) Each retry emits a streaming progress
notification so it's visible in the chat panel.

`cargo fix` and `cargo update` are deliberately **not** retried even though
they're nominally read-mostly: a partial first attempt could leave source
files or `Cargo.lock` half-edited, and re-running on top of that state isn't
safe.

It is **on by default**. The behaviour is controlled by three settings:

| VS Code setting | CLI flag | Default | Description |
|---|---|---|---|
| `cargo-mcp.retry.onBusy` | `--retry-on-busy=<bool>` | `true` | Master switch. Disable to make file-busy errors surface immediately. |
| `cargo-mcp.retry.delayMs` | `--retry-delay-ms=<n>` | `500` | Delay between attempts, in milliseconds. |
| `cargo-mcp.retry.maxAttempts` | `--retry-max-attempts=<n>` | `3` | Maximum total attempts (initial try + retries). |

Non-idempotent commands (`cargo_publish`, `cargo_add`, `cargo_remove`,
`cargo_fix`, `cargo_update`) and direct-to-file streaming (the `output_file`
mode of `cargo_metadata`) are **never** retried, regardless of the setting.

### File-busy holder diagnostics (Windows)

When a busy error is detected on Windows, cargo-mcp parses the offending
file paths out of cargo's output and asks the OS's [Restart Manager
APIs](https://learn.microsoft.com/windows/win32/api/restartmanager/) which
processes currently hold open handles on each file. Each holder is
reported by PID, executable name, and process kind (console, GUI app,
service, Explorer, critical system process).

The diagnostic is emitted in two places:

- **Per-retry progress line** — a one-line summary (e.g. `cargo-mcp: file
  held by: rust-analyzer-proc-macro-srv.exe (PID 12345)`) is streamed as
  a progress notification before each retry attempt.
- **Final stderr** — a multi-line block listing every busy file and its
  holders is appended to the captured stderr if the operation ultimately
  fails (or runs without retry).

The query is best-effort: if Restart Manager is unavailable, access is
denied, or the file has already been released, the per-file entry records
the reason and the rest of the report still renders. On non-Windows hosts
the path-extraction step still runs but no holder lookup is performed
(Restart Manager is a Windows-only API).

---

## Incremental-session finalise advisory (Windows Dev Drive / ReFS)

On Windows volumes that use ReFS — including Dev Drive — rustc occasionally
cannot rename the `-working` incremental compilation session directory to its
final name after a build. When this happens rustc emits:

```
warning: error finalizing incremental compilation session directory `...`: ...
```

This is an advisory about the incremental cache, not about the source code.
The build itself succeeded. The only consequence is that **the next build
cannot reuse work from this compilation** — it falls back to a full rebuild
for the affected crate.

cargo-mcp handles this in two ways:

**Proactive cleanup (`--clear-incr-working`)** — if the `--clear-incr-working=true`
flag is passed at startup (or `cargo-mcp.clearIncrWorking` is enabled in VS Code
settings), cargo-mcp walks `target/<profile>/incremental/` before each cargo
invocation and removes any stale `*-working` directories left behind by a
previous failed finalise. This prevents the rename from failing in the first
place on the next run.

**Diagnostic demotion** — the advisory is demoted from `warning` (or `error`
when `-D warnings` is active) to `note` in the NDJSON stream that
cargo-mcp returns. This keeps it out of the error summary and prevents it
from being mistaken for a compile failure.

If the advisory is the **only** reason cargo exited non-zero (which only
happens when `-D warnings` is active), cargo-mcp overrides the exit code to
0 and injects an `x-cargo-mcp-note` record explaining the situation, so the
AI agent sees a coherent "build succeeded" picture.

This advisory is fixed natively in rustc 1.96.0: the diagnostic was changed
from `warn` to `note`, which is unaffected by `-D warnings`.

---

## Building from source

For contributors or platforms not covered by a pre-built release:

```sh
git clone https://github.com/MikeGrier/cargo-mcp-rs
cd cargo-mcp-rs
cargo build --release -p cargo-mcp
```

| Platform | Binary |
|---|---|
| Windows | `target/release/cargo-mcp.exe` |
| Linux / macOS | `target/release/cargo-mcp` |

Or install directly to `~/bin`:

```sh
cargo install --path crates/cargo-mcp --root ~
```

Then configure VS Code manually as shown in the
[Manual server configuration](#manual-server-configuration) section above.

