//! Background work helper shared by the GUI windows.
//!
//! Every window used to duplicate the same `busy` flag plus worker-thread
//! pattern. If a worker panicked or the thread could not be started, the flag
//! stayed set and the refresh button stopped working for the rest of the
//! session. This module keeps that invariant in one place.

use native_windows_gui as nwg;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// Clears the busy flag when the worker leaves, including on panic.
struct BusyGuard(Arc<AtomicBool>);

impl Drop for BusyGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

/// Runs `job` on a worker thread and always restores the busy flag.
///
/// `sender` is notified after the job finishes, including when it panicked, so
/// the UI can re-enable its controls and show an empty result. Returns `false`
/// when the thread could not be spawned; the busy flag is already cleared in
/// that case and the caller should report the failure.
pub fn spawn(
    busy: Arc<AtomicBool>,
    sender: nwg::NoticeSender,
    job: impl FnOnce() + Send + 'static,
) -> bool {
    let reset_on_failure = Arc::clone(&busy);
    let worker = std::thread::Builder::new()
        .name("battery-tray-worker".to_owned())
        .spawn(move || {
            let guard = BusyGuard(busy);
            let outcome = catch_unwind(AssertUnwindSafe(job));
            // Release the flag before notifying so the UI never observes
            // "not busy" one message late.
            drop(guard);
            if outcome.is_err() {
                eprintln!("后台任务异常退出");
            }
            sender.notice();
        });
    match worker {
        Ok(_) => true,
        Err(error) => {
            reset_on_failure.store(false, Ordering::Release);
            eprintln!("启动后台任务失败：{error}");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn busy_guard_is_released_when_job_finishes() {
        let busy = Arc::new(AtomicBool::new(true));
        let worker = {
            let busy = Arc::clone(&busy);
            std::thread::spawn(move || {
                let _guard = BusyGuard(busy);
            })
        };
        assert!(worker.join().is_ok());
        assert!(!busy.load(Ordering::Acquire));
    }
}
