// Copyright (c) Michael Grier. All rights reserved.
//
// Test helper: poll a directory tree and briefly open every file in it
// with a deny-write / deny-delete share mode, intentionally inducing
// transient ERROR_SHARING_VIOLATION (os error 32) and ERROR_ACCESS_DENIED
// (os error 5) errors in any concurrent process trying to write into
// that tree (e.g. a `cargo build` writing into `target\`).
//
// Used by cargo-mcp's integration tests to drive the retry-on-busy
// path and the Restart Manager "who holds this file" diagnostic
// against a known process name (`rm-target-sniffer.exe`).
//
// Usage:
//     rm-target-sniffer <directory> [--duration-ms <N>]
//
// Behaviour:
//   1. Prints "READY\n" to stdout and flushes.
//   2. For up to <duration-ms> ms (default 30_000), repeatedly walks
//      <directory>, opening every regular file with FILE_SHARE_READ
//      only and closing it again after a brief hold. Errors are
//      ignored (the file may have been deleted between scan and open).
//   3. Exits 0 on the timer or if stdin closes, whichever happens first.
//
// On non-Windows hosts it prints "SKIP not-windows" and exits 0.

#[cfg(windows)]
fn main() {
    use std::io::Write;
    use std::os::windows::ffi::OsStrExt;
    use std::path::{Path, PathBuf};
    use std::ptr;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

    use windows_sys::Win32::Foundation::{CloseHandle, GENERIC_READ, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, OPEN_EXISTING,
    };

    let mut args = std::env::args_os().skip(1);
    let dir = match args.next() {
        Some(d) => PathBuf::from(d),
        None => {
            eprintln!("rm-target-sniffer: missing <directory> argument");
            std::process::exit(2);
        }
    };
    let mut duration_ms: u64 = 30_000;
    while let Some(a) = args.next() {
        let s = a.to_string_lossy().to_string();
        if s == "--duration-ms"
            && let Some(v) = args.next()
            && let Ok(n) = v.to_string_lossy().parse::<u64>()
        {
            duration_ms = n;
        }
    }
    let deadline = Instant::now() + Duration::from_millis(duration_ms);

    // Print readiness so the parent test can synchronise.
    {
        let stdout = std::io::stdout();
        let mut out = stdout.lock();
        let _ = writeln!(out, "READY");
        let _ = out.flush();
    }

    // Watch stdin in a background thread; on EOF flip a flag so the
    // main loop exits promptly without waiting for the deadline.
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

    fn walk(dir: &Path, out: &mut Vec<PathBuf>, depth: u32) {
        if depth > 16 {
            return;
        }
        let Ok(rd) = std::fs::read_dir(dir) else {
            return;
        };
        for ent in rd.flatten() {
            let p = ent.path();
            match ent.file_type() {
                Ok(ft) if ft.is_file() => out.push(p),
                Ok(ft) if ft.is_dir() => walk(&p, out, depth + 1),
                _ => {}
            }
        }
    }

    fn try_lock(path: &Path) {
        let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
        wide.push(0);
        // SAFETY: NUL-terminated wide string; standard CreateFileW form.
        let h = unsafe {
            CreateFileW(
                wide.as_ptr(),
                GENERIC_READ,
                FILE_SHARE_READ,
                ptr::null_mut(),
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL,
                ptr::null_mut(),
            )
        };
        if h == INVALID_HANDLE_VALUE {
            return;
        }
        // Hold briefly, then release. Long enough that a concurrent
        // writer is overwhelmingly likely to collide; short enough that
        // we don't deadlock the test forever.
        std::thread::sleep(Duration::from_millis(50));
        // SAFETY: `h` was returned by the matching CreateFileW above.
        unsafe { CloseHandle(h) };
    }

    while Instant::now() < deadline && !stop.load(Ordering::Relaxed) {
        let mut files = Vec::new();
        walk(&dir, &mut files, 0);
        for p in files {
            if stop.load(Ordering::Relaxed) {
                break;
            }
            try_lock(&p);
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

#[cfg(not(windows))]
fn main() {
    println!("SKIP not-windows");
}
