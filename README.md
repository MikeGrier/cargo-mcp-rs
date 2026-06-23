# cargo-mcp

`cargo-mcp` is a [Model Context Protocol (MCP)](https://modelcontextprotocol.io/) server
that exposes Cargo's build system as tools callable by AI agents such as GitHub Copilot
in VS Code.

Instead of routing every `cargo check`, `cargo test`, or `cargo build` through a
terminal, Copilot can invoke these operations directly via the MCP tool interface —
getting structured JSON output where available, with no interactive shell or pager
involved. [cargo-nextest](https://nexte.st/) is supported as a first-class
alternative test runner via `cargo_nextest_run` / `cargo_nextest_list` when the
plugin is installed.

---

## Installation — VS Code Marketplace (Windows only)

> **Windows only:** Pre-built VSIXes are currently provided for `win32-x64`
> and `win32-arm64` only. Linux and macOS users should follow the
> [manual installation](#installation--manual-build-from-source) steps below.

Install the **cargo-mcp** extension from the VS Code Marketplace:

1. Open VS Code.
2. Press `Ctrl+Shift+X` to open the Extensions panel.
3. Search for **cargo-mcp**.
4. Click **Install**.

The extension bundles the pre-built `cargo-mcp` binary and registers the MCP server
automatically. No manual configuration is needed — Copilot Chat will discover the
tools as soon as the extension activates.

> **Requires:** VS Code 1.101 or later and GitHub Copilot Chat.

---

## Installation — manual (build from source)

If you prefer to build from source, clone this repository and run:

```sh
cargo install --path crates/cargo-mcp --root ~
```

This compiles the binary in release mode and places it in `~/bin/`:

| Platform | Installed binary |
|---|---|
| Windows | `%USERPROFILE%\bin\cargo-mcp.exe` |
| Linux / macOS | `~/bin/cargo-mcp` |

Then configure VS Code to use it (see [Manual VS Code configuration](#manual-vs-code-configuration) below).

### Alternative — build only (no install)

```sh
cargo build --release -p cargo-mcp
```

| Platform | Binary location |
|---|---|
| Windows | `target/release/cargo-mcp.exe` |
| Linux / macOS | `target/release/cargo-mcp` |

---

## Manual VS Code configuration

Skip this section if you installed via the Marketplace — it handles configuration automatically.

MCP servers are configured via a `.vscode/mcp.json` file in the workspace or in
VS Code user settings.

### Option 1 — per-workspace (`.vscode/mcp.json`)

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

### Option 2 — user-global (`settings.json`)

Open VS Code user settings (`Ctrl+Shift+P` → **Preferences: Open User Settings (JSON)**) and add:

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

After saving, run **Developer: Reload Window** (`Ctrl+Shift+P`) to pick up the change.

> `${userHome}` is resolved by VS Code to the current user's home directory on all
> platforms. On Windows the binary is `cargo-mcp.exe` on disk, but VS Code appends
> `.exe` automatically, so the same configuration works cross-platform.
>
> If you used "build only" (no install), replace the command with the full path to the binary:
> ```jsonc
> "command": "/absolute/path/to/target/release/cargo-mcp"
> ```

---

## Prerequisites

The MCP server delegates all work to the `cargo` toolchain already on your PATH.

| Requirement | Notes |
|---|---|
| Rust toolchain | stable |
| `rustup` | optional but **strongly recommended** if your repository uses a `rust-toolchain.toml`. Without rustup, the toolchain file has no effect on *any* cargo invocation — see [Toolchain resolution](#toolchain-resolution). |
| `cargo clippy` | `rustup component add clippy` (required for `cargo_clippy`) |
| `cargo fmt` | `rustup component add rustfmt` (required for `cargo_fmt` / `cargo_fmt_check`) |

---

## Tool reference

All tools accept an optional `working_dir` parameter — an absolute path to the
directory containing the `Cargo.toml` to operate on. If omitted, the server's own
working directory is used.

Tools that support it use `--message-format=json`, returning NDJSON (one JSON object
per line). Tools without a stable JSON mode return plain text.

| Tool | Output | Mutates | Description |
|---|---|---|---|
| `cargo_metadata` | JSON | no | Workspace/package structure, dependencies, features, resolved graph |
| `cargo_check` | NDJSON | no | Fast compile-error checking without producing binaries |
| `cargo_build` | NDJSON | yes | Full compilation; diagnostics + build status |
| `cargo_test` | NDJSON+text | no | Run tests; compilation JSON then test-harness text results |
| `cargo_clippy` | NDJSON | no | Lint analysis with suggested fixes |
| `cargo_fmt_check` | text | no | Check formatting; returns diff of changes that would be applied |
| `cargo_fmt` | text | yes | Auto-format source code in place |
| `cargo_tree` | text | no | Dependency tree (use `cargo_metadata` for structured data) |
| `cargo_doc` | NDJSON | yes | Build HTML documentation; returns build warnings |
| `cargo_clean` | text | yes | Remove build artefacts (destructive) |
| `cargo_update` | text | yes | Update Cargo.lock to latest compatible versions |
| `cargo_fix` | text | yes | Auto-apply compiler and Clippy machine-applicable fixes |
| `cargo_add` | text | yes | Add a dependency to Cargo.toml |
| `cargo_remove` | text | yes | Remove a dependency from Cargo.toml |
| `cargo_publish` | text | yes | Package and upload to crates.io (irreversible — use `dry_run: true` first) |
| `cargo_nextest_run` | NDJSON+text | no | Run tests via [cargo-nextest](https://nexte.st/) — per-test process isolation, built-in retries, filter expressions. Requires the `cargo-nextest` plugin; the tool emits install instructions when missing. Does **not** support doctests — use `cargo_test` with `doc: true` for those. |
| `cargo_nextest_list` | NDJSON | no | Enumerate tests via `cargo nextest list --message-format json`. Framed as cargo-mcp's standard NDJSON stream: invocation header + compacted JSON payload line(s) + status trailer. |
| `cargo_setup` | text | no | Return the canonical cargo-mcp instruction text for Copilot setup |
| `cargo_diagnostic` | JSON | no | Report which `cargo` / `rustc` binary will be invoked, the active `rust-toolchain.toml` (if any), and relevant env (`PATH`, `CARGO`, `RUSTC`, `RUSTUP_TOOLCHAIN`, `RUSTUP_HOME`, `CARGO_HOME`) |

### Common parameters

These parameters are available on most tools:

| Parameter | Type | Description |
|---|---|---|
| `working_dir` | string | Absolute path to the Cargo.toml directory |
| `package` | string | Target a specific package within the workspace |
| `release` | boolean | Use the release profile |
| `features` | string | Comma-separated list of features to activate |
| `all_targets` | boolean | Include all targets (lib, bins, tests, benches, examples) |
| `locked` | boolean | Require `Cargo.lock` to remain unchanged (useful in CI) |

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
  "package": "my-crate",              // optional: test one package
  "test_name": "test_parse",          // optional: filter by test name substring
  "exact": true,                      // optional: exact match instead of substring
  "lib": true,                        // optional: only library (unit) tests
  "test": "integration_tests",        // optional: specific integration test target
  "no_fail_fast": true,               // optional: run all tests even if some fail
  "timeout_secs": 0,                  // optional: hard OVERALL wall-clock cap; 0 disables
  "per_test_timeout_secs": 30,        // optional: per-test idle watchdog (filter mode only); 0 disables
  // optional: regex-based selection. When set, cargo_test enumerates
  // tests via `--list`, matches names against the regex, and runs ONLY
  // matching cases via `--exact` — one cargo process per test binary
  // that has matches (additional launches only when the OS argv length
  // limit forces chunking). Hung-test protection in filter mode is
  // provided by `per_test_timeout_secs` (default on at 30s); `timeout_secs`
  // is a separate hard overall cap (default off in filter mode).
  "test_filter": {
    "pattern": "tests::parser::(commas|braces)$", // required: RE2 regex
    "include_ignored": false                       // optional, default false
  }
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
cargo's own record types. This lets you reconstruct exactly what was
invoked from the tool-result panel even when the JSON `arguments` shown
by the MCP client are sparse.

For **JSON-mode tools** (`check`, `build`, `test`, `clippy`, `doc`,
`metadata`) the *entire* response is a strict NDJSON stream — the
invocation header followed by one JSON object per line — so you can
parse the whole response with a single line-by-line JSON parser. On
failure cargo-mcp appends a `{"status":"error","exit_code":N}` trailer
record, and when the cargo child wrote anything to stderr (where the
Restart Manager "who holds this file" report and other side-channel
diagnostics land) a separate `{"reason":"x-cargo-mcp-stderr","text":...}`
record is appended after the trailer. For **text-mode tools** (`fmt`,
`tree`, `clean`, `update`, `fix`, `add`, `remove`, `publish`) only the
first line is JSON; the body that follows is the cargo child's combined
stdout/stderr and is not guaranteed to be JSON. Streaming progress
notifications are also emitted while the build runs; the final
notification reads `cargo <verb> finished` (or `failed`), with the
optional target triplet appended when one is supplied.

---

## Toolchain resolution

When cargo-mcp spawns `cargo` it resolves the binary in this priority order
(first match wins):

1. **`CARGO` environment variable** — if set and points to an existing file,
   that path is used.
2. **Rustup proxy** at `$CARGO_HOME/bin/cargo[.exe]` (default
   `~/.cargo/bin/cargo[.exe]`, `%USERPROFILE%\.cargo\bin\cargo.exe` on
   Windows). When found **with** a sibling `rustup` binary this is the
   rustup proxy; invoking it honours `rust-toolchain.toml` regardless of
   `PATH` ordering. If the sibling rustup is missing, cargo-mcp logs a
   warning and uses the binary anyway.
3. **`PATH` lookup** — fallback to spawning the bare name `cargo`.

The resolved path and resolution step are logged to the cargo-mcp server's
stderr (visible in VS Code's *MCP Logs: cargo* output channel) before each
invocation. Run the `cargo_diagnostic` tool for a one-shot structured report
covering the resolved binaries, `cargo --version --verbose`, the discovered
`rust-toolchain.toml` (and its contents), and the relevant environment
variables.

---

## License

See [LICENSE](LICENSE).
