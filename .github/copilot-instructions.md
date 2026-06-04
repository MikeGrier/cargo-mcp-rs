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

### cargo_test — timeout

`cargo_test` has a server-side default timeout configured by the
`cargo-mcp.test.timeoutSecs` VS Code setting (default: **30 seconds**).
- You do NOT need to pass `timeout_secs` for a normal test run; the default
  applies automatically.
- Pass `timeout_secs: N` to override the default for a single run.
- Pass `timeout_secs: 0` to disable the timeout entirely for a single run
  (e.g. a test suite known to take several minutes).

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
| *(last line)* | Exit status | `status` (`"success"` or `"error"`), `exit_code` (on error) |

`println!` inside tests is captured by libtest and replayed as
`x-cargo-mcp-test-output` lines only when the test fails (standard
libtest behaviour). `eprintln!` bypasses libtest capture and always
appears in `x-cargo-mcp-stderr`.

## File encoding

Source files in this repository may contain non-ASCII characters. When editing
files, prefer the editor's built-in edit tools over PowerShell file I/O
(`Set-Content`, `Out-File`, `>`) to avoid encoding corruption.
