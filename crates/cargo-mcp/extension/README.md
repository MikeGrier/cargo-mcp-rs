# Cargo MCP for VS Code

Give GitHub Copilot direct access to Cargo's build system. Instead of running
`cargo check`, `cargo build`, `cargo test`, and friends in a terminal, Copilot
calls them as structured [Model Context Protocol](https://modelcontextprotocol.io/)
tools — getting rich diagnostics with exact file paths and line numbers it can
act on immediately, with streaming progress as operations run.

This extension bundles the `cargo-mcp` server binary and registers it with
VS Code automatically — no manual `mcp.json` configuration required.

> **Platforms:** This extension ships pre-built binaries for **Windows
> (x64 and arm64)** only. Users on Linux/macOS should
> [build from source](https://github.com/MikeGrier/cargo-mcp-rs#building-from-source)
> and configure the server manually.

---

## Demo

Copilot driving a clean rebuild of this repository through the MCP tools —
`cargo_clean`, then a cross-architecture `cargo_build` and `cargo_test` for
both `x86_64-pc-windows-msvc` and `aarch64-pc-windows-msvc` — without ever
touching a terminal:

![Copilot using cargo-mcp to clean, build, and test cargo-mcp itself for both Windows x64 and arm64 targets](media/demo-clean-build-test.png)

Each tool result starts with the exact `cargo` invocation (including any flags
the dispatch layer added) and ends with the streaming summary line you see in
the chat history (`cargo build finished (aarch64-pc-windows-msvc)` etc.).

---

## Quick start

1. **Install the extension** (you're here).
2. **Reload VS Code** so the MCP server registers and the slash commands appear.
3. **Open Copilot Chat in Agent mode** and run:
   ```
   /cargo-mcp:setup
   ```
   This adds a short instruction block to your repository's
   `.github/copilot-instructions.md` (or equivalent) telling Copilot to prefer
   the MCP tools over running `cargo` in a terminal. Commit the change and
   Copilot will use the tools for every future session in that repository.

> **Why the setup step?** Tool descriptions are only visible to Copilot *after*
> it has already decided how to carry out a task. A repository instruction file
> is loaded *before* that decision, so it reliably intercepts the choice before
> Copilot reaches for a terminal.

---

## Tools provided

| Tool | Purpose |
|---|---|
| `cargo_check` | Fast compile-error checking (NDJSON diagnostics) |
| `cargo_build` | Full compilation with diagnostics + build status |
| `cargo_test` | Run tests; structured results |
| `cargo_clippy` | Lints with machine-applicable suggestions |
| `cargo_fmt` / `cargo_fmt_check` | Apply / verify formatting |
| `cargo_doc` | Build documentation |
| `cargo_tree` | Dependency tree |
| `cargo_metadata` | Workspace / package / dependency graph |
| `cargo_clean` | Remove build artefacts (with elicitation prompt) |
| `cargo_update` | Update `Cargo.lock` |
| `cargo_fix` | Apply machine-applicable fixes in bulk |
| `cargo_add` / `cargo_remove` | Dependency management |
| `cargo_publish` | Publish to crates.io (`dry_run` recommended first) |
| `cargo_diagnostic` | Report which `cargo`/`rustc` will be invoked, the active `rust-toolchain.toml`, and relevant env |
| `cargo_setup` | Return the canonical instruction text used by `/cargo-mcp:setup` |

For `cargo_check` and `cargo_clippy`, machine-applicable suggestions are
surfaced through an interactive elicitation prompt (configurable via the
`cargo-mcp.elicitationMode` setting).

---

## Output format

Every tool result begins with a one-line invocation header recording the
*effective* command, including any flags the dispatch layer added implicitly:

```
$ cargo check --message-format=json --all-targets
(cwd: /path/to/project)
```

For JSON-mode tools the body is NDJSON (one JSON object per line). Streaming
progress notifications run while cargo is working; the final notification —
shown as the collapsed summary line in chat history — reads
`cargo <verb> finished` (or `failed`), with an optional target triplet
appended when one is supplied.

---

## Toolchain resolution

When the server spawns `cargo` it resolves the binary in this priority order:

1. **`CARGO` environment variable** — if set and points to an existing file.
2. **Rustup proxy** at `$CARGO_HOME/bin/cargo[.exe]` (default
   `~/.cargo/bin/cargo[.exe]`). When found alongside a sibling `rustup` binary
   this is the rustup proxy and honours `rust-toolchain.toml` regardless of
   `PATH` ordering.
3. **`PATH` lookup** — fallback to the bare name `cargo`.

The resolved path is logged to the server's stderr (visible in
**Output → MCP Logs: cargo**) before each invocation. Run the
`cargo_diagnostic` tool for a structured one-shot report.

If your project uses a `rust-toolchain.toml`, installing
[`rustup`](https://rustup.rs/) is **strongly recommended** — without it, the
toolchain file has no effect on any cargo invocation (a property of cargo
itself, not specific to this extension).

---

## Requirements

| Requirement | Notes |
|---|---|
| VS Code | 1.101 or later |
| GitHub Copilot Chat | Agent mode enabled |
| Rust toolchain | stable — `cargo` on `PATH` |
| `rustup` | optional but recommended (see above) |
| `cargo clippy` | `rustup component add clippy` |
| `cargo fmt` | `rustup component add rustfmt` |

---

## Settings

| Setting | Description |
|---|---|
| `cargo-mcp.binaryPath` | Override the path to the `cargo-mcp` binary. Leave blank to use the bundled one. Intended for development against a locally-built server. |
| `cargo-mcp.elicitationMode` | How to handle machine-applicable fix suggestions: `prompt` (default), `always-accept`, or `always-skip`. |

---

## Commands

- **cargo-mcp: Open Copilot setup chat** — opens Copilot Chat with the setup
  prompt pre-filled.
- **cargo-mcp: Copy bundled server binary path** — copies the path of the
  bundled `cargo-mcp` binary to the clipboard.
- **cargo-mcp: Show bundled server version** — displays the bundled server
  version.

---

## Links

- [Source code & full documentation](https://github.com/MikeGrier/cargo-mcp-rs)
- [Issue tracker](https://github.com/MikeGrier/cargo-mcp-rs/issues)
- [Releases (VSIX downloads)](https://github.com/MikeGrier/cargo-mcp-rs/releases)

## License

MIT
