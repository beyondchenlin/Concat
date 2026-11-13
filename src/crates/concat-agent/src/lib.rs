// SPDX-License-Identifier: AGPL-3.0-or-later
//! Native admission for an embedded agent; this is not an agent scheduler.
//!
//! Acquire [`SessionOwner`] before opening Pi's writable storage or scheduler.
//! The canonical storage directory has one permanent lock file. An operating
//! system lock covers its complete ownership lifetime. A generation is appended
//! and synced before it is issued, and every new owner starts in
//! [`Phase::RecoveryRequired`]. No recovered queue is admitted automatically.
//!
//! The host must route every write, model/tool dispatch, and delayed callback
//! through [`OperationPermit::with_valid`]. Check the current project, task,
//! cancellation, revision, and track locks in the same native edit-thread
//! callback; this crate only checks host ownership. Never expose its owner or a
//! recovery approval method directly to model-generated code.
//!
//! On handoff, revoke first, stop Pi and close its writable storage, drop all
//! in-flight permits, then call [`SessionOwner::finish_handoff`]. A timeout keeps
//! the operating system lock; it never steals ownership from a live host.
//! Dropping the owner also revokes it, but outstanding permits retain the lock
//! until they have drained. A process crash releases the kernel lock; its next
//! owner still requires reconciliation and an explicit user decision.
//!
//! The lock file must never be deleted, renamed, or copied over while any host
//! uses the storage. Use a local application-owned directory; network filesystems
//! and storage copied outside this protocol are not supported ownership domains.
//! In particular, a database transaction is not a substitute for this lock.

use std::fmt;
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, Weak};
use std::time::{Duration, Instant};

/// Non-secret native model channels, versioned independently of projects and sessions.
pub mod configuration;
/// Product-scoped platform secure credentials, never persisted in model metadata.
pub mod credentials;
/// Durable native edit intent, receipt, disk-save and notification states.
pub mod operations;
/// Persistent project-location admission for independent script versions.
pub mod script_locations;
/// Independent project script versions and explicit cutting-plan input bindings.
pub mod scripts;
/// Original Pi session storage through a generation-fenced native SQLite port.
pub mod sqlite;

/// A permanent file, shared by all embedded host instances using this storage.
pub const OWNER_FILE_NAME: &str = ".bk-cat-agent-owner.lock";

const RECORD_MAGIC: [u8; 8] = *b"BKCATG01";
const RECORD_SIZE: usize = 24;

/// Whether this host is allowed to admit new side effects.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum Phase {
    /// Ownership exists, but reconciliation and a user decision are required.
    RecoveryRequired,
    /// A user decision admitted fresh, revalidated work in this host generation.
    Ready,
    /// New and delayed work is refused; existing permits still hold the lock.
    Draining,
    /// All permits drained and the operating system lock was released.
    Released,
}

/// Read-only diagnostic state; it is not an execution credential.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OwnerStatus {
    /// Persisted generation of the host.
    pub generation: u64,
    /// Current admission phase.
    pub phase: Phase,
    /// Permits which must finish before ownership can move.
    pub active_operations: usize,
}

/// The native state the user reviewed before admitting this host.
///
/// The UI must capture these values from the current native project and Pi
/// storage after reconciliation, not from a model or an old plan. This record
/// does not itself verify the project; the edit bridge revalidates it on every
/// submission. It deliberately contains no key or media content.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Reconciliation {
    /// Stable identity of the current native project.
    pub project_identity: String,
    /// Current native project revision, expressed by the integration.
    pub project_revision: String,
    /// Current persisted session revision, expressed by the integration.
    pub session_revision: String,
}

impl Reconciliation {
    fn is_complete(&self) -> bool {
        !self.project_identity.trim().is_empty()
            && !self.project_revision.trim().is_empty()
            && !self.session_revision.trim().is_empty()
    }
}

/// The explicit user choice made after reviewing an interrupted session.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecoveryDecision {
    /// Discard the interrupted queue and admit new work.
    DiscardInterrupted,
    /// Continue only work revalidated against the reviewed current state.
    ContinueAfterRevalidation,
}

/// A failed admission. Every failure leaves execution refused.
#[derive(Debug)]
pub enum Error {
    /// Another file handle or process owns this storage.
    Busy,
    /// A filesystem operation failed; no new generation was issued.
    Io {
        /// Operation which failed.
        operation: &'static str,
        /// Operating system error.
        source: io::Error,
    },
    /// The persistent generation journal is damaged; no automatic repair occurs.
    InvalidJournal,
    /// The persistent generation cannot be incremented without wrapping.
    GenerationExhausted,
    /// The caller has not completed the native recovery review.
    RecoveryRequired,
    /// This owner has been revoked or released.
    Revoked,
    /// The token belongs to another owner, store, or generation.
    TokenMismatch,
    /// The user-review record is missing a current-state identity or revision.
    IncompleteReconciliation,
    /// A recovery decision can only be recorded once for each new owner.
    RecoveryAlreadyCompleted,
    /// The integration retained permits past the handoff deadline.
    DrainTimeout {
        /// Number of outstanding permits at the deadline.
        active_operations: usize,
    },
    /// A panic occurred inside an admitted callback; execution is refused.
    Poisoned,
    /// The storage directory is not a directory or its lock entry is a symlink.
    InvalidStorage,
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Busy => formatter.write_str("another agent host owns this storage"),
            Self::Io { operation, source } => write!(formatter, "{operation}: {source}"),
            Self::InvalidJournal => {
                formatter.write_str("invalid host-generation journal; ownership refused")
            }
            Self::GenerationExhausted => formatter.write_str("host-generation counter exhausted"),
            Self::RecoveryRequired => {
                formatter.write_str("reconcile the interrupted session before executing work")
            }
            Self::Revoked => {
                formatter.write_str("this agent host no longer has execution authority")
            }
            Self::TokenMismatch => {
                formatter.write_str("the token does not belong to this host generation")
            }
            Self::IncompleteReconciliation => formatter
                .write_str("reconciliation is missing the current project or session revision"),
            Self::RecoveryAlreadyCompleted => {
                formatter.write_str("recovery was already approved for this host")
            }
            Self::DrainTimeout { active_operations } => write!(
                formatter,
                "{active_operations} operations still hold ownership"
            ),
            Self::Poisoned => {
                formatter.write_str("an admitted callback panicked; host execution refused")
            }
            Self::InvalidStorage => formatter
                .write_str("storage must be a local directory with a permanent regular lock file"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

fn io_error(operation: &'static str, source: io::Error) -> Error {
    Error::Io { operation, source }
}

// Rust 1.93.1's standard library returns Unsupported for Android file locks.
// Use the platform's descriptor-scoped flock there; a process-wide fcntl lock
// would let two independently opened hosts in the same process share ownership.
// The File remains in Core until every permit drains and handoff is explicit.
#[cfg(target_os = "android")]
fn android_flock(file: &File, operation: libc::c_int) -> io::Result<()> {
    use std::os::fd::AsRawFd;

    loop {
        // SAFETY: File owns a live descriptor for the entire call. flock does
        // not retain a pointer or close the descriptor; Core retains the File.
        if unsafe { libc::flock(file.as_raw_fd(), operation) } == 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

fn lock_new_owner_file(file: &File) -> io::Result<()> {
    #[cfg(target_os = "android")]
    {
        android_flock(file, libc::LOCK_EX)
    }
    #[cfg(not(target_os = "android"))]
    {
        file.lock()
    }
}

fn try_lock_owner_file(file: &File) -> Result<(), TryLockError> {
    #[cfg(target_os = "android")]
    {
        android_flock(file, libc::LOCK_EX | libc::LOCK_NB).map_err(|error| {
            if error.kind() == io::ErrorKind::WouldBlock {
                TryLockError::WouldBlock
            } else {
                TryLockError::Error(error)
            }
        })
    }
    #[cfg(not(target_os = "android"))]
    {
        file.try_lock()
    }
}

fn unlock_owner_file(file: &File) -> io::Result<()> {
    #[cfg(target_os = "android")]
    {
        android_flock(file, libc::LOCK_UN)
    }
    #[cfg(not(target_os = "android"))]
    {
        file.unlock()
    }
}

struct State {
    phase: Phase,
    active_operations: usize,
    release_requested: bool,
    file: Option<File>,
    reconciliation: Option<(Reconciliation, RecoveryDecision)>,
}

impl State {
    fn release_if_drained(&mut self) -> Result<(), Error> {
        if self.phase == Phase::Draining && self.release_requested && self.active_operations == 0 {
            // A concurrent process spawn can briefly inherit this open file
            // description. Closing our descriptor alone would then publish
            // Released while that inherited descriptor still keeps the lock.
            // Unlock only after every admitted operation has drained. On an
            // error retain both the handle and Draining; never claim handoff.
            if let Some(file) = self.file.as_ref() {
                unlock_owner_file(file)
                    .map_err(|error| io_error("unlock drained session ownership", error))?;
            }
            // Never unlink: the permanent inode defines one ownership domain.
            drop(self.file.take());
            self.phase = Phase::Released;
        }
        Ok(())
    }
}

struct Core {
    storage_root: PathBuf,
    generation: u64,
    state: Mutex<State>,
    drained: Condvar,
    // Admission can be retired without waiting for an already-running SQLite
    // step or disk sync. The existing mutex still protects synchronous writes.
    stop_requested: AtomicBool,
    published_phase: AtomicU8,
    published_operations: AtomicUsize,
}

impl Core {
    fn request_stop(&self) {
        self.stop_requested.store(true, Ordering::Release);
        self.drained.notify_all();
    }

    fn check_admission(&self) -> Result<(), Error> {
        if self.state.is_poisoned() {
            self.request_stop();
            return Err(Error::Poisoned);
        }
        if self.stop_requested.load(Ordering::Acquire) {
            return Err(Error::Revoked);
        }
        Ok(())
    }

    fn publish_status(&self, state: &State) {
        // Store the count before the phase: observing Released also observes
        // its zero drain count. These diagnostics never grant authority.
        self.published_operations
            .store(state.active_operations, Ordering::Release);
        self.published_phase
            .store(state.phase as u8, Ordering::Release);
    }

    fn state(&self) -> Result<MutexGuard<'_, State>, Error> {
        self.state.lock().map_err(|poisoned| {
            self.request_stop();
            let mut state = poisoned.into_inner();
            state.phase = Phase::Draining;
            let _ = state.release_if_drained();
            self.publish_status(&state);
            Error::Poisoned
        })
    }

    fn revoke(&self, request_release: bool) -> Result<(), Error> {
        self.request_stop();
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let result = if state.phase != Phase::Released {
            state.phase = Phase::Draining;
            state.release_requested |= request_release;
            state.release_if_drained()
        } else {
            Ok(())
        };
        self.publish_status(&state);
        self.drained.notify_all();
        result
    }
}

/// A read-only cancellation signal for a native connection's progress handler.
/// It cannot reset admission, acquire another generation or keep an owner alive.
#[derive(Clone)]
pub(crate) struct StopSignal(Weak<Core>);

impl StopSignal {
    pub(crate) fn requested(&self) -> bool {
        self.0.upgrade().is_none_or(|core| {
            core.stop_requested.load(Ordering::Acquire) || core.state.is_poisoned()
        })
    }
}

/// A credential bound to a live owner object and its durable generation.
///
/// Tokens are deliberately not serializable and cannot be constructed from a
/// generation number. A stale callback keeps its old token, never a replacement
/// acquired from whichever host happens to be current when it finishes.
#[derive(Clone)]
pub struct OwnerToken {
    core: Weak<Core>,
    generation: u64,
}

impl OwnerToken {
    /// The durable generation, for protocol envelopes and diagnostics.
    pub fn generation(&self) -> u64 {
        self.generation
    }
}

impl fmt::Debug for OwnerToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OwnerToken")
            .field("generation", &self.generation)
            .finish_non_exhaustive()
    }
}

/// The sole native owner of one canonical persistent-session directory.
///
/// This type is not cloneable. Tokens are weak handles and cannot keep an owner
/// alive; operation permits retain the operating system lock until they drain.
pub struct SessionOwner {
    core: Arc<Core>,
}

impl SessionOwner {
    /// Acquire an exclusive operating system lock and durably issue a generation.
    ///
    /// `storage_root` must already exist. The lock file is created if absent,
    /// but is never removed, truncated, renamed, or overwritten. A partial or
    /// malformed journal refuses acquisition and requires explicit repair.
    pub fn acquire(storage_root: impl AsRef<Path>) -> Result<Self, Error> {
        let storage_root = fs::canonicalize(storage_root)
            .map_err(|error| io_error("canonicalize session storage", error))?;
        if !storage_root.is_dir() {
            return Err(Error::InvalidStorage);
        }
        let lock_path = storage_root.join(OWNER_FILE_NAME);
        match fs::symlink_metadata(&lock_path) {
            Ok(metadata) if !metadata.is_file() || metadata.file_type().is_symlink() => {
                return Err(Error::InvalidStorage);
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(io_error("inspect session ownership file", error)),
        }
        let mut options = OpenOptions::new();
        options.read(true).write(true).truncate(false);
        // Do not allow another Windows handle to rename/delete the permanent
        // lock entry while it is held. Ordinary read/write sharing is needed
        // so another instance reaches try_lock and receives Busy.
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            options.share_mode(0x0000_0001 | 0x0000_0002);
        }
        // Only an atomic create_new can authorize an empty journal. An
        // existing empty file can be a truncated old generation and must
        // never reset the counter to one.
        let (mut file, created) = match options.create_new(true).open(&lock_path) {
            Ok(file) => (file, true),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                let file = options
                    .create_new(false)
                    .open(&lock_path)
                    .map_err(|error| io_error("open existing session ownership file", error))?;
                (file, false)
            }
            Err(error) => return Err(io_error("create session ownership file", error)),
        };
        if created {
            // A competing opener can briefly lock our new empty file before
            // us. It cannot become an owner: an existing empty journal is
            // refused below. Wait for that check to release its handle rather
            // than return Busy and strand our first initialization forever.
            lock_new_owner_file(&file)
                .map_err(|error| io_error("lock newly created ownership file", error))?;
        } else {
            match try_lock_owner_file(&file) {
                Ok(()) => {}
                Err(TryLockError::WouldBlock) => return Err(Error::Busy),
                Err(TryLockError::Error(error)) => {
                    return Err(io_error("lock session ownership file", error));
                }
            }
        }
        let generation = issue_generation(&mut file, created)?;
        // On Unix, also persist a newly-created lock directory entry. Windows
        // File does not expose portable directory flushing; sync_all above
        // provides the file-content durability used by the process-crash tests.
        #[cfg(unix)]
        File::open(&storage_root)
            .and_then(|directory| directory.sync_all())
            .map_err(|error| io_error("sync session storage directory", error))?;

        Ok(Self {
            core: Arc::new(Core {
                storage_root,
                generation,
                state: Mutex::new(State {
                    phase: Phase::RecoveryRequired,
                    active_operations: 0,
                    release_requested: false,
                    file: Some(file),
                    reconciliation: None,
                }),
                drained: Condvar::new(),
                stop_requested: AtomicBool::new(false),
                published_phase: AtomicU8::new(Phase::RecoveryRequired as u8),
                published_operations: AtomicUsize::new(0),
            }),
        })
    }

    /// The canonical storage directory covered by the operating system lock.
    pub fn storage_root(&self) -> &Path {
        &self.core.storage_root
    }

    /// Create a credential tied to this exact host, not just its generation number.
    pub fn token(&self) -> OwnerToken {
        OwnerToken {
            core: Arc::downgrade(&self.core),
            generation: self.core.generation,
        }
    }

    /// Read diagnostics without waiting for an in-flight write or disk sync.
    /// This published snapshot is never an execution credential.
    pub fn status(&self) -> Result<OwnerStatus, Error> {
        if self.core.state.is_poisoned() {
            self.core.request_stop();
            return Err(Error::Poisoned);
        }
        let mut phase = match self.core.published_phase.load(Ordering::Acquire) {
            0 => Phase::RecoveryRequired,
            1 => Phase::Ready,
            2 => Phase::Draining,
            3 => Phase::Released,
            _ => Phase::Draining,
        };
        let active_operations = self.core.published_operations.load(Ordering::Acquire);
        if self.core.stop_requested.load(Ordering::Acquire) && phase != Phase::Released {
            phase = Phase::Draining;
        }
        Ok(OwnerStatus {
            generation: self.core.generation,
            phase,
            active_operations,
        })
    }

    /// Atomically read native state while this owner is still valid.
    ///
    /// Unlike an operation permit, this admits only a synchronous read and
    /// permits recovery review before execution is enabled. Never write or
    /// schedule work in this closure. It shares the non-reentrant owner mutex.
    pub fn with_owned<T>(&self, token: &OwnerToken, read: impl FnOnce() -> T) -> Result<T, Error> {
        self.check_token(token)?;
        self.core.check_admission()?;
        let state = self.core.state()?;
        self.core.check_admission()?;
        if matches!(state.phase, Phase::Draining | Phase::Released) {
            return Err(Error::Revoked);
        }
        let result = read();
        drop(state);
        Ok(result)
    }

    fn check_token(&self, token: &OwnerToken) -> Result<(), Error> {
        let Some(core) = token.core.upgrade() else {
            return Err(Error::TokenMismatch);
        };
        if token.generation != self.core.generation || !Arc::ptr_eq(&core, &self.core) {
            return Err(Error::TokenMismatch);
        }
        Ok(())
    }

    /// Admit a user decision after native reconciliation; never resumes Pi itself.
    ///
    /// The integration must discard or revalidate the old queue according to
    /// `decision` before any entry that can start Pi's scheduler. This includes
    /// submission, waits, resume, abort, compact, and signals. In particular,
    /// directly aborting an old conversation is not a read-only queue cleanup.
    pub fn confirm_recovery(
        &self,
        token: &OwnerToken,
        reconciliation: Reconciliation,
        decision: RecoveryDecision,
    ) -> Result<(), Error> {
        self.check_token(token)?;
        self.core.check_admission()?;
        if !reconciliation.is_complete() {
            return Err(Error::IncompleteReconciliation);
        }
        let mut state = self.core.state()?;
        self.core.check_admission()?;
        match state.phase {
            Phase::RecoveryRequired => {
                state.reconciliation = Some((reconciliation, decision));
                state.phase = Phase::Ready;
                self.core.publish_status(&state);
                Ok(())
            }
            Phase::Ready => Err(Error::RecoveryAlreadyCompleted),
            Phase::Draining | Phase::Released => Err(Error::Revoked),
        }
    }

    /// The review which admitted this generation, for native diagnostics.
    pub fn reconciliation(&self) -> Result<Option<(Reconciliation, RecoveryDecision)>, Error> {
        Ok(self.core.state()?.reconciliation.clone())
    }

    /// Hold ownership until an asynchronous operation and its callbacks are done.
    ///
    /// A permit is not permission to perform unchecked writes later. Its
    /// `with_valid` method must guard each actual write and external dispatch.
    pub fn begin(&self, token: &OwnerToken) -> Result<OperationPermit, Error> {
        self.check_token(token)?;
        self.core.check_admission()?;
        let mut state = self.core.state()?;
        self.core.check_admission()?;
        check_ready(state.phase)?;
        state.active_operations = state.active_operations.checked_add(1).ok_or(Error::Busy)?;
        self.core.publish_status(&state);
        Ok(OperationPermit {
            core: Arc::clone(&self.core),
            generation: self.core.generation,
        })
    }

    /// Permanently retire new admission without waiting for a current write.
    ///
    /// Use this on a UI or platform lifecycle thread. In-flight synchronous
    /// work remains protected by its original gate and lifetime permit until
    /// it settles or rolls back. Storage ownership is not released here.
    /// Close the old component and its databases before `finish_handoff`.
    pub fn request_stop(&self) {
        self.core.request_stop();
    }

    /// Request stop, then synchronously fence the current admitted callback.
    ///
    /// This preserves the original atomic-revocation contract: it waits for
    /// the callback currently inside `with_valid`. Use `request_stop` on UI
    /// threads; use `finish_handoff` after stopping and closing old operations.
    pub fn revoke(&self) {
        let _ = self.core.revoke(false);
    }

    /// Revoke and wait for all permits to drain before releasing the kernel lock.
    ///
    /// A timeout leaves ownership locked and revoked. The caller must keep
    /// shutting down the old host; a replacement cannot acquire it yet.
    pub fn finish_handoff(&self, timeout: Duration) -> Result<(), Error> {
        self.core.revoke(true)?;
        let started = Instant::now();
        let mut state = self
            .core
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        loop {
            if state.active_operations == 0 {
                state.release_if_drained()?;
                self.core.publish_status(&state);
                return Ok(());
            }
            let remaining = timeout.saturating_sub(started.elapsed());
            if remaining.is_zero() {
                return Err(Error::DrainTimeout {
                    active_operations: state.active_operations,
                });
            }
            let (next, _) = self
                .core
                .drained
                .wait_timeout(state, remaining)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            state = next;
        }
    }
}

impl Drop for SessionOwner {
    fn drop(&mut self) {
        let _ = self.core.revoke(true);
    }
}

/// A non-cloneable in-flight operation which keeps ownership until it drains.
pub struct OperationPermit {
    core: Arc<Core>,
    generation: u64,
}

impl OperationPermit {
    /// Generation to carry in the native protocol envelope.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub(crate) fn stop_signal(&self) -> StopSignal {
        StopSignal(Arc::downgrade(&self.core))
    }

    /// Atomically check admission and start a synchronous side effect.
    ///
    /// Synchronous revocation cannot split this check and closure. A fast stop
    /// request retires later admission while already admitted work settles.
    /// The closure should initiate a model call, perform a native edit, or persist one
    /// callback; do not await an asynchronous operation while holding this gate.
    /// Never call owner/permit methods recursively from the closure: they share
    /// this mutex. Every later callback must enter `with_valid` again using the
    /// original permit, so revocation suppresses late writes and follow-up calls.
    pub fn with_valid<T>(&self, operation: impl FnOnce() -> T) -> Result<T, Error> {
        if self.generation != self.core.generation {
            return Err(Error::TokenMismatch);
        }
        self.core.check_admission()?;
        let state = self.core.state()?;
        self.core.check_admission()?;
        check_ready(state.phase)?;
        let result = operation();
        drop(state);
        Ok(result)
    }
}

impl Drop for OperationPermit {
    fn drop(&mut self) {
        let mut state = match self.core.state.lock() {
            Ok(state) => state,
            Err(poisoned) => {
                self.core.request_stop();
                let mut state = poisoned.into_inner();
                state.phase = Phase::Draining;
                state
            }
        };
        debug_assert!(state.active_operations > 0);
        state.active_operations = state.active_operations.saturating_sub(1);
        let _ = state.release_if_drained();
        self.core.publish_status(&state);
        self.core.drained.notify_all();
    }
}

fn check_ready(phase: Phase) -> Result<(), Error> {
    match phase {
        Phase::Ready => Ok(()),
        Phase::RecoveryRequired => Err(Error::RecoveryRequired),
        Phase::Draining | Phase::Released => Err(Error::Revoked),
    }
}

fn issue_generation(file: &mut File, created: bool) -> Result<u64, Error> {
    let length = file
        .metadata()
        .map_err(|error| io_error("read generation journal metadata", error))?
        .len();
    if length % RECORD_SIZE as u64 != 0 || (created && length != 0) || (!created && length == 0) {
        return Err(Error::InvalidJournal);
    }
    file.seek(SeekFrom::Start(0))
        .map_err(|error| io_error("seek generation journal", error))?;
    let mut previous = 0_u64;
    for _ in 0..length / RECORD_SIZE as u64 {
        let mut record = [0_u8; RECORD_SIZE];
        file.read_exact(&mut record)
            .map_err(|error| io_error("read generation journal", error))?;
        let generation =
            u64::from_le_bytes(record[8..16].try_into().expect("fixed-width generation"));
        let complement =
            u64::from_le_bytes(record[16..24].try_into().expect("fixed-width complement"));
        if record[..8] != RECORD_MAGIC
            || generation == 0
            || complement != !generation
            || previous.checked_add(1) != Some(generation)
        {
            return Err(Error::InvalidJournal);
        }
        previous = generation;
    }
    let generation = previous.checked_add(1).ok_or(Error::GenerationExhausted)?;
    let mut record = [0_u8; RECORD_SIZE];
    record[..8].copy_from_slice(&RECORD_MAGIC);
    record[8..16].copy_from_slice(&generation.to_le_bytes());
    record[16..24].copy_from_slice(&(!generation).to_le_bytes());
    file.seek(SeekFrom::End(0))
        .map_err(|error| io_error("append generation journal", error))?;
    file.write_all(&record)
        .map_err(|error| io_error("write new host generation", error))?;
    file.sync_all()
        .map_err(|error| io_error("sync new host generation", error))?;
    Ok(generation)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn drained_handoff_unlocks_while_an_inherited_descriptor_remains_open() {
        let directory = tempfile::tempdir().unwrap();
        let owner = SessionOwner::acquire(directory.path()).unwrap();
        let token = owner.token();
        // A real duplicate of the same open file description, just as an
        // unrelated child holds between fork and exec. Keep it alive across
        // handoff: a close-only release deterministically fails this test.
        let inherited = owner
            .core
            .state
            .lock()
            .unwrap()
            .file
            .as_ref()
            .unwrap()
            .try_clone()
            .unwrap();
        owner.finish_handoff(Duration::ZERO).unwrap();
        assert_eq!(owner.status().unwrap().phase, Phase::Released);
        let next = SessionOwner::acquire(directory.path())
            .expect("Released must permit handoff even before inherited descriptors close");
        assert_eq!(next.token().generation(), token.generation() + 1);
        assert!(matches!(
            owner.with_owned(&token, || ()),
            Err(Error::Revoked)
        ));
        assert!(matches!(next.begin(&token), Err(Error::TokenMismatch)));
        drop(inherited);
        assert!(
            matches!(SessionOwner::acquire(directory.path()), Err(Error::Busy)),
            "closing an old description must not unlock the new owner's different handle"
        );
    }

    #[cfg(unix)]
    #[test]
    fn inherited_descriptor_does_not_release_a_live_operation_before_drain() {
        let directory = tempfile::tempdir().unwrap();
        let owner = SessionOwner::acquire(directory.path()).unwrap();
        owner
            .confirm_recovery(
                &owner.token(),
                Reconciliation {
                    project_identity: "isolated-project".into(),
                    project_revision: "1".into(),
                    session_revision: "1".into(),
                },
                RecoveryDecision::DiscardInterrupted,
            )
            .unwrap();
        let permit = owner.begin(&owner.token()).unwrap();
        let inherited = owner
            .core
            .state
            .lock()
            .unwrap()
            .file
            .as_ref()
            .unwrap()
            .try_clone()
            .unwrap();
        drop(owner);
        assert!(matches!(permit.with_valid(|| ()), Err(Error::Revoked)));
        assert!(matches!(
            SessionOwner::acquire(directory.path()),
            Err(Error::Busy)
        ));
        drop(permit);
        let next = SessionOwner::acquire(directory.path())
            .expect("the final drained permit explicitly releases the retired owner's lock");
        assert_eq!(next.token().generation(), 2);
        drop(inherited);
        assert!(matches!(
            SessionOwner::acquire(directory.path()),
            Err(Error::Busy)
        ));
    }

    #[test]
    fn a_read_only_file_never_issues_a_generation() {
        let root = std::env::temp_dir().join(format!(
            "bk-cat-generation-write-test-{}",
            std::process::id()
        ));
        fs::create_dir_all(&root).expect("test directory");
        let path = root.join("read-only-journal");
        File::create(&path).expect("empty journal");
        let mut file = File::open(&path).expect("read-only journal");
        assert!(matches!(
            issue_generation(&mut file, true),
            Err(Error::Io { .. })
        ));
        assert_eq!(file.metadata().expect("metadata").len(), 0);
        drop(file);
        fs::remove_dir_all(root).expect("remove this test's directory");
    }
}
