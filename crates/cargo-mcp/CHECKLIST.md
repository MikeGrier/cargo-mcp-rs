# cargo-mcp CHECKLIST

## Restart Manager testing & end-to-end visibility

Goal: prove that when a real cargo build fails because some other process
holds an artefact (file or directory), the user sees the offending
process name and full image path in the cargo-mcp output. Today we have
the Win32 lookup wired up but no observed evidence it ever surfaces.

### Diagnostics first (cheap, high signal)

- [x] Confirm the VS Code extension forwards
  `cargo-mcp.unsafe.windowsRestartManager` into the spawned server's
  argv as `--unsafe-windows-rm=true`. Verified in
  `crates/cargo-mcp/extension/src/extension.ts` (and built `out/extension.js`).
- [x] **ROOT CAUSE FOUND: holder report is dropped by the
  JSON-mode tool formatter.** `crates/cargo-mcp/src/invoke.rs`
  appends the holder report to `CargoOutput.stderr`, but
  `format_json_output` in `crates/cargo-mcp/src/tools.rs` (the path
  used for `cargo build` / `check` / `test` / `clippy`) only
  surfaces stderr in the empty-stdout error branch. When cargo
  failed and produced any NDJSON (the normal case for a real build
  failure), the JSON-mode formatter emits filtered NDJSON + a
  status trailer and **drops stderr entirely**. Net effect: the
  holder report makes it as far as `out.stderr` and then vanishes.

### Fix the formatter so the holder report reaches the user

- [x] In `crates/cargo-mcp/src/tools.rs::format_json_output`,
  always include any non-empty stderr text on failure (both the
  empty-stdout branch and the JSON-stdout branch). Format proposal:
  append a literal `\n[stderr]\n<trimmed stderr>\n` block after the
  status trailer so the NDJSON stream itself stays valid for
  downstream parsers but the user (and the agent) sees the holder
  report.
- [x] Add a unit test in `tools.rs` that constructs a synthetic
  `CargoOutput { exit_code: 1, stdout: "<one valid compiler-message JSON line>", stderr: "...holder report fixture...\n" }`
  and asserts the formatted string contains the holder report text.
- [ ] Capture one real "Access is denied" / "os error 5" /
  "os error 32" stderr block from an actual failed build and paste
  it verbatim into a Layer 1 fixture (see below). With the
  formatter fix, this should now be observable end-to-end.

### Sniffer redesign (`crates/rm-test-helpers/src/target_sniffer.rs`)

Replace the scan-and-poke loop with grab-and-squat, mimicking the real
offenders (AV, indexers, anything with the dir as CWD).

- [x] Args: `<dir> [--hold-ms N] [--mode files|dir|both] [--glob *.rlib]`
  (default mode=`both`, hold-ms=`30000`, glob=`*.rlib`).
- [x] On startup: walk once, open the requested handles, then print
  `READY <pid>` (single line) and flush before sleeping.
- [x] `mode=dir`: open the directory itself with
  `CreateFileW(GENERIC_READ, FILE_SHARE_READ, OPEN_EXISTING,
  FILE_FLAG_BACKUP_SEMANTICS)` — the CWD pattern. Children remain
  read/write/createable; only directory delete/rename fails.
- [x] `mode=files`: open every glob match with `FILE_SHARE_READ` only
  (deny write + delete) and hold.
- [x] Hold until stdin EOF or `--hold-ms` elapses, then close every
  handle and exit 0.
- [x] No prior helper tests existed for the old shape; smoke-tested
  manually that `mode=dir` blocks `Remove-Item` and `mode=files
  --glob *.exe` blocks `cargo clean`.

### Layer 1 — Unit tests (no subprocess)

In `crates/cargo-mcp/src/busy_files.rs`:

- [x] Add a fixture that is the real captured directory-deletion error
  (`error: failed to remove directory \`...\`` followed by indented
  `Caused by: ... (os error 32)`) and assert `extract_busy_paths`
  returns the directory path. (See
  `extracts_directory_path_from_real_cargo_clean_error_block`.)
- [ ] _(Deferred)_ LNK1104 fixture — not encountered in Layer 2
  scenarios; capture next time we hit a linker conflict in the field.
- [x] Existing extractor heuristics already covered the captured
  shape; no loosening required.

### Layer 2 — In-process integration

New windows-only test in `crates/cargo-mcp/tests/rm_end_to_end.rs`:

- [x] Generate a tiny throwaway crate at
  `%TEMP%\cargo-mcp-l2-<pid>-<nanos>\` (write `Cargo.toml` +
  `src/main.rs` by hand).
- [x] `cargo build --quiet` it once so `target/debug/deps/` exists.
- [x] Spawn `rm-target-sniffer <victim>\target\debug\deps --mode files
  --glob *.exe --hold-ms 20000`, await `READY <pid>`.
- [x] Call `set_rm_lookup_enabled(true)` and
  `set_retry_config(true, 200, 2)`, then run `run_cargo_streaming(&["clean"], ...)`.
- [x] Assert combined output contains `rm-target-sniffer.exe (` and
  `PID <sniffer_pid>`.
- [x] **Real product limitation discovered:** when the busy resource
  is a *directory*, `RmGetList` returns `ERROR_ACCESS_DENIED (5)` even
  for same-user processes. The diagnostic surfaces as `(Restart
  Manager: RmGetList probe failed: Access is denied. (code 5))` with
  no process name. Documented as `#[ignore]`'d test
  `cargo_clean_against_held_dir_currently_only_reports_rm_access_denied`
  so a future workaround (e.g. promoting the dir to a representative
  child file before registering) can be detected by flipping the
  ignore.

### Layer 3 — Subprocess end-to-end

Marked `#[ignore]` (run on demand) — proves what the agent actually sees.

- [x] `tests/rm_subprocess_e2e.rs::cargo_clean_holder_report_reaches_agent_through_mcp_transport`
  spawns the built `cargo-mcp.exe` as a child, drives the
  newline-delimited JSON-RPC handshake (`initialize` →
  `notifications/initialized` → `tools/call cargo_clean` → `shutdown`),
  and asserts that `result.content[0].text` from the `cargo_clean`
  response contains both `rm-target-sniffer.exe (` and
  `PID <sniffer_pid>`. This catches transport-level regressions where
  the holder report would be produced by `invoke` but lost between
  `format_text_output` / `format_json_output` and the JSON-RPC frame.
- [x] Run on demand:
  `cargo test -p cargo-mcp --test rm_subprocess_e2e -- --ignored --nocapture`.


### Wrap-up

- [ ] Build, test, clippy, fmt across the workspace.
- [ ] Encoding check on every touched file.
- [ ] Commit on `micgrier/safe-rm-wrapper`. Subject:
  `test(rm): redesigned sniffer, layered tests for end-to-end RM visibility`.
- [ ] Push.
