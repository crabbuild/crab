//! Simple stderr-based progress reporting for LFS transfers.
//!
//! Uses `\r` carriage return for in-place updates on terminals.
//! Falls back to line-based output when stderr is not a terminal.

use std::io::Write;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

/// Progress reporter for LFS batch transfers.
pub struct TransferProgress {
    total: u64,
    completed: Arc<AtomicU64>,
    operation: &'static str,
    start: Instant,
    is_tty: bool,
}

impl TransferProgress {
    pub fn new(operation: &'static str, total: u64) -> Self {
        let is_tty = atty_stderr();
        Self {
            total,
            completed: Arc::new(AtomicU64::new(0)),
            operation,
            start: Instant::now(),
            is_tty,
        }
    }

    /// Get a clone of the completed counter for use in async tasks.
    pub fn counter(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.completed)
    }

    /// Increment the completed count and print progress.
    pub fn inc(&self) {
        let done = self.completed.fetch_add(1, Ordering::Relaxed) + 1;
        self.print(done);
    }

    /// Print the final summary.
    pub fn finish(&self) {
        let done = self.completed.load(Ordering::Relaxed);
        let elapsed = self.start.elapsed();
        if self.is_tty {
            eprint!("\r\x1b[K"); // Clear the line.
        }
        eprintln!(
            "{}: {done}/{} object(s), {:.1}s",
            self.operation,
            self.total,
            elapsed.as_secs_f64(),
        );
    }

    /// Return elapsed seconds since the progress reporter was created.
    pub fn elapsed_secs(&self) -> f64 {
        self.start.elapsed().as_secs_f64()
    }

    fn print(&self, done: u64) {
        if self.is_tty {
            let pct = if self.total > 0 {
                (done * 100) / self.total
            } else {
                100
            };
            eprint!("\r{}: {done}/{} ({pct}%)", self.operation, self.total,);
            let _ = std::io::stderr().flush();
        }
    }
}

/// Check if stderr is a terminal (for progress bar rendering).
fn atty_stderr() -> bool {
    #[cfg(unix)]
    {
        // SAFETY: `isatty` is a standard POSIX function that checks whether
        // file descriptor 2 (stderr) refers to a terminal. It has no
        // preconditions beyond a valid fd number.
        unsafe { libc::isatty(2) != 0 }
    }
    #[cfg(not(unix))]
    {
        false
    }
}
