# cargo-mcp

`cargo-mcp` is a [Model Context Protocol (MCP)](https://modelcontextprotocol.io/) server
that exposes Cargo's build system as tools callable by AI agents such as GitHub Copilot
in VS Code.

Instead of routing every `cargo check`, `cargo test`, or `cargo build` through a
terminal, Copilot can invoke these operations directly via the MCP tool interface —
getting structured JSON output where available, with no interactive shell or pager
involved.

---

## Installation — VS Code Marketplace (recommended)

Install the **cargo-mcp** extension from the VS Code Marketplace:

1. Open VS Code.
2. Press `Ctrl+Shift+X` (or `Cmd+Shift+X` on macOS) to open the Extensions panel.
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
| `cargo_build` | NDJSON | yes | Full compilation; diagnostics + artefact info |
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
| `cargo_setup` | text | no | Return the canonical cargo-mcp instruction text for Copilot setup |

### Common parameters

These parameters are available on most tools:

| Parameter | Type | Description |
|---|---|---|
| `working_dir` | string | Absolute path to the Cargo.toml directory |
| `package` | string | Target a specific package within the workspace |
| `release` | boolean | Use the release profile |
| `features` | string | Comma-separated list of features to activate |
| `all_targets` | boolean | Include all targets (lib, bins, tests, benches, examples) |

### `cargo_metadata`

```jsonc
{
  "working_dir": "/path/to/project",  // optional
  "no_deps": true                     // optional: omit resolved dependency graph (faster)
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

---

## License

See [LICENSE](LICENSE).
