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
- [ ] Capture one real "Access is denied" / "os error 5" /
  "os error 32" stderr block from an actual failed build and paste it
  verbatim into a Layer 1 fixture (see below).

### Sniffer redesign (`crates/rm-test-helpers/src/target_sniffer.rs`)

Replace the scan-and-poke loop with grab-and-squat, mimicking the real
offenders (AV, indexers, anything with the dir as CWD).

- [ ] Args: `<dir> [--hold-ms N] [--mode files|dir|both] [--glob *.rlib]`
  (default mode=`both`, hold-ms=`30000`, glob=`*.rlib`).
- [ ] On startup: walk once, open the requested handles, then print
  `READY <pid>` (single line) and flush before sleeping.
- [ ] `mode=dir`: open the directory itself with
  `CreateFileW(GENERIC_READ, FILE_SHARE_READ, OPEN_EXISTING,
  FILE_FLAG_BACKUP_SEMANTICS)` — the CWD pattern. Children remain
  read/write/createable; only directory delete/rename fails.
- [ ] `mode=files`: open every glob match with `FILE_SHARE_READ` only
  (deny write + delete) and hold.
- [ ] Hold until stdin EOF or `--hold-ms` elapses, then close every
  handle and exit 0.
- [ ] Update existing helper unit/integration tests for the new arg shape.

### Layer 1 — Unit tests (no subprocess)

In `crates/cargo-mcp/src/busy_files.rs`:

- [ ] Add a fixture that is the real captured directory-deletion error
  and assert `extract_busy_paths` returns the directory path.
- [ ] Add a fixture for a real `link.exe LNK1104` / `os error 32`
  mid-build line and assert the locked artefact is extracted.
- [ ] If either fails, extend `line_is_busy_indicator` /
  `harvest_*_paths` until both pass. **Do not** loosen the
  diagnostic-block heuristic — extend the busy-indicator phrase list
  instead.

### Layer 2 — In-process integration

New windows-only test alongside `tests/rm_who_holds.rs`:

- [ ] Generate a tiny throwaway crate at
  `%TEMP%\cargo-mcp-it-<pid>\` (write `Cargo.toml` + `src/main.rs` by
  hand; no need to invoke `cargo new`).
- [ ] `cargo build` it once so `target/debug/deps/` exists.
- [ ] Spawn `rm-target-sniffer <victim>\target\debug\deps --mode dir
  --hold-ms 10000`, await `READY <pid>`.
- [ ] Call `set_rm_lookup_enabled(true)` and
  `set_retry_config(true, 200, 3)`, then run `cargo_clean` (or a forced
  `cargo_build` after `touch`) on the victim crate via
  `run_cargo_streaming`.
- [ ] Assert the captured streamed lines OR `CargoOutput.stderr`
  contains `rm-target-sniffer.exe (` and `(PID <sniffer_pid>)`.

### Layer 3 — Subprocess end-to-end

Marked `#[ignore]` (run on demand) — proves what the agent actually sees.

- [ ] Spawn the built `cargo-mcp.exe` as a child with
  `--unsafe-windows-rm=true`.
- [ ] Hand-roll an NDJSON / Content-Length framer (~80 LOC). Send
  `initialize`, then `tools/call` for `cargo_build` against the victim
  crate while the sniffer is squatting on `deps/`.
- [ ] Read the JSON-RPC response and the `notifications/message`
  progress frames.
- [ ] Assert `result.content[0].text` contains the holder block and at
  least one progress notification carries the short summary line
  (`name.exe (...) (PID N)`).

### Wrap-up

- [ ] Build, test, clippy, fmt across the workspace.
- [ ] Encoding check on every touched file.
- [ ] Commit on `micgrier/safe-rm-wrapper`. Subject:
  `test(rm): redesigned sniffer, layered tests for end-to-end RM visibility`.
- [ ] Push.
