//! Cooperative cancellation shared between the UI, signal handlers, and the
//! runner thread. Aborting kills the currently running fio worker (fio jobs
//! run as threads via `--thread`, so killing the worker kills all I/O).

use std::process::Child;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Default)]
pub struct AbortHandle {
    aborted: AtomicBool,
    current: Mutex<Option<Child>>,
}

impl AbortHandle {
    pub fn is_aborted(&self) -> bool {
        self.aborted.load(Ordering::SeqCst)
    }

    /// Requests cancellation and kills the in-flight fio worker, if any.
    /// Returns false when an abort was already pending — callers use this to
    /// escalate (e.g. a second Ctrl-C force-exits).
    pub fn abort(&self) -> bool {
        let first = !self.aborted.swap(true, Ordering::SeqCst);
        if let Some(child) = self.current.lock().unwrap().as_mut() {
            let _ = child.kill();
        }
        first
    }

    /// Registers the running worker so `abort()` can kill it. If an abort
    /// already happened, the child is killed immediately.
    pub fn adopt(&self, child: Child) {
        let mut slot = self.current.lock().unwrap();
        *slot = Some(child);
        if self.is_aborted()
            && let Some(child) = slot.as_mut()
        {
            let _ = child.kill();
        }
    }

    /// Reaps the registered worker. Call only after its stdout reached EOF,
    /// so the wait cannot block for long.
    pub fn reap(&self) -> Option<std::io::Result<std::process::ExitStatus>> {
        let child = self.current.lock().unwrap().take();
        child.map(|mut c| c.wait())
    }
}
