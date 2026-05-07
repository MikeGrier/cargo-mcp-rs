# cargo-mcp — Cargo tools for GitHub Copilot

`cargo-mcp` is a [Model Context Protocol (MCP)](https://modelcontextprotocol.io/)
server that gives GitHub Copilot direct access to Cargo's build system. Instead of
running `cargo check`, `cargo build`, `cargo test`, and friends in a terminal,
Copilot calls them as structured tools — getting rich diagnostics with exact file
paths and line numbers it can act on immediately, with streaming progress as
operations run.

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

Every tool result begins with a one-line invocation header that records the
*effective* command cargo-mcp ran, including any flags the dispatch layer
added implicitly (e.g. `--message-format=json`):

```
$ cargo check --message-format=json --all-targets
(cwd: /path/to/project)
```

This lets you reconstruct exactly what was invoked from the tool-result panel
even when the JSON `arguments` shown by the MCP client are sparse (for
example, only `working_dir` and a single boolean flag).

For JSON-mode tools (`check`, `build`, `test`, `clippy`, `doc`, `metadata`)
the body of the result is NDJSON — one JSON object per line — filtered to
keep only `compiler-message` and `build-finished` records. While the build
runs, streaming progress notifications are also emitted; the final
notification reads `cargo <verb> finished` (or `failed`), with the optional
target triplet appended when one is supplied. This is what appears as the
collapsed summary line in the VS Code chat history.

For tools without a JSON mode (`fmt`, `tree`, `clean`, `update`, `fix`,
`add`, `remove`, `publish`) the body is the combined stdout/stderr of cargo,
prefixed with the same invocation header.

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

Most tools accept these optional parameters:

| Parameter | Type | Description |
|---|---|---|
| `working_dir` | string | Absolute path to the Cargo.toml directory. Defaults to cwd. |
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
  "package": "my-crate",              // optional
  "test_name": "test_parse",          // optional: filter by name substring
  "exact": true,                      // optional: exact match instead of substring
  "lib": true,                        // optional: only library (unit) tests
  "test": "integration_tests",        // optional: specific integration test target name
  "no_fail_fast": true                // optional: run all tests even if some fail
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

If you suspect the wrong toolchain is being used, call the `cargo_diagnostic`
tool — it returns the resolved paths, `cargo --version --verbose` output, the
location and contents of any `rust-toolchain.toml` found by walking ancestor
directories, and the relevant env vars in a single report.

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
