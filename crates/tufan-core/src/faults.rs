//! Deterministic fault injection for tests. Inactive (one relaxed atomic
//! load) unless a test arms a fault.

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    Write,
    Verify,
    Publish,
    SourceDelete,
    /// Simulated crash right after the destination is committed in a move.
    CrashAfterCommit,
}

static ARMED: AtomicBool = AtomicBool::new(false);
static FAULTS: Mutex<Vec<(Stage, u32)>> = Mutex::new(Vec::new());

/// Fail the next `times` hits of `stage`.
pub fn arm(stage: Stage, times: u32) {
    FAULTS.lock().unwrap().push((stage, times));
    ARMED.store(true, Ordering::SeqCst);
}

pub fn clear() {
    FAULTS.lock().unwrap().clear();
    ARMED.store(false, Ordering::SeqCst);
}

pub(crate) fn hit(stage: Stage) -> io::Result<()> {
    if !ARMED.load(Ordering::Relaxed) {
        return Ok(());
    }
    let mut f = FAULTS.lock().unwrap();
    if let Some(entry) = f.iter_mut().find(|(s, n)| *s == stage && *n > 0) {
        entry.1 -= 1;
        return Err(io::Error::other(format!("injected fault at {stage:?}")));
    }
    Ok(())
}
