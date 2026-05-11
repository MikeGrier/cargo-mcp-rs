// Copyright (c) Michael Grier. All rights reserved.
//
// Layer 3 end-to-end test: drives the built `cargo-mcp.exe` as a real
// MCP subprocess over stdio JSON-RPC and verifies the Restart Manager
// holder report reaches the user inside `result.content[0].text` —
// the exact bytes an agent sees through the MCP transport.
//
// This is the most expensive test in the suite (spawns three child
// processes, builds two helper binaries, builds a throwaway victim
// crate, and waits for retry attempts) and is gated behind
// `#[ignore]` so it runs only on demand:
//
//     cargo test -p cargo-mcp --test rm_subprocess_e2e -- --ignored --nocapture
//
// Layers 1 and 2 cover unit and in-process behaviour at much lower
// cost; Layer 3 exists to catch transport-level regressions (e.g. the
// JSON-mode formatter previously dropping `CargoOutput.stderr` and
// hiding the holder report from agents even though `invoke` produced
// it correctly).

#![cfg(windows)]

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

/// Build a workspace binary via `cargo build --message-format=json` and
/// return the executable path the compiler reported.
fn build_bin(package: &str, bin: &str) -> PathBuf {
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
            package,
            "--bin",
            bin,
            "--message-format=json-render-diagnostics",
        ])
        .current_dir(&workspace_root)
        .output()
        .expect("invoke cargo to build helper");

    assert!(
        output.status.success(),
        "cargo build for {package}::{bin} failed: {}",
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
        if v.pointer("/target/name").and_then(|n| n.as_str()) != Some(bin) {
            continue;
        }
        if let Some(exe) = v.get("executable").and_then(|e| e.as_str()) {
            return PathBuf::from(exe);
        }
    }
    panic!("could not find executable for {package}::{bin} in cargo build output");
}

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
/// PID it reported.
fn await_sniffer_ready(child: &mut Child) -> u32 {
    let stdout = child.stdout.take().expect("sniffer stdout");
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    let n = reader.read_line(&mut line).expect("read READY line");
    assert!(
        n > 0 && line.starts_with("READY "),
        "sniffer did not signal READY (got {line:?})"
    );
    let pid: u32 = line
        .trim()
        .strip_prefix("READY ")
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| panic!("could not parse PID from {line:?}"));
    child.stdout = Some(reader.into_inner());
    pid
}

struct ChildGuard(Child);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        drop(self.0.stdin.take());
        for _ in 0..50 {
            if let Ok(Some(_)) = self.0.try_wait() {
                return;
            }
            thread::sleep(Duration::from_millis(20));
        }
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Spawn a background thread that reads NDJSON lines from `child`'s
/// stdout and pushes each parsed `serde_json::Value` onto a channel.
/// Returns the receiver and a handle to the join (caller owns drop).
fn spawn_stdout_reader(child: &mut Child) -> mpsc::Receiver<serde_json::Value> {
    let (tx, rx) = mpsc::channel();
    let stdout = child.stdout.take().expect("server stdout");
    thread::spawn(move || {
        let reader = BufReader::new(stdout);
        for line in reader.lines() {
            let Ok(line) = line else { return };
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<serde_json::Value>(&line) {
                Ok(v) => {
                    if tx.send(v).is_err() {
                        return;
                    }
                }
                Err(_) => {
                    // Non-JSON line (shouldn't happen — server speaks NDJSON
                    // exclusively — but log to test stderr for diagnosis).
                    eprintln!("non-JSON line from server: {line}");
                }
            }
        }
    });
    rx
}

/// Send one JSON-RPC frame (NDJSON: serialised value + newline) to the
/// server's stdin.
fn send_frame(child: &mut Child, value: &serde_json::Value) {
    let stdin = child.stdin.as_mut().expect("server stdin");
    let mut s = serde_json::to_string(value).expect("serialise frame");
    s.push('\n');
    stdin.write_all(s.as_bytes()).expect("write frame");
    stdin.flush().expect("flush frame");
}

/// Drain the channel until a message with `"id" == request_id` arrives,
/// or `timeout` elapses. Returns the matching response and a Vec of all
/// notifications observed in the meantime.
fn await_response(
    rx: &mpsc::Receiver<serde_json::Value>,
    request_id: u64,
    timeout: Duration,
) -> (serde_json::Value, Vec<serde_json::Value>) {
    let deadline = Instant::now() + timeout;
    let mut notes = Vec::new();
    loop {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .unwrap_or_default();
        let v = rx
            .recv_timeout(remaining.max(Duration::from_millis(1)))
            .unwrap_or_else(|_| {
                panic!(
                    "timed out after {:?} waiting for response id {request_id}; \
                     notifications seen so far: {}",
                    timeout,
                    notes.len()
                )
            });
        if v.get("id").and_then(|i| i.as_u64()) == Some(request_id) {
            return (v, notes);
        }
        notes.push(v);
    }
}

#[test]
#[ignore = "expensive: spawns cargo-mcp.exe as a child and exercises the full MCP transport"]
fn cargo_clean_holder_report_reaches_agent_through_mcp_transport() {
    // ── 0. Build helpers + cargo-mcp itself. ────────────────────────
    let server_exe = build_bin("cargo-mcp", "cargo-mcp");
    let sniffer_exe = build_bin("rm-test-helpers", "rm-target-sniffer");

    // ── 1. Make and populate a throwaway victim crate. ──────────────
    let victim = std::env::temp_dir().join(format!(
        "cargo-mcp-l3-{}-{}",
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

    // ── 2. Spawn the sniffer holding *.exe in deps\. ────────────────
    let mut sniffer_child = Command::new(&sniffer_exe)
        .arg(&deps_dir)
        .args(["--mode", "files", "--glob", "*.exe", "--hold-ms", "30000"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn rm-target-sniffer");
    let sniffer_pid = await_sniffer_ready(&mut sniffer_child);
    let _sniffer_guard = ChildGuard(sniffer_child);

    // ── 3. Spawn cargo-mcp.exe with RM lookup enabled. ──────────────
    let mut server = Command::new(&server_exe)
        .arg("--unsafe-windows-rm=true")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn cargo-mcp");
    let rx = spawn_stdout_reader(&mut server);

    // ── 4. Drive the JSON-RPC handshake. ────────────────────────────
    send_frame(
        &mut server,
        &serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": { "name": "rm-l3-test", "version": "0" }
            }
        }),
    );
    let (init_resp, _init_notes) = await_response(&rx, 1, Duration::from_secs(10));
    assert!(
        init_resp.get("result").is_some(),
        "initialize did not produce a result: {init_resp}"
    );
    send_frame(
        &mut server,
        &serde_json::json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized"
        }),
    );

    // ── 5. Call cargo_clean against the held victim crate. ──────────
    let victim_str = victim.to_string_lossy().into_owned();
    send_frame(
        &mut server,
        &serde_json::json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {
                "name": "cargo_clean",
                "arguments": { "working_dir": victim_str }
            }
        }),
    );
    let (call_resp, notes) = await_response(&rx, 2, Duration::from_secs(60));

    // ── 6. Cleanly shut the server down before asserting. ───────────
    send_frame(
        &mut server,
        &serde_json::json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "shutdown",
            "params": {}
        }),
    );
    let _ = await_response(&rx, 3, Duration::from_secs(5));
    drop(server.stdin.take());
    let _ = server.wait();
    drop(_sniffer_guard);
    let _ = std::fs::remove_dir_all(&victim);

    // ── 7. Assertions on the agent-visible payload. ─────────────────
    let text = call_resp
        .pointer("/result/content/0/text")
        .and_then(|t| t.as_str())
        .unwrap_or_else(|| panic!("missing /result/content/0/text in: {call_resp}"));

    let pid_token = format!("PID {sniffer_pid}");
    assert!(
        text.contains("rm-target-sniffer.exe ("),
        "expected agent-visible text to contain 'rm-target-sniffer.exe ('; \
         full text:\n{text}\n--- notifications: {} ---",
        notes.len()
    );
    assert!(
        text.contains(&pid_token),
        "expected agent-visible text to contain '{pid_token}'; full text:\n{text}"
    );

    // Note: cargo_clean uses the non-streaming `run_cargo` path, so no
    // `notifications/message` frames are expected from the retry loop.
    // Streaming tools (cargo_build / cargo_check / cargo_test /
    // cargo_clippy) would emit them via `log_info`; cover that channel
    // separately when adding a streaming-tool E2E.
    let _ = notes;
}
