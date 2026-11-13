// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 Jareer Abdullah and Concat contributors

//! The fixed #601 current-project bridge, with native host admission.
//!
//! This adapter belongs to the internal protocol's background thread. The
//! Slint event loop never waits for itself: calls from the installed UI thread
//! are refused. Every request rechecks ownership and the current project in
//! that event loop. Writes additionally require a Ready owner, the expected
//! native session and revision, and Studio's complete current track-lock check.
//!
//! Cancellation and timeout can retire a queued request before its execution
//! starts. An already running synchronous edit is not claimed to be cancelled;
//! a timeout after admission reports an unresolved outcome requiring native
//! reconciliation. It must never trigger an automatic retry.
//!
//! There is no fallback to the unguarded #601 write methods. Until individually
//! audited, old apply/set-video/undo/redo/save calls refuse. In particular,
//! requesting asynchronous autosave is not a durable-save receipt.

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use concat_agent::{OwnerToken, SessionOwner};
use concat_api::{ApiError, ErrorCode, WindowLink};
use concat_host::Session;
use concat_host::session::EditorView;
use concat_project::Command;
use concat_project::model::VideoSettings;

use crate::host::Shell;
use crate::studio::{Studio, same_folder};

const PATIENCE: Duration = Duration::from_secs(10);
const QUEUED: u8 = 0;
const RUNNING: u8 = 1;
const FINISHED: u8 = 2;
const CANCELLED: u8 = 3;

/// Per-request execution admission, shared with the waiting background caller.
struct PendingRequest {
    state: AtomicU8,
}

impl PendingRequest {
    fn new() -> Self {
        Self {
            state: AtomicU8::new(QUEUED),
        }
    }

    fn start(&self) -> bool {
        self.state
            .compare_exchange(QUEUED, RUNNING, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    fn cancel_if_queued(&self) -> bool {
        self.state
            .compare_exchange(QUEUED, CANCELLED, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    fn finish(&self) {
        let _ = self
            .state
            .compare_exchange(RUNNING, FINISHED, Ordering::AcqRel, Ordering::Acquire);
        // A refusal before execution is terminal too. Preserve CANCELLED so
        // a timeout racing its reply never calls a cancelled edit unresolved.
        self.cancel_if_queued();
    }

    fn is_cancelled(&self) -> bool {
        self.state.load(Ordering::Acquire) == CANCELLED
    }

    fn pending(&self) -> bool {
        matches!(self.state.load(Ordering::Acquire), QUEUED | RUNNING)
    }
}

#[derive(Default)]
struct PendingRegistry {
    stopped: bool,
    requests: Vec<Weak<PendingRequest>>,
}

impl PendingRegistry {
    fn register(&mut self) -> Result<Arc<PendingRequest>, ApiError> {
        // Stopping persists across registration. A caller which acquired its
        // owner permit before Stop cannot register a late request afterwards.
        if self.stopped {
            return Err(cancelled());
        }
        let request = Arc::new(PendingRequest::new());
        self.requests
            .retain(|request| request.upgrade().is_some_and(|request| request.pending()));
        self.requests.push(Arc::downgrade(&request));
        Ok(request)
    }

    fn stop(&mut self) -> usize {
        self.stopped = true;
        let mut cancelled = 0;
        self.requests.retain(|request| {
            let Some(request) = request.upgrade() else {
                return false;
            };
            cancelled += usize::from(request.cancel_if_queued());
            request.pending()
        });
        cancelled
    }
}

/// The background protocol adapter to the one project open in the native UI.
pub struct Live {
    owner: Arc<SessionOwner>,
    token: OwnerToken,
    pending: Mutex<PendingRegistry>,
}

impl Live {
    /// Bind this adapter to an existing native owner and its original token.
    ///
    /// This never approves recovery or starts the agent scheduler. Retain this
    /// exact token through callbacks; never replace it with a new host's token.
    pub fn new(owner: Arc<SessionOwner>, token: OwnerToken) -> Self {
        Self {
            owner,
            token,
            pending: Mutex::new(PendingRegistry::default()),
        }
    }

    /// Permanently stop this bridge and retire requests not yet executing.
    ///
    /// Returns the count actually cancelled. A synchronous edit already running
    /// is allowed to settle and must be reconciled; it is not falsely reported
    /// as cancelled or rolled back. Revoking the owner separately rejects every
    /// queued write and later callback in that host generation. A new reviewed
    /// host must construct a new bridge; this object cannot be re-enabled.
    pub fn cancel_pending(&self) -> usize {
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        pending.stop()
    }

    fn register(&self) -> Result<Arc<PendingRequest>, ApiError> {
        let mut pending = self.pending.lock().map_err(|_| {
            ApiError::new(ErrorCode::Refused, "the internal request registry failed")
        })?;
        pending.register()
    }

    fn read<T: Send + 'static>(
        &self,
        path: &str,
        body: impl FnOnce(&Studio) -> Result<T, ApiError> + Send + 'static,
    ) -> Result<T, ApiError> {
        self.on_window(path, false, |studio| body(studio))
    }

    /// No caller on the UI thread may synchronously wait on this adapter.
    fn require_background_thread() -> Result<(), ApiError> {
        let mut on_ui_thread = false;
        Shell::with(|_, _| on_ui_thread = true);
        if on_ui_thread {
            return Err(ApiError::new(
                ErrorCode::Refused,
                "the internal protocol must call the window from its background thread",
            ));
        }
        Ok(())
    }

    fn on_window<T: Send + 'static>(
        &self,
        path: &str,
        publish: bool,
        body: impl FnOnce(&mut Studio) -> Result<T, ApiError> + Send + 'static,
    ) -> Result<T, ApiError> {
        Self::require_background_thread()?;
        let permit = if publish {
            Some(self.owner.begin(&self.token).map_err(ownership_error)?)
        } else {
            self.owner
                .with_owned(&self.token, || ())
                .map_err(ownership_error)?;
            None
        };
        let request = self.register()?;
        let queued_request = Arc::clone(&request);
        let owner = Arc::clone(&self.owner);
        let token = self.token.clone();
        let path = path.to_owned();
        let (reply, answer) = mpsc::channel();

        slint::invoke_from_event_loop(move || {
            let run = || {
                // The CAS happens inside the ownership gate, immediately
                // before the native state is borrowed and checked. A queued
                // timeout/cancel cannot pass this boundary later.
                if !queued_request.start() {
                    return Err(cancelled());
                }
                let mut outcome = None;
                Shell::with(|shell, app| {
                    let result = {
                        let mut studio = shell.studio.borrow_mut();
                        if studio
                            .session
                            .as_ref()
                            .is_some_and(|session| same_folder(session.path(), &path))
                        {
                            body(&mut studio)
                        } else {
                            Err(not_open(&path))
                        }
                    };
                    if publish && result.is_ok() {
                        shell.studio.borrow().publish(&app, &shell.models);
                    }
                    outcome = Some(result);
                });
                outcome.unwrap_or_else(|| Err(closing()))
            };

            let result = match permit {
                Some(permit) => permit.with_valid(run).map_err(ownership_error),
                None => owner.with_owned(&token, run).map_err(ownership_error),
            }
            .and_then(|result| result);
            queued_request.finish();
            // A dropped caller receives nothing. It cannot reactivate or
            // retry this request, whose ticket is already terminal.
            let _ = reply.send(result);
        })
        .map_err(|_| {
            request.cancel_if_queued();
            closing()
        })?;

        match answer.recv_timeout(PATIENCE) {
            Ok(result) => result,
            Err(RecvTimeoutError::Timeout) => {
                if request.cancel_if_queued() || request.is_cancelled() {
                    Err(ApiError::new(
                        ErrorCode::Cancelled,
                        "the queued window request timed out before execution and was cancelled",
                    ))
                } else {
                    Err(ApiError::new(
                        ErrorCode::Busy,
                        "the window request crossed its execution boundary but no reply arrived; reconcile the current native project before any retry",
                    ))
                }
            }
            Err(RecvTimeoutError::Disconnected) => {
                request.cancel_if_queued();
                Err(closing())
            }
        }
    }
}

impl WindowLink for Live {
    fn live(&self) -> bool {
        self.owner.with_owned(&self.token, || ()).is_ok()
    }

    fn view(&self, path: &str) -> Result<EditorView, ApiError> {
        self.read(path, |studio| Ok(session(studio)?.view()))
    }

    fn snapshot(&self, path: &str) -> Result<Session, ApiError> {
        self.read(path, |studio| Ok(session(studio)?.snapshot()))
    }

    fn apply_guarded(
        &self,
        path: &str,
        expected_session: u64,
        expected_revision: u64,
        command: Command,
    ) -> Result<EditorView, ApiError> {
        self.on_window(path, true, move |studio| {
            studio
                .apply_agent(command, expected_session, expected_revision)
                .map_err(|message| ApiError::new(ErrorCode::Refused, message))
        })
    }

    fn apply(&self, _path: &str, _command: Command) -> Result<EditorView, ApiError> {
        Err(guarded_only("edit.apply"))
    }

    fn set_video(&self, _path: &str, _video: VideoSettings) -> Result<EditorView, ApiError> {
        Err(guarded_only("project.setVideo"))
    }

    fn undo(&self, _path: &str) -> Result<EditorView, ApiError> {
        Err(guarded_only("edit.undo"))
    }

    fn redo(&self, _path: &str) -> Result<EditorView, ApiError> {
        Err(guarded_only("edit.redo"))
    }

    fn save(&self, _path: &str) -> Result<(), ApiError> {
        Err(ApiError::new(
            ErrorCode::Refused,
            "agent save requires a session/revision-bound durable save receipt; requesting autosave is not a saved result",
        ))
    }
}

fn session(studio: &Studio) -> Result<&Session, ApiError> {
    studio
        .session
        .as_ref()
        .ok_or_else(|| ApiError::new(ErrorCode::NotOpen, "the window has no project open"))
}

fn guarded_only(method: &str) -> ApiError {
    ApiError::new(
        ErrorCode::Refused,
        format!(
            "{method} is refused for the embedded agent; use the reviewed session/revision-bound edit path"
        ),
    )
}

fn ownership_error(error: concat_agent::Error) -> ApiError {
    ApiError::new(
        ErrorCode::Refused,
        format!("agent host admission refused: {error}"),
    )
}

fn not_open(path: &str) -> ApiError {
    ApiError::new(
        ErrorCode::NotOpen,
        format!("{path} is no longer the project open in the window"),
    )
}

fn cancelled() -> ApiError {
    ApiError::new(
        ErrorCode::Cancelled,
        "the queued native request was cancelled before execution",
    )
}

fn closing() -> ApiError {
    ApiError::failed("the window is closing; reconcile any request whose outcome was not received")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stop_before_registration_refuses_the_late_request() {
        let mut registry = PendingRegistry::default();
        assert_eq!(registry.stop(), 0);
        assert!(registry.register().is_err());
        assert!(
            registry.register().is_err(),
            "Stop remains in force for this bridge"
        );
    }

    #[test]
    fn stop_after_registration_cancels_and_closes_admission() {
        let mut registry = PendingRegistry::default();
        let request = registry.register().expect("registered");
        assert_eq!(registry.stop(), 1);
        assert!(!request.start());
        assert!(registry.register().is_err());
    }

    #[test]
    fn queued_timeout_or_stop_never_executes_later() {
        let request = PendingRequest::new();
        assert!(request.cancel_if_queued());
        assert!(!request.start());
        assert!(!request.start());
        assert!(!request.pending());
        request.finish();
        assert!(
            request.is_cancelled(),
            "finishing a cancelled callback preserves its no-effect outcome"
        );
    }

    #[test]
    fn a_running_native_edit_is_not_falsely_cancelled() {
        let request = PendingRequest::new();
        assert!(request.start());
        assert!(!request.cancel_if_queued());
        assert!(request.pending());
        request.finish();
        assert!(!request.start());
        assert!(!request.pending());
    }

    #[test]
    fn racing_admission_and_cancellation_have_exactly_one_winner() {
        let request = Arc::new(PendingRequest::new());
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let cancellation = Arc::clone(&request);
        let cancellation_barrier = Arc::clone(&barrier);
        let worker = std::thread::spawn(move || {
            cancellation_barrier.wait();
            cancellation.cancel_if_queued()
        });
        barrier.wait();
        let started = request.start();
        let cancelled = worker.join().expect("cancellation worker");
        assert_ne!(started, cancelled);
        assert!(!request.start(), "the request cannot execute twice");
    }
}
