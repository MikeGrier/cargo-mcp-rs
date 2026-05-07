// Copyright (c) Michael Grier.

//! Channel-based line reader with timeout support.
//!
//! A background thread reads lines from stdin and sends them through an
//! `mpsc` channel. This lets callers choose between blocking reads (for
//! the main JSON-RPC loop) and bounded-wait reads (for elicitation prompts
//! that should time out instead of hanging the server).
//!
//! ## Cancellation
//!
//! Call [`LineReader::register_cancel`] with the JSON-RPC request ID of the
//! currently running tool. The background thread will intercept any incoming
//! `notifications/cancelled` message whose `requestId` matches and set the
//! returned `Arc<AtomicBool>` to `true`. Call [`LineReader::clear_cancel`]
//! when the tool finishes (whether normally or via cancellation).

use std::{
    io::{self, BufRead},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver},
    },
    thread,
    time::Duration,
};

use serde_json::Value;

/// A slot that the background reader thread checks on every incoming line.
///
/// When set, any `notifications/cancelled` whose `requestId` matches the
/// stored `Value` will set the `AtomicBool` to `true` and be consumed
/// (not forwarded to the main loop).
type CancelWatcher = Arc<Mutex<Option<(Value, Arc<AtomicBool>)>>>;

/// Channel-based line reader with timeout-bounded reads.
pub struct LineReader {
    rx: Receiver<String>,
    cancel_watcher: CancelWatcher,
}

impl LineReader {
    /// Spawn a background reader thread for the given `stdin` handle.
    pub fn new(stdin: io::Stdin) -> Self {
        let cancel_watcher: CancelWatcher = Arc::new(Mutex::new(None));
        let watcher_clone = Arc::clone(&cancel_watcher);
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let mut locked = stdin.lock();
            let mut line = String::new();
            loop {
                line.clear();
                match locked.read_line(&mut line) {
                    Ok(0) => break,
                    Ok(_) => {
                        // Fast-path: only parse if there is an active cancel
                        // watcher and the line looks like a cancel notification.
                        let intercepted = if line.contains("notifications/cancelled") {
                            if let Ok(msg) = serde_json::from_str::<Value>(line.trim()) {
                                let is_cancel = msg.get("method").and_then(|v| v.as_str())
                                    == Some("notifications/cancelled");
                                if is_cancel {
                                    let req_id =
                                        msg.get("params").and_then(|p| p.get("requestId")).cloned();
                                    if let Some(cancel_id) = req_id {
                                        let guard = watcher_clone.lock().unwrap();
                                        if let Some((ref watched_id, ref cancel_flag)) = *guard {
                                            if &cancel_id == watched_id {
                                                cancel_flag.store(true, Ordering::Release);
                                                true // intercepted; do not enqueue
                                            } else {
                                                false
                                            }
                                        } else {
                                            false
                                        }
                                    } else {
                                        false
                                    }
                                } else {
                                    false
                                }
                            } else {
                                false
                            }
                        } else {
                            false
                        };

                        if !intercepted && tx.send(line.clone()).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });
        LineReader { rx, cancel_watcher }
    }

    /// Register a cancel watcher for the given JSON-RPC request ID.
    ///
    /// Returns an `Arc<AtomicBool>` that will be set to `true` if the client
    /// sends `notifications/cancelled` with a matching `requestId`. The
    /// notification is consumed and not forwarded to the main loop.
    ///
    /// Call [`clear_cancel`] when the operation finishes.
    pub fn register_cancel(&self, request_id: Value) -> Arc<AtomicBool> {
        let token = Arc::new(AtomicBool::new(false));
        *self.cancel_watcher.lock().unwrap() = Some((request_id, Arc::clone(&token)));
        token
    }

    /// Clear the cancel watcher registered by [`register_cancel`].
    pub fn clear_cancel(&self) {
        *self.cancel_watcher.lock().unwrap() = None;
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
        LineReader {
            rx,
            cancel_watcher: Arc::new(Mutex::new(None)),
        }
    }
}
