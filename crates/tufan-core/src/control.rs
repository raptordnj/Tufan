//! Pause / resume / cancel signalling shared between a job and its workers.

use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::Duration;

use crate::error::{Error, Result};

const RUNNING: u8 = 0;
const PAUSED: u8 = 1;
const CANCELLED: u8 = 2;

#[derive(Default)]
pub struct Control {
    state: AtomicU8,
    lock: Mutex<()>,
    cv: Condvar,
}

impl Control {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn pause(&self) {
        let _ = self
            .state
            .compare_exchange(RUNNING, PAUSED, Ordering::SeqCst, Ordering::SeqCst);
    }
    pub fn resume(&self) {
        if self
            .state
            .compare_exchange(PAUSED, RUNNING, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
        {
            let _g = self.lock.lock().unwrap();
            self.cv.notify_all();
        }
    }
    pub fn cancel(&self) {
        self.state.store(CANCELLED, Ordering::SeqCst);
        let _g = self.lock.lock().unwrap();
        self.cv.notify_all();
    }
    pub fn is_paused(&self) -> bool {
        self.state.load(Ordering::SeqCst) == PAUSED
    }
    pub fn is_cancelled(&self) -> bool {
        self.state.load(Ordering::SeqCst) == CANCELLED
    }

    /// Called at safe boundaries. Blocks while paused; errors if cancelled.
    pub fn checkpoint(&self) -> Result<()> {
        loop {
            match self.state.load(Ordering::SeqCst) {
                RUNNING => return Ok(()),
                CANCELLED => return Err(Error::Cancelled),
                _ => {
                    let g = self.lock.lock().unwrap();
                    if self.state.load(Ordering::SeqCst) == PAUSED {
                        let _ = self.cv.wait_timeout(g, Duration::from_millis(500)).unwrap();
                    }
                }
            }
        }
    }

    /// Interruptible sleep used by retry backoff.
    pub fn sleep(&self, d: Duration) -> Result<()> {
        let g = self.lock.lock().unwrap();
        if self.is_cancelled() {
            return Err(Error::Cancelled);
        }
        let _ = self.cv.wait_timeout(g, d).unwrap();
        if self.is_cancelled() {
            return Err(Error::Cancelled);
        }
        Ok(())
    }
}

/// Lock-free progress counters, polled by front ends.
#[derive(Default)]
pub struct Progress {
    pub files_discovered: AtomicU64,
    pub files_done: AtomicU64,
    pub files_failed: AtomicU64,
    pub files_skipped: AtomicU64,
    pub bytes_known: AtomicU64,
    pub bytes_done: AtomicU64,
    pub scan_complete: AtomicU8,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Snapshot {
    pub files_discovered: u64,
    pub files_done: u64,
    pub files_failed: u64,
    pub files_skipped: u64,
    pub bytes_known: u64,
    pub bytes_done: u64,
    pub scan_complete: bool,
}

impl Progress {
    pub fn snapshot(&self) -> Snapshot {
        let r = Ordering::Relaxed;
        Snapshot {
            files_discovered: self.files_discovered.load(r),
            files_done: self.files_done.load(r),
            files_failed: self.files_failed.load(r),
            files_skipped: self.files_skipped.load(r),
            bytes_known: self.bytes_known.load(r),
            bytes_done: self.bytes_done.load(r),
            scan_complete: self.scan_complete.load(r) != 0,
        }
    }
}
