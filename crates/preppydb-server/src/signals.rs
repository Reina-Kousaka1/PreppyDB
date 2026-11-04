//! SIGTERM/SIGINT handling for graceful shutdown.
//!
//! `std` has no signal API and this project deliberately has no dependencies, so the two
//! handlers are installed through a direct declaration of the C `signal(3)` call on Unix.
//! The handler does one thing — store a flag, which is async-signal-safe — and the server
//! polls [`requested`] to stop accepting requests, join its workers and close the engine
//! cleanly (WAL close, manifest refresh) instead of dying mid-write.
//!
//! On platforms without this support the default behaviour applies: the process is
//! terminated and the write-ahead log recovers the data on the next start (which is what
//! the crash-recovery test proves works).

use std::sync::atomic::{AtomicBool, Ordering};

static STOP: AtomicBool = AtomicBool::new(false);

/// Whether a shutdown signal has arrived since the process started.
pub fn requested() -> bool {
    STOP.load(Ordering::SeqCst)
}

/// Install the handlers. Safe to call more than once.
pub fn install() {
    platform::install();
}

#[cfg(unix)]
mod platform {
    use super::STOP;
    use std::sync::atomic::Ordering;

    const SIGINT: i32 = 2;
    const SIGTERM: i32 = 15;

    extern "C" fn on_signal(_signal: i32) {
        // Only an atomic store here: everything else is unsafe in a signal handler.
        STOP.store(true, Ordering::SeqCst);
    }

    extern "C" {
        /// `void (*signal(int, void (*)(int)))(int)` — the historic simple API, which
        /// keeps the handler installed (it does not reset to `SIG_DFL` after the first
        /// delivery on the platforms this project supports).
        fn signal(signum: i32, handler: extern "C" fn(i32)) -> usize;
    }

    pub fn install() {
        // SAFETY: `on_signal` has the required signature, is `extern "C"` and lives for
        // the whole program; the return value (the previous handler) is intentionally
        // discarded because there is nothing sensible to restore it to.
        unsafe {
            signal(SIGINT, on_signal);
            signal(SIGTERM, on_signal);
        }
    }
}

#[cfg(not(unix))]
mod platform {
    pub fn install() {}
}

#[cfg(test)]
mod tests {
    #[test]
    fn install_is_idempotent() {
        super::install();
        super::install();
        assert!(!super::requested(), "no signal has been delivered");
    }
}
