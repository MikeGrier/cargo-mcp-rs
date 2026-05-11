// Copyright (c) Michael Grier. All rights reserved.
//
// Integration tests for the safe Restart Manager wrapper, exercised
// against the `rm-hold-file` test helper from the sibling
// `rm-test-helpers` crate so we have a known process holding a known
// file. Windows-only because Restart Manager is Windows-only.
//
// `cargo-mcp` is a binary-only crate, so we can't `use cargo_mcp::rm`
// from an integration test. The `rm` module is deliberately self-
// contained (no back-references into the rest of the crate), so we
// include it via `#[path]` instead.

#![cfg(windows)]

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

struct Holder {
    child: Child,
}

impl Holder {
    fn spawn(helper: &std::path::Path, target: &std::path::Path) -> Holder {
        let mut child = Command::new(helper)
            .arg(target)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn rm-hold-file");

        let stdout = child.stdout.take().expect("child stdout");
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        let n = reader.read_line(&mut line).expect("read READY line");
        if n == 0 || line.trim() != "READY" {
            let mut errbuf = String::new();
            if let Some(mut e) = child.stderr.take() {
                use std::io::Read;
                let _ = e.read_to_string(&mut errbuf);
            }
            panic!("rm-hold-file did not signal READY (got {line:?}); stderr: {errbuf}");
        }
        child.stdout = Some(reader.into_inner());
        Holder { child }
    }
}

impl Drop for Holder {
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
fn rm_who_holds_identifies_helper_with_full_image_path() {
    let helper = build_helper("rm-hold-file");
    assert!(helper.exists(), "helper missing: {}", helper.display());

    let tmp = std::env::temp_dir().join(format!(
        "cargo-mcp-rm-int-{}-{}.tmp",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::write(&tmp, b"hello").expect("write temp file");

    let holder = Holder::spawn(&helper, &tmp);
    let helper_pid = holder.child.id();

    let result = rm::who_holds(&[tmp.as_path()]);

    drop(holder);
    let _ = std::fs::remove_file(&tmp);

    assert_eq!(result.len(), 1);
    let entry = &result[0];
    assert!(
        entry.error.is_none(),
        "RM returned error: {:?}",
        entry.error
    );

    let matched = entry
        .holders
        .iter()
        .find(|h| h.pid == helper_pid)
        .unwrap_or_else(|| {
            panic!(
                "RM did not report PID {helper_pid}; holders={:?}",
                entry.holders
            )
        });

    let name = matched.app_name.to_ascii_lowercase();
    assert!(
        name.contains("rm-hold-file"),
        "RM app_name {name:?} does not contain 'rm-hold-file'"
    );

    let app_path = matched
        .app_path
        .as_ref()
        .expect("app_path should resolve for our own child process");
    let app_path_str = app_path.to_string_lossy().to_ascii_lowercase();
    assert!(
        app_path_str.ends_with("rm-hold-file.exe"),
        "app_path {app_path_str:?} does not end with rm-hold-file.exe"
    );
    assert!(
        app_path.exists(),
        "resolved app_path does not exist: {}",
        app_path.display()
    );
}

#[test]
fn rm_who_holds_returns_empty_for_unheld_temp_file() {
    let tmp = std::env::temp_dir().join(format!(
        "cargo-mcp-rm-int-empty-{}-{}.tmp",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::write(&tmp, b"hi").unwrap();
    let result = rm::who_holds(&[tmp.as_path()]);
    let _ = std::fs::remove_file(&tmp);

    assert_eq!(result.len(), 1);
    if result[0].error.is_none() {
        assert!(
            result[0].holders.is_empty(),
            "unexpected holders for unheld temp file: {:?}",
            result[0].holders
        );
    }
}
