// SPDX-License-Identifier: AGPL-3.0-or-later
//! Per-message and queued-byte bounds for the original runtime's native ports.
//! These are transport memory limits, not task, turn, token or spending budgets.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

/// A complete model request, including history, tools and JSON encoding.
/// The former 1 MiB boundary rejected a verified 1,048,576-token model's
/// history before its original automatic compaction could be reached.
pub const MAX_MODEL_REQUEST_BYTES: usize = 16 * 1024 * 1024;
/// A native-wire envelope can quote an already encoded model JSON body.
/// Reserve twice that body's bytes plus bounded protocol metadata.
pub const MAX_BRIDGE_FRAME_BYTES: usize = 2 * MAX_MODEL_REQUEST_BYTES + 64 * 1024;
/// Aggregate bytes held by one native mailbox or platform delivery queue.
/// Keep a larger individual frame from multiplying the existing message count
/// into gigabytes of queued data. Small-frame concurrency is unchanged.
pub const MAX_QUEUED_FRAME_BYTES: usize = 128 * 1024 * 1024;

/// A thread-safe byte budget shared only by one transport queue.
#[derive(Clone, Debug)]
pub struct ByteBudget {
    used: Arc<AtomicUsize>,
    limit: usize,
}

impl ByteBudget {
    /// Create an empty queue budget; it never grants model or native authority.
    pub fn new(limit: usize) -> Self {
        Self {
            used: Arc::new(AtomicUsize::new(0)),
            limit,
        }
    }

    /// Reserve before copying/enqueuing a frame. Dropping the returned guard
    /// releases exactly these bytes on delivery, rejection or queue shutdown.
    pub fn reserve(&self, bytes: usize) -> Option<ByteReservation> {
        self.used
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                used.checked_add(bytes).filter(|next| *next <= self.limit)
            })
            .ok()?;
        Some(ByteReservation {
            used: Arc::clone(&self.used),
            bytes,
        })
    }
}

/// Queue ownership of a byte reservation; moving it does not change the budget.
#[derive(Debug)]
pub struct ByteReservation {
    used: Arc<AtomicUsize>,
    bytes: usize,
}

impl Drop for ByteReservation {
    fn drop(&mut self) {
        self.used.fetch_sub(self.bytes, Ordering::AcqRel);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Barrier, mpsc};

    #[test]
    fn rejected_enqueue_and_queue_drop_release_the_reserved_bytes() {
        let budget = ByteBudget::new(12);
        let (sender, receiver) = mpsc::sync_channel(1);
        sender.try_send(budget.reserve(8).unwrap()).unwrap();
        assert!(budget.reserve(5).is_none());
        let failed = sender.try_send(budget.reserve(4).unwrap()).unwrap_err();
        drop(failed);
        assert_eq!(budget.used.load(Ordering::Acquire), 8);
        drop(receiver);
        assert_eq!(budget.used.load(Ordering::Acquire), 0);
        assert!(budget.reserve(12).is_some());
        assert!(budget.reserve(usize::MAX).is_none());
    }

    #[test]
    fn concurrent_reservations_never_exceed_the_queue_budget() {
        let budget = ByteBudget::new(2);
        let barrier = Arc::new(Barrier::new(9));
        let workers: Vec<_> = (0..8)
            .map(|_| {
                let budget = budget.clone();
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    let reservation = budget.reserve(1);
                    barrier.wait();
                    barrier.wait();
                    reservation.is_some()
                })
            })
            .collect();
        barrier.wait();
        assert_eq!(budget.used.load(Ordering::Acquire), 2);
        barrier.wait();
        assert_eq!(
            workers
                .into_iter()
                .map(|worker| worker.join().unwrap())
                .filter(|admitted| *admitted)
                .count(),
            2
        );
        assert_eq!(budget.used.load(Ordering::Acquire), 0);
    }
}
