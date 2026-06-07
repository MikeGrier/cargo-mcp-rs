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

### cargo_test — timeout

When launched by the VS Code extension, `cargo_test` applies a server-side
default timeout from the `cargo-mcp.test.timeoutSecs` setting (**30 seconds**
by default). Without the extension (or with `cargo-mcp.test.timeoutSecs` set
to `0`), the server has no default timeout. The budget covers only test
**execution** — the clock starts when compilation and linking finish (cargo's
`build-finished` record), so a slow build never trips the timeout.
- Omit `timeout_secs` to let the server default apply.
- Pass `timeout_secs: N` to use a specific budget for this run.
- Pass `timeout_secs: 0` to disable the timeout for this run, regardless of
  the server default.

When to override `timeout_secs`:

- **Raise it** (or pass `0` to disable) for runs you *know* are slow —
  long-running integration suites, a single targeted test that internally
  sleeps or polls, a benchmark-style test. Better to disable the timeout
  for one call than to chase a spurious `TimeoutError`.
- **Lower it** when you're sanity-checking a fix and want fast feedback
  if the change regressed something into an infinite loop.
- Otherwise leave it at the server default — the budget only covers
  execution, so a slow *build* never trips it.

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
