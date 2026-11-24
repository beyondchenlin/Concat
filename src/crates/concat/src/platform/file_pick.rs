// SPDX-License-Identifier: AGPL-3.0-or-later
//! A file selection owns its copied resources until a current UI callback
//! accepts them. Cancelling or dropping a late result releases only that pick.

use std::path::PathBuf;
#[cfg(any(target_os = "android", target_os = "ios", test))]
use std::sync::atomic::{AtomicBool, AtomicU64};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex, Weak};

const PENDING: u8 = 0;
const ACCEPTED: u8 = 1;
const CANCELLED: u8 = 2;
#[cfg(any(target_os = "android", target_os = "ios", test))]
static NEXT: AtomicU64 = AtomicU64::new(1);
static ACTIVE: Mutex<Option<Weak<Shared>>> = Mutex::new(None);
type Settlement = Box<dyn Fn(bool) -> Result<(), String> + Send + Sync>;

/// Actual copied bytes for one item, plus fully completed selection items.
#[cfg(any(target_os = "android", target_os = "ios", test))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PickProgress {
    /// Items whose copy attempt has finished, including reported failures.
    pub completed: u32,
    /// All items in the user's selection, preserving its original order.
    pub total: u32,
    /// Bytes written for the current item; resets when the next item starts.
    pub copied_bytes: u64,
    /// Zero means the provider did not expose a usable size.
    pub total_bytes: u64,
}

#[cfg(any(target_os = "android", target_os = "ios", test))]
impl PickProgress {
    pub(crate) fn fraction(self) -> f32 {
        if self.total == 0 {
            return 0.0;
        }
        let part = if self.total_bytes == 0 {
            0.0
        } else {
            (self.copied_bytes as f64 / self.total_bytes as f64).min(1.0)
        };
        ((f64::from(self.completed) + part) / f64::from(self.total)).min(1.0) as f32
    }
}

struct Shared {
    #[cfg(any(target_os = "android", target_os = "ios", test))]
    id: u64,
    state: AtomicU8,
    #[cfg(any(target_os = "android", target_os = "ios", test))]
    progress: Box<dyn Fn(PickProgress) + Send + Sync>,
    settled: Box<dyn Fn(bool) + Send + Sync>,
    platform: Mutex<Option<Settlement>>,
    #[cfg(any(target_os = "android", target_os = "ios", test))]
    platform_installed: AtomicBool,
}

/// A platform operation's exact generation and one-way ownership decision.
#[derive(Clone)]
pub struct PickTransfer(Arc<Shared>);

impl PickTransfer {
    #[cfg(any(target_os = "android", target_os = "ios", test))]
    pub(crate) fn new(
        progress: impl Fn(PickProgress) + Send + Sync + 'static,
        settled: impl Fn(bool) + Send + Sync + 'static,
    ) -> Self {
        let id = loop {
            let id = NEXT.fetch_add(1, Ordering::Relaxed);
            if id != 0 {
                break id;
            }
        };
        Self(Arc::new(Shared {
            id,
            state: AtomicU8::new(PENDING),
            progress: Box::new(progress),
            settled: Box::new(settled),
            platform: Mutex::new(None),
            platform_installed: AtomicBool::new(false),
        }))
    }

    /// The number is opaque; platform callbacks must return this exact value.
    #[cfg(any(target_os = "android", target_os = "ios", test))]
    pub fn id(&self) -> u64 {
        self.0.id
    }

    /// False after either cancellation or final UI acceptance.
    #[cfg(any(target_os = "android", target_os = "ios", test))]
    pub fn pending(&self) -> bool {
        self.0.state.load(Ordering::Acquire) == PENDING
    }

    /// Register before starting the system picker. True releases copied files
    /// to the application; false cancels I/O and removes only owned copies.
    #[cfg(any(target_os = "android", target_os = "ios", test))]
    pub fn on_settled(
        &self,
        callback: impl Fn(bool) -> Result<(), String> + Send + Sync + 'static,
    ) {
        assert!(
            !self.0.platform_installed.swap(true, Ordering::AcqRel),
            "file-pick settlement hook installed twice"
        );
        let mut hook = self
            .0
            .platform
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        *hook = Some(Box::new(callback));
        let state = self.0.state.load(Ordering::Acquire);
        assert!(
            state != ACCEPTED,
            "file-pick hook must be installed before accepting its resources"
        );
        let immediate = if state == PENDING { None } else { hook.take() };
        drop(hook);
        if let Some(callback) = immediate {
            let _ = callback(false);
        }
    }

    /// A zero-item update is a cancellation poll and never renders 0/0.
    #[cfg(any(target_os = "android", target_os = "ios", test))]
    pub fn progress(&self, progress: PickProgress) -> bool {
        if !self.pending() {
            return false;
        }
        if progress.total > 0 && progress.completed <= progress.total {
            (self.0.progress)(progress);
        }
        self.pending()
    }

    /// Retire once. Repeated cancellation cannot delete accepted media.
    pub fn cancel(&self) -> bool {
        self.decide(CANCELLED).unwrap_or(true)
    }

    fn decide(&self, state: u8) -> Result<bool, String> {
        let mut hook = self
            .0
            .platform
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if self
            .0
            .state
            .compare_exchange(PENDING, state, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Ok(false);
        }
        let callback = hook.take();
        drop(hook);
        if let Some(callback) = callback
            && let Err(error) = callback(state == ACCEPTED)
        {
            // Never expose paths if the platform did not acknowledge the
            // transfer of ownership. A best-effort cancellation still
            // cannot remove files that the platform already accepted.
            self.0.state.store(CANCELLED, Ordering::Release);
            (self.0.settled)(false);
            if state == ACCEPTED {
                let _ = callback(false);
            }
            return Err(error);
        }
        (self.0.settled)(state == ACCEPTED);
        Ok(true)
    }
}

/// Contains a platform result that has not yet been accepted by the current
/// project/UI generation. Dropping it cancels that request's owned copies.
pub struct PickedFiles {
    paths: Vec<PathBuf>,
    transfer: Option<PickTransfer>,
}

impl PickedFiles {
    pub(crate) fn new(paths: Vec<PathBuf>, transfer: Option<PickTransfer>) -> Self {
        Self { paths, transfer }
    }
    /// Call only on the UI thread after project/request identity checks and
    /// immediately before starting the ordered native media preparation.
    pub fn accept(mut self) -> Result<Option<Vec<PathBuf>>, String> {
        if let Some(transfer) = &self.transfer
            && !transfer.decide(ACCEPTED)?
        {
            return Ok(None);
        }
        self.transfer = None;
        Ok(Some(std::mem::take(&mut self.paths)))
    }
}

impl Drop for PickedFiles {
    fn drop(&mut self) {
        if let Some(transfer) = &self.transfer {
            transfer.cancel();
        }
    }
}

#[cfg(any(target_os = "android", target_os = "ios", test))]
pub(crate) fn activate(transfer: &PickTransfer) {
    let old = ACTIVE
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .replace(Arc::downgrade(&transfer.0))
        .and_then(|weak| weak.upgrade());
    if let Some(old) = old {
        PickTransfer(old).cancel();
    }
}

pub(crate) fn cancel_active() -> bool {
    let current = ACTIVE
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .as_ref()
        .and_then(Weak::upgrade);
    current.is_some_and(|current| PickTransfer(current).cancel())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn observed() -> (PickTransfer, Arc<Mutex<Vec<bool>>>) {
        let outcomes = Arc::new(Mutex::new(Vec::new()));
        let record = Arc::clone(&outcomes);
        let transfer = PickTransfer::new(|_| {}, |_| {});
        transfer.on_settled(move |accepted| {
            record.lock().unwrap().push(accepted);
            Ok(())
        });
        (transfer, outcomes)
    }

    #[test]
    fn cancellation_during_result_handoff_never_accepts_or_releases_owned_files() {
        let (transfer, outcomes) = observed();
        let result = PickedFiles::new(
            vec![PathBuf::from("this-request-copy")],
            Some(transfer.clone()),
        );
        assert!(transfer.cancel());
        assert!(result.accept().unwrap().is_none());
        assert!(!transfer.cancel());
        assert_eq!(*outcomes.lock().unwrap(), vec![false]);
        assert!(!transfer.progress(PickProgress {
            completed: 0,
            total: 1,
            copied_bytes: 1024,
            total_bytes: 2048
        }));
    }

    #[test]
    fn accepted_copies_cannot_be_removed_by_late_cancellation_or_drop() {
        let (transfer, outcomes) = observed();
        let paths = vec![PathBuf::from("accepted-copy")];
        assert_eq!(
            PickedFiles::new(paths.clone(), Some(transfer.clone()))
                .accept()
                .unwrap(),
            Some(paths)
        );
        assert!(!transfer.cancel());
        assert_eq!(*outcomes.lock().unwrap(), vec![true]);
    }

    #[test]
    fn stale_ui_result_drop_cancels_exactly_its_own_generation() {
        let (old, old_outcomes) = observed();
        let (new, new_outcomes) = observed();
        assert_ne!(old.id(), new.id());
        drop(PickedFiles::new(vec![PathBuf::from("old-copy")], Some(old)));
        assert_eq!(*old_outcomes.lock().unwrap(), vec![false]);
        assert!(new.pending());
        assert!(new_outcomes.lock().unwrap().is_empty());
    }

    #[test]
    fn cancellation_before_platform_hook_installation_is_not_lost() {
        let transfer = PickTransfer::new(|_| {}, |_| {});
        assert!(transfer.cancel());
        let outcomes = Arc::new(Mutex::new(Vec::new()));
        let record = Arc::clone(&outcomes);
        transfer.on_settled(move |accepted| {
            record.lock().unwrap().push(accepted);
            Ok(())
        });
        assert_eq!(*outcomes.lock().unwrap(), vec![false]);
    }

    #[test]
    fn failed_platform_handoff_exposes_no_paths_and_requests_owned_cleanup() {
        let transfer = PickTransfer::new(|_| {}, |_| {});
        let calls = Arc::new(Mutex::new(Vec::new()));
        let record = Arc::clone(&calls);
        transfer.on_settled(move |accepted| {
            record.lock().unwrap().push(accepted);
            if accepted {
                Err("platform acceptance failed".into())
            } else {
                Ok(())
            }
        });
        let result = PickedFiles::new(vec![PathBuf::from("not-yet-owned")], Some(transfer.clone()));
        assert_eq!(result.accept().unwrap_err(), "platform acceptance failed");
        assert!(!transfer.pending());
        assert_eq!(*calls.lock().unwrap(), vec![true, false]);
    }

    #[test]
    fn replacing_the_active_selection_retires_only_the_old_owner() {
        let (old, old_outcomes) = observed();
        let (new, new_outcomes) = observed();
        activate(&old);
        activate(&new);
        assert_eq!(*old_outcomes.lock().unwrap(), vec![false]);
        assert!(new.pending());
        assert!(cancel_active());
        assert!(!cancel_active());
        assert_eq!(*new_outcomes.lock().unwrap(), vec![false]);
        let (accepted, accepted_outcomes) = observed();
        activate(&accepted);
        assert!(
            PickedFiles::new(Vec::new(), Some(accepted))
                .accept()
                .unwrap()
                .is_some()
        );
        assert!(!cancel_active());
        assert_eq!(*accepted_outcomes.lock().unwrap(), vec![true]);
    }

    #[test]
    fn concurrent_accept_and_cancel_choose_one_platform_owner() {
        for _ in 0..64 {
            let (transfer, outcomes) = observed();
            let gate = Arc::new(std::sync::Barrier::new(2));
            let accept_gate = Arc::clone(&gate);
            let result =
                PickedFiles::new(vec![PathBuf::from("racing-copy")], Some(transfer.clone()));
            let accepting = std::thread::spawn(move || {
                accept_gate.wait();
                result.accept().unwrap()
            });
            gate.wait();
            let cancelled = transfer.cancel();
            let paths = accepting.join().unwrap();
            assert_ne!(cancelled, paths.is_some());
            assert_eq!(*outcomes.lock().unwrap(), vec![paths.is_some()]);
        }
    }

    #[test]
    fn actual_bytes_refine_item_progress_without_inventing_unknown_size() {
        assert_eq!(
            PickProgress {
                completed: 1,
                total: 2,
                copied_bytes: 50,
                total_bytes: 100
            }
            .fraction(),
            0.75
        );
        assert_eq!(
            PickProgress {
                completed: 0,
                total: 1,
                copied_bytes: 50,
                total_bytes: 0
            }
            .fraction(),
            0.0
        );
        assert_eq!(
            PickProgress {
                completed: 1,
                total: 1,
                copied_bytes: 0,
                total_bytes: 0
            }
            .fraction(),
            1.0
        );
    }
}
