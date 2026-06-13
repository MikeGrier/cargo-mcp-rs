# GitHub Copilot Instructions — cargo-mcp-rs

## Cargo commands — use MCP tools, never the terminal

This repository ships a **cargo-mcp MCP server** that exposes every common
`cargo` command as a first-class MCP tool. The server provides structured
output, streaming progress notifications, and safe elicitation for destructive
operations.

**Rule:** When working in any Rust/Cargo project, ALWAYS use the `cargo_*` MCP
tools listed below instead of running `cargo` commands in a PowerShell or bash
terminal. This applies even inside a larger workflow — do not switch to the
terminal for cargo just because a previous step used the terminal.

Two specific things that are NOT reasons to fall back to the terminal:

- **Filtering output** (e.g. piping `cargo test` into `Select-String`/`grep`
  or `Select-Object -First N`). The MCP tools return the full structured
  output — call the tool, then filter the returned text yourself. Do not shell
  out just to grep.
- **A toolchain override** (e.g. `cargo +nightly test`, `cargo +ms-prod build`).
  Pass the `toolchain` parameter instead — see below.


| MCP tool | Replaces |
|---|---|
| `cargo_metadata` | `cargo metadata` |
| `cargo_check` | `cargo check` |
| `cargo_build` | `cargo build` |
| `cargo_test` | `cargo test` |
| `cargo_clippy` | `cargo clippy` |
| `cargo_fmt_check` | `cargo fmt --check` |
| `cargo_fmt` | `cargo fmt` |
| `cargo_tree` | `cargo tree` |
| `cargo_doc` | `cargo doc` |
| `cargo_clean` | `cargo clean` |
| `cargo_update` | `cargo update` |
| `cargo_fix` | `cargo fix` |
| `cargo_add` | `cargo add` |
| `cargo_remove` | `cargo remove` |
| `cargo_publish` | `cargo publish` |

### When to use each tool

- **Check / build / test / clippy / doc** — always prefer these over terminal;
  they stream structured progress back to VS Code.
- **`cargo_clean`** — use before a clean rebuild; do not run `cargo clean` in
  the terminal.
- **`cargo_fmt` / `cargo_fmt_check`** — use for formatting checks in CI-like
  workflows inside the editor.
- **`cargo_add` / `cargo_remove` / `cargo_update`** — always use for
  dependency management; never manually edit Cargo.toml for dependency version
  changes when these tools are available.
- **`cargo_fix`** — use after `cargo_check` or `cargo_clippy` to apply
  machine-applicable fixes in bulk.
- **`cargo_publish`** — always run with `dry_run: true` first to validate;
  only publish for real when the dry-run succeeds.

### Toolchain override (`+toolchain`)

To run a command with a specific rustup toolchain — the equivalent of
`cargo +nightly ...` or `cargo +ms-prod ...` — pass the `toolchain` parameter
(e.g. `"toolchain": "nightly"` or `"toolchain": "ms-prod"`). The server
prepends the `+<toolchain>` token for you. Supported on `cargo_check`,
`cargo_build`, `cargo_test`, `cargo_clippy`, `cargo_doc`, `cargo_tree`,
`cargo_fmt`, and `cargo_fmt_check`. Never shell out to the terminal just to
apply a `+toolchain` override.

### Environment variables (`env`)

Every cargo-running MCP tool accepts an optional `env` object that sets or
unsets environment variables on the cargo subprocess for that one call. Keys
are env var names; values are either a string (set the variable) or `null`
(remove it from the child's environment). The map is layered on top of
cargo-mcp's built-in defaults (`CARGO_TERM_COLOR`, `NO_COLOR`, `RUSTC`), so
a caller-supplied value wins, and the resulting block is what cargo-mcp hands
to the OS as the child's environment.

Use this instead of shelling out to a terminal just to apply an env var (e.g.
`RUSTFLAGS`, `RUST_LOG`, `RUSTC_BOOTSTRAP`, compiler-internal dumps such as
`FIREBIRD_DUMP_MIR`):

```json
{ "env": { "RUSTFLAGS": "-C debuginfo=2", "FIREBIRD_DUMP_MIR": "1" } }
```

When to use `env`:

- One-shot debug knobs (`RUSTFLAGS`, `RUST_LOG`, `RUST_BACKTRACE`,
  `RUSTC_BOOTSTRAP`, compiler-internal dumps) that only this single tool
  call needs.
- Reproducing an issue under a specific env without restarting the MCP
  server or polluting the host shell.

When NOT to use `env`:

- Permanent / project-wide config — put it in `Cargo.toml`,
  `.cargo/config.toml`, or `rust-toolchain.toml` instead.
- Secrets. The block is passed verbatim to the cargo child process (and so
  is visible via OS-level process inspection), and may be captured by
  future logging additions — treat it as not confidential.

### Redirecting full output to a file (`output_path`)

`cargo_check`, `cargo_build`, `cargo_test`, `cargo_clippy`, and `cargo_doc`
accept an optional `output_path`: a relative path (under the working
directory; no `..` components; parent must already exist) that receives the
**complete** NDJSON output. When set, the tool result is a compact
**summary** instead of the full transcript:

| Always kept in summary | Dropped from summary (still in file) |
|---|---|
| `x-cargo-mcp-invocation` (header) | `compiler-artifact`, `build-script-executed` |
| `x-cargo-mcp-output-file` pointer (`path`, `bytes`, `lines`) | `compiler-message` with `level: warning` |
| `compiler-message` with `level: error` (incl. ICE) | passing-test lines (`test foo ... ok`) |
| `build-finished` | captured `println!` replay bodies |
| `x-cargo-mcp-stderr` (when present) | |
| status trailer (`{"status":...}`) | |
| **`cargo_test` only:** libtest summary/failure markers — `running N tests`, ` ... FAILED`, `failures:`, `---- name stdout ----`, `panicked at`, `note: run with`, `test result:` | |

**Use `output_path` when:**

- The full output would be large enough to bloat your context (long
  `cargo_test` runs, big workspaces, `cargo_build` with many crates).
- You'd otherwise pipe to a temp file (`> build.log`,
  `Out-File test-out.txt`) just to keep the response small. Pass
  `"output_path": "target/cargo-mcp/<run>.ndjson"` instead.

**Don't use `output_path` when:**

- You want the diagnostics inline so you can act on them immediately
  (small `cargo_check` / `cargo_clippy` after a focused edit).
- The tool isn't one of the five listed above; `cargo_metadata` has its
  own `output_file` parameter with the same intent.

**Workflow:** read the summary first. If it shows a non-zero `exit_code`
or failure markers, open the file at `output_path` for the full
transcript (which contains every dropped warning, captured stdout,
artifact line, etc.).

```json
{ "output_path": "target/cargo-mcp/test-run.ndjson" }
```

### cargo_test — timeouts

`cargo_test` has two independent timeout knobs that can be combined freely.
Both apply only to the test **execution** phase: each clock arms when
compilation and linking finish (cargo's `build-finished` record), so a slow
build never trips either of them.

**`timeout_secs` — hard OVERALL wall-clock cap.** Same meaning whether or
not `test_filter` is set. Bounds the whole execution phase (in filter mode:
across all per-binary launches together). When it elapses, cargo's entire
process tree is terminated and a `TimeoutError` is returned with whatever
NDJSON was already streamed. Use this knob to keep throughput going on a
slow system.

- **Without `test_filter`:** when launched by the VS Code extension, the
  server applies a default from `cargo-mcp.test.timeoutSecs` (**30
  seconds** by default; `0` disables it). Without the extension, there is
  no default.
- **With `test_filter`:** there is **no overall default** in filter mode.
  Omit it to let a long matched run complete unbounded; pass an explicit
  positive value to cap it. (The per-test watchdog below catches hung
  tests; the overall cap is only for bounding total wall time.)
- Pass `timeout_secs: 0` to disable the overall cap for this run
  regardless of the server default.

**`per_test_timeout_secs` — per-test idle watchdog.** Only meaningful when
`test_filter` is set; **ignored otherwise**. Arms on the per-binary
invocation's `build-finished` record, then resets to `now +
per_test_timeout_secs` every time the libtest harness emits a test
boundary line (`running N tests` or `test <name> ... ok|FAILED|ignored`).
A long suite of fast tests never trips it; a single hung test does. If it
fires for one binary, that binary's cargo process tree is killed and the
orchestrator records `exit_code: -1` plus an inline `cargo-mcp
test_filter: per-binary run failed: TimeoutError` body for it, then moves
on to the next matched binary — so one hung test does not block the rest
of the filter run from completing.

- Omit under `test_filter` to let the server default apply (same
  `cargo-mcp.test.timeoutSecs` source, **30 seconds** under the VS Code
  extension). When the server default is absent or set to `0`, filter
  mode still applies a hard-coded **30-second** fallback so hung-test
  protection is on by default. The only way to fully disable per-test
  protection for a call is to pass `per_test_timeout_secs: 0`
  explicitly in the tool arguments.
- Combine with `timeout_secs` when you want both ("kill a hung test fast,
  but also cap the whole run").

When to override the budgets:

- **Raise / disable `timeout_secs`** for unfiltered runs you *know* are
  slow — long integration suites, tests that internally sleep or poll,
  benchmark-style tests. Better to disable for one call than chase a
  spurious `TimeoutError`.
- **Raise / disable `per_test_timeout_secs`** for filter runs where a
  single matched test legitimately takes longer than the default (e.g.
  an integration test that bootstraps a fixture). The clock resets on
  every boundary, so this only matters for tests that block silently for
  longer than the budget.
- **Lower either** when sanity-checking a fix and you want fast feedback
  if the change regressed something into an infinite loop.
- Otherwise leave both at the server default.

### cargo_test — regex-based test selection (`test_filter`)

`cargo_test` accepts an optional `test_filter` object that runs only the
tests whose names match a regex. The server does one `--no-run` build,
enumerates tests with libtest's `--list`, matches names against the
regex, then launches **one cargo process per test binary that has
matches** (additional launches only when the OS argv length limit forces
chunking the name list — never more than necessary).

```json
{
  "test_filter": {
    "pattern": "tests::parser::(commas|braces)$",
    "include_ignored": false
  }
}
```

- `pattern` is required and uses the Rust `regex` crate (RE2-style:
  linear-time, no backreferences, no lookaround). It's matched against
  the libtest test name (typically `module::path::test_name`); add `^` /
  `$` for a full-name anchor, otherwise it matches as a substring.
  IMPORTANT: integration tests (under `tests/`) enumerate **without** a
  `module::` prefix, while unit tests inside the crate enumerate as
  `mod::sub::test_name`. A leading `^` anchor binds to that prefix —
  `^foo` matches integration test `foo` but NOT unit test
  `tools::tests::foo`. To span both, drop the anchor (substring) or
  include both forms in the alternation
  (e.g. `^(foo|tools::tests::foo)$`).
- `include_ignored` defaults to `false`. Set `true` to enumerate ignored
  tests via `--list --ignored` and run them with `--include-ignored`.
- With `test_filter` set, hung-test protection is provided by the
  separate **`per_test_timeout_secs`** knob (see the *timeouts* section
  above) — the watchdog arms on each per-binary `build-finished` and
  resets on every libtest boundary line. `timeout_secs` is still
  available as a hard overall cap on the whole filter run; it is off by
  default in filter mode so a long matched run can complete.
- Doctests are **not** selectable via `test_filter` in this revision —
  use the regular `cargo_test` flow if you need to target a doctest.
- The response adds two filter-mode trailers to the normal records:
  `x-cargo-mcp-test-filter-discovery` (the full filter plan — per-binary
  enumerated/matched counts plus the matched name list) and
  `x-cargo-mcp-test-filter-summary` (rollup totals — binaries
  discovered, tests enumerated/matched, launches, exit code).

When to use `test_filter`:

- Re-running a focused slice after a fix instead of the whole suite,
  without having to retype a `--exact` list.
- Selecting tests by a regex pattern across multiple modules / binaries
  in one call.

When **not** to use `test_filter`:

- First-time runs of an unfamiliar suite — plain `cargo_test` with no
  filter is faster when you want everything.
- Targeting doctests — not supported in this revision.
- When you'd be matching every test anyway (`.*`) — that's just the
  unfiltered path with extra overhead.

### Reading cargo_test output

`cargo_test` returns a strict NDJSON stream. Parse it line-by-line; every
non-blank line is a JSON object. The `reason` field identifies the record type:

| `reason` | Content | Key fields |
|---|---|---|
| `x-cargo-mcp-invocation` | Effective command and working dir (first line) | `argv`, `cwd` |
| `compiler-message` | Compilation error or warning | `message` (rustc diagnostic) |
| `build-finished` | Build phase outcome | `success` (bool) |
| `x-cargo-mcp-test-output` | One line of libtest harness output or captured `println!` | `text` |
| `x-cargo-mcp-stderr` | `eprintln!` and other test stderr (when non-empty) | `text` |
| `x-cargo-mcp-test-filter-discovery` | (filter mode only) Top-level filter plan: `pattern`, `include_ignored`, `tests_enumerated`, `tests_matched`, `launches_planned`, `binaries[]` (each `package`, `target`, `kind`, `executable`, `tests_enumerated`, `tests_matched`, `matched[]`), `enumeration_errors[]` |  |
| `x-cargo-mcp-test-filter-summary` | (filter mode only, last filter trailer) Rollup totals: `pattern`, `binaries_discovered`, `binaries_with_matches`, `tests_enumerated`, `tests_matched`, `launches`, `status`, `exit_code` |  |
| *(last line)* | Exit status | `status` (`"success"` or `"error"`), `exit_code` (on error) |

`println!` inside tests is captured by libtest and replayed as
`x-cargo-mcp-test-output` lines only when the test fails (standard
libtest behaviour). `eprintln!` bypasses libtest capture and always
appears in `x-cargo-mcp-stderr`.

## File encoding

Source files in this repository may contain non-ASCII characters. When editing
files, prefer the editor's built-in edit tools over PowerShell file I/O
(`Set-Content`, `Out-File`, `>`) to avoid encoding corruption.


## Responding to PR review comments

When addressing review comments on a pull request (Copilot reviewer or
human), **reply on each individual review thread**, not only with a
summary comment on the PR conversation.

Why: as a PR accumulates more rounds of review, a single summary comment
makes it impossible to tell from the GitHub UI which inline threads are
old vs. new, or which have been addressed. A per-thread reply leaves an
"author replied" marker on each line where the reviewer raised an issue.

Procedure:

1. Fetch the inline review comments and their IDs:
   ```pwsh
   gh api repos/<owner>/<repo>/pulls/<num>/comments --jq '.[] | {id, path, line, user: .user.login, body: (.body | .[0:80])}'
   ```
2. For each comment, post a reply to that specific thread:
   ```pwsh
   gh api -X POST "repos/<owner>/<repo>/pulls/<num>/comments/<comment-id>/replies" -F body=@reply.md
   ```
3. Keep each reply short and concrete: name the commit SHA that
   addressed it and (when small) quote the new behaviour. If you
   intentionally chose not to act on a comment, say so on that thread.
4. A summary comment on the PR conversation is fine *in addition*, but
   never *instead*.

The VS Code GitHub Pull Request extension's `resolveReviewThread` tool
often reports `canResolve: false` for Copilot-authored threads; in that
case post the per-thread reply via `gh api` as above and let the human
maintainer resolve.