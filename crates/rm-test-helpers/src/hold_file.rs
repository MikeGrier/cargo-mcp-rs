// Copyright (c) Michael Grier. All rights reserved.
//
// Test helper: open a file with a deny-write / deny-delete share mode
// and block until stdin closes. Used by cargo-mcp integration tests to
// have a known process holding a known file so the Restart Manager
// wrapper has something concrete to find.
//
// Usage:
//     rm-hold-file <absolute-path>
//
// On startup the helper:
//   1. Opens <path> for read with FILE_SHARE_READ only (so any other
//      process trying to write or delete it will hit ERROR_SHARING_VIOLATION).
//   2. Prints the literal line "READY\n" to stdout and flushes.
//   3. Reads stdin until EOF; closes the file and exits 0.
//
// On non-Windows hosts it prints "SKIP not-windows" and exits 0 so the
// integration test can decide to skip.

#[cfg(windows)]
fn main() {
    use std::io::{BufRead, Write};
    use std::os::windows::ffi::OsStrExt;
    use std::path::PathBuf;
    use std::ptr;

    use windows_sys::Win32::Foundation::{CloseHandle, GENERIC_READ, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, OPEN_EXISTING,
    };

    let path = match std::env::args_os().nth(1) {
        Some(p) => PathBuf::from(p),
        None => {
            eprintln!("rm-hold-file: missing <path> argument");
            std::process::exit(2);
        }
    };

    let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
    wide.push(0);

    // SAFETY: Standard CreateFileW invocation. `wide` is NUL-terminated
    // and outlives the call; we ignore the handle's security descriptor
    // and template.
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            GENERIC_READ,
            FILE_SHARE_READ, // intentionally NOT FILE_SHARE_WRITE | FILE_SHARE_DELETE
            ptr::null_mut(),
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        let err = std::io::Error::last_os_error();
        eprintln!(
            "rm-hold-file: CreateFileW failed for {}: {err}",
            path.display()
        );
        std::process::exit(1);
    }

    // Signal readiness. The integration test waits for this before
    // proceeding so the RM query is guaranteed to see the lock.
    let stdout = std::io::stdout();
    {
        let mut out = stdout.lock();
        let _ = writeln!(out, "READY");
        let _ = out.flush();
    }

    // Block until stdin closes (parent test drops the writer end). We
    // don't care about the contents, just the close signal.
    let stdin = std::io::stdin();
    let mut s = String::new();
    let mut h = stdin.lock();
    while let Ok(n) = h.read_line(&mut s) {
        if n == 0 {
            break;
        }
        s.clear();
    }

    // SAFETY: `handle` came from CreateFileW above and has not been closed.
    unsafe { CloseHandle(handle) };
}

#[cfg(not(windows))]
fn main() {
    println!("SKIP not-windows");
}
