// Copyright (c) Michael Grier. All rights reserved.
//
// Test helper: open a configurable set of handles inside a directory
// and HOLD them open until either stdin closes or a duration elapses.
// Mimics the real-world offenders that cargo-mcp tries to identify
// (antivirus scanners, file indexers, processes that have set the dir
// as their CWD, etc.) so we can exercise the Restart Manager
// "who holds this file" diagnostic against a known process name
// (`rm-target-sniffer.exe`).
//
// Usage:
//     rm-target-sniffer <directory>
//                       [--mode files|dir|both]    default: both
//                       [--hold-ms N]              default: 30000
//                       [--glob *.rlib]            default: *.rlib (mode=files)
//
// Behaviour:
//   1. Walks <directory> once and opens handles per --mode:
//        - dir:   opens <directory> itself with FILE_FLAG_BACKUP_SEMANTICS,
//                 the same shape as a CWD handle: children remain
//                 createable / writable, but the directory itself
//                 cannot be renamed or removed.
//        - files: opens every file matching --glob with FILE_SHARE_READ
//                 only (deny write, deny delete) and holds.
//        - both:  both of the above.
//   2. Prints "READY <pid>\n" to stdout and flushes.
//   3. Sleeps until stdin closes OR --hold-ms elapses, whichever
//      happens first.
//   4. Closes every handle and exits 0.
//
// Errors during walk / open are logged to stderr but never abort the
// program: the test harness can decide whether enough handles were
// acquired by inspecting the printed READY line.
//
// On non-Windows hosts the program prints "SKIP not-windows" and
// exits 0 so cross-platform CI doesn't fail.

#[cfg(windows)]
fn main() {
    use std::ffi::OsString;
    use std::io::Write;
    use std::os::windows::ffi::OsStrExt;
    use std::path::{Path, PathBuf};
    use std::ptr;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

    use windows_sys::Win32::Foundation::{CloseHandle, GENERIC_READ, HANDLE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_FLAG_BACKUP_SEMANTICS, FILE_SHARE_READ,
        OPEN_EXISTING,
    };

    let mut args = std::env::args_os().skip(1);
    let dir = match args.next() {
        Some(d) => PathBuf::from(d),
        None => {
            eprintln!("rm-target-sniffer: missing <directory> argument");
            std::process::exit(2);
        }
    };
    let mut mode_buf: String = String::from("both");
    let mut hold_ms: u64 = 30_000;
    let mut glob: OsString = OsString::from("*.rlib");
    while let Some(a) = args.next() {
        let s = a.to_string_lossy().to_string();
        match s.as_str() {
            "--mode" => {
                if let Some(v) = args.next() {
                    mode_buf = v.to_string_lossy().to_string();
                }
            }
            "--hold-ms" => {
                if let Some(v) = args.next()
                    && let Ok(n) = v.to_string_lossy().parse::<u64>()
                {
                    hold_ms = n;
                }
            }
            "--glob" => {
                if let Some(v) = args.next() {
                    glob = v;
                }
            }
            other => {
                eprintln!("rm-target-sniffer: ignoring unknown arg {other:?}");
            }
        }
    }
    let mode = mode_buf.as_str();
    let want_dir = matches!(mode, "dir" | "both");
    let want_files = matches!(mode, "files" | "both");
    if !want_dir && !want_files {
        eprintln!("rm-target-sniffer: --mode must be one of files|dir|both (got {mode:?})");
        std::process::exit(2);
    }

    fn wide(p: &Path) -> Vec<u16> {
        let mut v: Vec<u16> = p.as_os_str().encode_wide().collect();
        v.push(0);
        v
    }

    fn open_share_read(p: &Path) -> Option<HANDLE> {
        let w = wide(p);
        // SAFETY: NUL-terminated wide string; standard CreateFileW form.
        let h = unsafe {
            CreateFileW(
                w.as_ptr(),
                GENERIC_READ,
                FILE_SHARE_READ,
                ptr::null_mut(),
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL,
                ptr::null_mut(),
            )
        };
        if h == INVALID_HANDLE_VALUE {
            None
        } else {
            Some(h)
        }
    }

    fn open_directory_handle(p: &Path) -> Option<HANDLE> {
        let w = wide(p);
        // SAFETY: NUL-terminated wide string; FILE_FLAG_BACKUP_SEMANTICS
        // is required for directory handles per CreateFileW docs.
        let h = unsafe {
            CreateFileW(
                w.as_ptr(),
                GENERIC_READ,
                FILE_SHARE_READ,
                ptr::null_mut(),
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS,
                ptr::null_mut(),
            )
        };
        if h == INVALID_HANDLE_VALUE {
            None
        } else {
            Some(h)
        }
    }

    fn glob_match(name: &std::ffi::OsStr, pattern: &std::ffi::OsStr) -> bool {
        let n = name.to_string_lossy();
        let p = pattern.to_string_lossy();
        if let Some(rest) = p.strip_prefix('*') {
            n.ends_with(rest)
        } else {
            n == p
        }
    }

    fn walk_files(dir: &Path, pattern: &std::ffi::OsStr, out: &mut Vec<PathBuf>, depth: u32) {
        if depth > 16 {
            return;
        }
        let Ok(rd) = std::fs::read_dir(dir) else {
            return;
        };
        for ent in rd.flatten() {
            let p = ent.path();
            match ent.file_type() {
                Ok(ft) if ft.is_file() && glob_match(&ent.file_name(), pattern) => {
                    out.push(p);
                }
                Ok(ft) if ft.is_dir() => walk_files(&p, pattern, out, depth + 1),
                _ => {}
            }
        }
    }

    let mut handles: Vec<HANDLE> = Vec::new();

    if want_dir {
        match open_directory_handle(&dir) {
            Some(h) => handles.push(h),
            None => {
                let err = std::io::Error::last_os_error();
                eprintln!(
                    "rm-target-sniffer: failed to open directory handle on {}: {err}",
                    dir.display(),
                );
            }
        }
    }

    let mut file_attempts = 0usize;
    if want_files {
        let mut matches: Vec<PathBuf> = Vec::new();
        walk_files(&dir, &glob, &mut matches, 0);
        file_attempts = matches.len();
        for p in &matches {
            if let Some(h) = open_share_read(p) {
                handles.push(h);
            }
        }
    }

    eprintln!(
        "rm-target-sniffer: dir={} mode={} held_handles={} file_matches={} hold_ms={}",
        dir.display(),
        mode,
        handles.len(),
        file_attempts,
        hold_ms,
    );

    {
        let stdout = std::io::stdout();
        let mut out = stdout.lock();
        let _ = writeln!(out, "READY {}", std::process::id());
        let _ = out.flush();
    }

    let stop = Arc::new(AtomicBool::new(false));
    {
        let stop = Arc::clone(&stop);
        std::thread::spawn(move || {
            use std::io::Read;
            let mut buf = [0u8; 64];
            let stdin = std::io::stdin();
            let mut h = stdin.lock();
            while let Ok(n) = h.read(&mut buf) {
                if n == 0 {
                    break;
                }
            }
            stop.store(true, Ordering::Relaxed);
        });
    }

    let deadline = Instant::now() + Duration::from_millis(hold_ms);
    while Instant::now() < deadline && !stop.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_millis(50));
    }

    for h in handles {
        // SAFETY: each handle was returned by a successful CreateFileW
        // above and has not been closed since.
        unsafe { CloseHandle(h) };
    }
}

#[cfg(not(windows))]
fn main() {
    println!("SKIP not-windows");
}
