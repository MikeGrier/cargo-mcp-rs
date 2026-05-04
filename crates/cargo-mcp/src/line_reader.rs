// Copyright (c) Michael Grier.

//! Channel-based line reader with timeout support.
//!
//! A background thread reads lines from stdin and sends them through an
//! `mpsc` channel. This lets callers choose between blocking reads (for
//! the main JSON-RPC loop) and bounded-wait reads (for elicitation prompts
//! that should time out instead of hanging the server).

use std::{
    io::{self, BufRead},
    sync::mpsc::{self, Receiver},
    thread,
    time::Duration,
};

/// Channel-based line reader with timeout-bounded reads.
pub struct LineReader {
    rx: Receiver<String>,
}

impl LineReader {
    /// Spawn a background reader thread for the given `stdin` handle.
    pub fn new(stdin: io::Stdin) -> Self {
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let mut locked = stdin.lock();
            let mut line = String::new();
            loop {
                line.clear();
                match locked.read_line(&mut line) {
                    Ok(0) => break,
                    Ok(_) => {
                        if tx.send(line.clone()).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });
        LineReader { rx }
    }

    /// Read the next line, blocking indefinitely. Returns `None` on EOF.
    pub fn read_line(&self) -> Option<String> {
        self.rx.recv().ok()
    }

    /// Read the next line with a timeout. Returns `None` on timeout or EOF.
    pub fn read_line_timeout(&self, timeout: Duration) -> Option<String> {
        self.rx.recv_timeout(timeout).ok()
    }

    /// Create a `LineReader` pre-loaded with the given lines (for testing).
    #[cfg(test)]
    pub fn from_lines(lines: &[&str]) -> Self {
        let (tx, rx) = mpsc::channel();
        for line in lines {
            let _ = tx.send(line.to_string());
        }
        LineReader { rx }
    }
}
