// Copyright (c) Michael Grier. All rights reserved.
//
// Test helper: cross-platform sleeper used as a fake `cargo` shim by
// cargo-mcp's `invoke_timeout` integration test. Verifies that the
// timeout / cancellation paths in `invoke::run_cargo_streaming_with_timeout`
// actually terminate the spawned subprocess (and, by virtue of the
// Job Object / process group plumbing in `ManagedChild`, its descendants).
//
// Behaviour:
//   * Prints `STARTED <pid>\n` to stdout and flushes, so the parent can
//     observe that the subprocess made it past spawn.
//   * Sleeps for 600 seconds. Any extra command-line args (e.g. the
//     cargo subcommand "check") are ignored — this binary stands in for
//     `cargo` and must accept whatever args the caller passes.
//   * Exits 0 if the sleep completes; in practice the parent kills it
//     well before then.

use std::io::Write;
use std::thread;
use std::time::Duration;

fn main() {
    let pid = std::process::id();
    {
        let stdout = std::io::stdout();
        let mut guard = stdout.lock();
        let _ = writeln!(guard, "STARTED {pid}");
        let _ = guard.flush();
    }
    thread::sleep(Duration::from_secs(600));
}
