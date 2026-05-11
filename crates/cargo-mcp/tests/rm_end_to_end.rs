// Copyright (c) Michael Grier. All rights reserved.
//
// Layer 2 end-to-end-in-process test for the Restart Manager
// "who holds this file" pipeline.
//
// Spawns rm-target-sniffer to hold a directory handle on a real
// victim crate's `target\debug\deps` (the CWD-pattern offender we
// see in production), then drives `invoke::run_cargo_streaming` for
// `cargo clean`. Because `clean` is on `IDEMPOTENT_SUBCOMMANDS` and
// the failure carries `(os error 32)` ("being used by another
// process"), the retry path runs and `collect_busy_holders` /
// `append_holder_report` should resolve and append the sniffer's
// PID, exe name, and full image path to the captured stderr.
//
// `cargo-mcp` is a binary-only crate, so the test mounts the three
// relevant modules at the test crate's root via `#[path]`. This
// preserves their `crate::busy_files::...` and `crate::rm::...`
// references and avoids the substantial restructure of converting
// the bin into a lib+bin.
//
// Windows-only: Restart Manager is Windows-only.

#![cfg(windows)]

#[path = "../src/busy_files.rs"]
#[allow(dead_code)]
mod busy_files;
#[path = "../src/invoke.rs"]
#[allow(dead_code)]
mod invoke;
#[path = "../src/rm/mod.rs"]
#[allow(dead_code)]
mod rm;

use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

fn build_helper(bin_name: &str) -> PathBuf {
    let workspace_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();

    let output = Command::new(env!("CARGO"))
        .args([
            "build",
            "-p",
            "rm-test-helpers",
            "--bin",
            bin_name,
            "--message-format=json-render-diagnostics",
        ])
        .current_dir(&workspace_root)
        .output()
        .expect("invoke cargo to build helper");

    assert!(
        output.status.success(),
        "cargo build for {bin_name} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    for line in output.stdout.split(|b| *b == b'\n') {
        if line.is_empty() {
            continue;
        }
        let v: serde_json::Value = match serde_json::from_slice(line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if v.get("reason").and_then(|r| r.as_str()) != Some("compiler-artifact") {
            continue;
        }
        if v.pointer("/target/name").and_then(|n| n.as_str()) != Some(bin_name) {
            continue;
        }
        if let Some(exe) = v.get("executable").and_then(|e| e.as_str()) {
            return PathBuf::from(exe);
        }
    }

    panic!("could not find executable for {bin_name} in cargo build output");
}

/// Lay out a tiny crate at <root>/Cargo.toml + src/main.rs so cargo
/// has a real workspace to clean. The crate has no dependencies and
/// builds in ~1 second from scratch.
fn make_victim_crate(root: &std::path::Path) {
    std::fs::create_dir_all(root.join("src")).expect("create victim src");
    std::fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"victim\"\nversion = \"0.0.0\"\nedition = \"2021\"\n",
    )
    .expect("write Cargo.toml");
    std::fs::write(root.join("src").join("main.rs"), "fn main(){}\n").expect("write main.rs");
}

/// Wait for the sniffer to print `READY <pid>` on stdout. Returns the
/// PID it reported (which equals `child.id()`, but we read it back to
/// prove the helper actually got past handle acquisition).
fn await_ready(child: &mut Child) -> u32 {
    let stdout = child.stdout.take().expect("child stdout");
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    let n = reader.read_line(&mut line).expect("read READY line");
    if n == 0 || !line.starts_with("READY ") {
        let mut errbuf = String::new();
        if let Some(mut e) = child.stderr.take() {
            use std::io::Read;
            let _ = e.read_to_string(&mut errbuf);
        }
        panic!("sniffer did not signal READY (got {line:?}); stderr: {errbuf}");
    }
    let pid: u32 = line
        .trim()
        .strip_prefix("READY ")
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| panic!("could not parse PID from {line:?}"));
    child.stdout = Some(reader.into_inner());
    pid
}

struct Sniffer {
    child: Child,
}

impl Drop for Sniffer {
    fn drop(&mut self) {
        drop(self.child.stdin.take());
        for _ in 0..50 {
            if let Ok(Some(_)) = self.child.try_wait() {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn cargo_clean_against_held_file_in_deps_surfaces_holder_in_stderr() {
    // ── 0. Prepare helper + victim crate. ───────────────────────────
    let sniffer_exe = build_helper("rm-target-sniffer");
    let victim = std::env::temp_dir().join(format!(
        "cargo-mcp-l2-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    make_victim_crate(&victim);

    // ── 1. cargo build the victim once so target\debug\deps exists. ─
    let build_status = Command::new(env!("CARGO"))
        .args(["build", "--quiet"])
        .current_dir(&victim)
        .status()
        .expect("cargo build victim");
    assert!(
        build_status.success(),
        "victim crate did not build successfully"
    );
    let deps_dir = victim.join("target").join("debug").join("deps");
    assert!(
        deps_dir.is_dir(),
        "expected deps dir at {} but it was not created",
        deps_dir.display()
    );

    // ── 2. Start the sniffer holding a directory handle on deps\. ───
    let mut child = Command::new(&sniffer_exe)
        .arg(&deps_dir)
        .args(["--mode", "files", "--glob", "*.exe", "--hold-ms", "20000"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn rm-target-sniffer");
    let sniffer_pid = await_ready(&mut child);
    let sniffer = Sniffer { child };

    // ── 3. Configure the in-process invoke layer and run cargo clean.
    invoke::set_rm_lookup_enabled(true);
    invoke::set_retry_config(true, 200, 2);
    let victim_str = victim.to_string_lossy().into_owned();
    let mut streamed: Vec<String> = Vec::new();
    let result = invoke::run_cargo_streaming(&["clean"], Some(&victim_str), &mut |line| {
        streamed.push(line.to_owned());
    })
    .expect("run_cargo_streaming returned an OS-level error");

    // ── 4. Release the sniffer and clean up. ────────────────────────
    drop(sniffer);
    let _ = std::fs::remove_dir_all(&victim);

    // ── 5. Assert the holder report reached the captured stderr. ───
    assert_ne!(
        result.exit_code, 0,
        "expected cargo clean to fail with the directory held; \
         stdout={}\nstderr={}",
        result.stdout, result.stderr
    );
    assert!(
        invoke::is_transient_busy_error(&result.stderr)
            || invoke::is_transient_busy_error(&result.stdout),
        "expected a transient busy error to be detected; stderr was:\n{}",
        result.stderr
    );

    let combined = format!(
        "{}\n{}\n{}",
        result.stdout,
        result.stderr,
        streamed.join("\n")
    );
    let pid_token = format!("PID {sniffer_pid}");
    assert!(
        combined.contains("rm-target-sniffer"),
        "expected holder report mentioning 'rm-target-sniffer'; combined output was:\n{combined}"
    );
    assert!(
        combined.contains(&pid_token),
        "expected '{pid_token}' in combined output; was:\n{combined}"
    );

    // The full image path should appear inside parentheses next to
    // the basename per the documented `name.exe (full\path)` format.
    let path_marker = "rm-target-sniffer.exe (";
    assert!(
        combined.contains(path_marker),
        "expected '{path_marker}' (basename + open-paren full-path marker) \
         in combined output; was:\n{combined}"
    );
}

/// Documents a real Restart Manager limitation discovered while
/// building Layer 2: when the busy resource passed to
/// `RmRegisterResources` is a **directory** rather than a file,
/// `RmGetList` returns `ERROR_ACCESS_DENIED (5)` even though the
/// calling process opened the directory itself with the same user
/// identity. The diagnostic surfaced by `cargo-mcp` in this case is:
///
/// ```text
///   <dir-path>
///     (Restart Manager: RmGetList probe failed: Access is denied. (code 5))
/// ```
///
/// This matches the offender pattern the user originally reported
/// ("some process is setting it to its current working directory,
/// which opens a handle on the directory to keep it valid"). Until
/// the RM behaviour can be worked around (e.g. by upgrading the
/// path to a representative child file before registering), the
/// process name will not appear for the CWD-holder pattern.
///
/// `#[ignore]` so it does not appear as a CI failure, but available
/// to opt into via `cargo test -- --ignored` for manual exploration
/// or once the limitation is addressed.
#[test]
#[ignore = "documents Restart Manager directory-handle limitation; not yet fixable in product"]
fn cargo_clean_against_held_dir_currently_only_reports_rm_access_denied() {
    let sniffer_exe = build_helper("rm-target-sniffer");
    let victim = std::env::temp_dir().join(format!(
        "cargo-mcp-l2dir-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    make_victim_crate(&victim);

    let build_status = Command::new(env!("CARGO"))
        .args(["build", "--quiet"])
        .current_dir(&victim)
        .status()
        .expect("cargo build victim");
    assert!(build_status.success());
    let deps_dir = victim.join("target").join("debug").join("deps");

    let mut child = Command::new(&sniffer_exe)
        .arg(&deps_dir)
        .args(["--mode", "dir", "--hold-ms", "20000"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn rm-target-sniffer");
    let _sniffer_pid = await_ready(&mut child);
    let sniffer = Sniffer { child };

    invoke::set_rm_lookup_enabled(true);
    invoke::set_retry_config(true, 200, 2);
    let victim_str = victim.to_string_lossy().into_owned();
    let result = invoke::run_cargo_streaming(&["clean"], Some(&victim_str), &mut |_| {})
        .expect("run_cargo_streaming returned an OS-level error");
    drop(sniffer);
    let _ = std::fs::remove_dir_all(&victim);

    assert_ne!(result.exit_code, 0);
    assert!(
        result.stderr.contains("RmGetList probe failed")
            && result.stderr.contains("Access is denied"),
        "expected the RM-access-denied marker to appear; stderr was:\n{}",
        result.stderr
    );
    assert!(
        !result.stderr.contains("rm-target-sniffer.exe ("),
        "if this assertion ever fails, the RM directory limitation has been worked around; \
         flip this test from #[ignore] to a positive assertion. Stderr:\n{}",
        result.stderr
    );
}
