// SPDX-License-Identifier: AGPL-3.0-or-later

//! The native, process-local endpoint for the embedded agent runtime.
//!
//! This uses the real MCP implementation and current-window API, with a
//! separate allowlist at the backend boundary. It opens no listening socket,
//! never discovers an external server, and cannot fall back to a headless
//! editing session. Acquiring it does not open Pi storage or start a scheduler.
//! The platform runtime must acquire this endpoint before opening that storage,
//! reconcile it, and obtain a native user decision before admitting writes.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use concat_agent::operations::{Admission, Invocation, JournalError, OperationJournal};
use concat_agent::{OwnerStatus, OwnerToken, Reconciliation, RecoveryDecision, SessionOwner};
use concat_api::{Api, ApiError, AppDirs, ErrorCode, OpenProjects, Request, Response};
use concat_host::export::Exporter;
use concat_mcp::Backend;
use serde_json::{Value, json};

use crate::host::Shell;
use crate::live::Live;

pub(crate) mod native;
/// Isolated real-window integration driver, excluded from ordinary builds.
#[cfg(feature = "bk-cat-native-probe")]
pub mod probe;

type JournalSlot = Arc<Mutex<Option<Arc<OperationJournal>>>>;
type InvocationSlot = Arc<Mutex<Option<Invocation>>>;

/// One internal protocol endpoint, bound to one persistent-storage owner.
///
/// Only trusted native platform glue may hold this object. Its recovery and
/// lifecycle methods are deliberately absent from the model's tool catalogue.
pub struct AgentBridge {
    owner: Arc<SessionOwner>,
    token: OwnerToken,
    live: Arc<Live>,
    protocol: Mutex<concat_mcp::Session>,
    journal: JournalSlot,
    invocation: InvocationSlot,
    component_claimed: AtomicBool,
}

impl AgentBridge {
    /// Acquire exclusive storage ownership before any writable Pi adapter opens.
    /// A second instance returns an error and receives no agent endpoint.
    pub fn acquire(dirs: &AppDirs, open: OpenProjects, exporter: Exporter) -> Result<Self, String> {
        let storage = dirs.data.join("bk-cat").join("agent-session");
        std::fs::create_dir_all(&storage)
            .map_err(|error| format!("create agent storage: {error}"))?;
        let owner = Arc::new(SessionOwner::acquire(&storage).map_err(|error| error.to_string())?);
        let token = owner.token();
        let live = Arc::new(Live::new(Arc::clone(&owner), token.clone()));
        let mut api = Api::with_dirs(dirs.clone(), Arc::new(|_| {}));
        api.share_open_projects(open);
        api.share_exporter(exporter);
        api.share_window(Arc::clone(&live) as Arc<dyn concat_api::WindowLink>);
        api.add_capability("edit.guarded");
        let journal = Arc::new(Mutex::new(None));
        let invocation = Arc::new(Mutex::new(None));
        let backend = AgentBackend {
            api,
            owner: Arc::clone(&owner),
            token: token.clone(),
            journal: Arc::clone(&journal),
            invocation: Arc::clone(&invocation),
        };
        Ok(Self {
            owner,
            token,
            live,
            protocol: Mutex::new(concat_mcp::Session::embedded(Box::new(backend))),
            journal,
            invocation,
            component_claimed: AtomicBool::new(false),
        })
    }

    /// The native host's current recovery/admission state, with no authority.
    pub fn status(&self) -> Result<OwnerStatus, String> {
        self.owner.status().map_err(|error| error.to_string())
    }

    /// Resolve one actual MCP frame on a background thread.
    ///
    /// The platform must also validate the local page's origin and original
    /// generation before reaching this entry. It is not a general IPC server.
    pub fn handle(&self, message: Value) -> Result<Option<Value>, String> {
        self.handle_with_invocation(message, None)
    }

    /// The private platform channel supplies original Pi call identity outside
    /// model arguments. The protocol lock scopes it to exactly this one frame.
    pub(crate) fn handle_with_invocation(
        &self,
        message: Value,
        invocation: Option<Invocation>,
    ) -> Result<Option<Value>, String> {
        // A protocol cancellation must never queue behind the mutex held by
        // the native edit it is trying to stop. Retire this whole host bridge;
        // continuing requires native reconciliation and a new bridge, never
        // replaying a cancelled Pi queue. This path also serves queued calls.
        if message.get("jsonrpc") == Some(&json!("2.0"))
            && message.get("method").and_then(Value::as_str) == Some("notifications/cancelled")
            && message.get("id").is_none()
            && message
                .pointer("/params/requestId")
                .is_some_and(|id| id.is_string() || id.is_i64() || id.is_u64())
        {
            self.stop();
            return Ok(None);
        }
        let mut on_ui_thread = false;
        Shell::with(|_, _| on_ui_thread = true);
        if on_ui_thread {
            return Err("the embedded protocol must run off the native editing thread".to_owned());
        }
        self.owner
            .with_owned(&self.token, || ())
            .map_err(|error| error.to_string())?;
        let mut protocol = self
            .protocol
            .lock()
            .map_err(|_| "the embedded protocol failed".to_owned())?;
        *self
            .invocation
            .lock()
            .map_err(|_| "native invocation state failed".to_owned())? = invocation;
        let _scope = InvocationScope(&self.invocation);
        Ok(protocol.handle(message))
    }

    /// Record a real native user decision after storage and project reconciliation.
    ///
    /// The caller must first discard or revalidate the interrupted Pi queue.
    /// This function does not resume it, call a provider, or start any scheduler.
    pub fn confirm_recovery(
        &self,
        state: Reconciliation,
        decision: RecoveryDecision,
    ) -> Result<(), String> {
        self.owner
            .confirm_recovery(&self.token, state, decision)
            .map_err(|error| error.to_string())?;
        let journal = match OperationJournal::open(&self.owner, &self.token) {
            Ok(journal) => Arc::new(journal),
            Err(error) => {
                self.stop();
                return Err(error.to_string());
            }
        };
        *self
            .journal
            .lock()
            .map_err(|_| "native journal state failed".to_owned())? = Some(journal);
        Ok(())
    }

    /// Consume a genuine native Written result. Document matching and hashing
    /// happen over the same file bytes, never over a second later path read.
    pub(crate) fn record_saved(
        &self,
        path: &str,
        session: u64,
        revision: u64,
        expected_document: &Value,
    ) -> Result<u64, String> {
        let journal = self
            .journal
            .lock()
            .map_err(|_| "native journal state failed".to_owned())?
            .clone();
        match journal {
            Some(journal) => journal
                .record_saved_document(path, session, revision, expected_document)
                .map_err(|error| error.to_string()),
            None => Ok(0),
        }
    }

    /// Connections and their permits must close before ownership is released.
    fn close_journal(&self) -> Result<(), String> {
        self.journal
            .lock()
            .map_err(|_| "native journal state failed".to_owned())?
            .take();
        Ok(())
    }

    /// Retire later admission first, then cancel this bridge's queued requests.
    /// This UI-safe request does not wait for an admitted synchronous write;
    /// finish_handoff fences and drains that work before ownership is released.
    pub fn stop(&self) {
        self.owner.request_stop();
        self.live.cancel_pending();
    }

    /// Release the lock only after the runtime and its writable storage close.
    ///
    /// Must run on a background thread while the native event loop can drain its
    /// callbacks. A timeout keeps the revoked owner locked; it never steals it.
    pub fn finish_handoff(&self, timeout: Duration) -> Result<(), String> {
        let mut on_ui_thread = false;
        Shell::with(|_, _| on_ui_thread = true);
        if on_ui_thread {
            return Err("agent handoff must drain on a background thread".to_owned());
        }
        self.stop();
        self.close_journal()?;
        self.owner
            .finish_handoff(timeout)
            .map_err(|error| error.to_string())
    }
}

impl Drop for AgentBridge {
    fn drop(&mut self) {
        self.stop();
    }
}

struct InvocationScope<'a>(&'a Mutex<Option<Invocation>>);

impl Drop for InvocationScope<'_> {
    fn drop(&mut self) {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
    }
}

struct AgentBackend {
    api: Api,
    owner: Arc<SessionOwner>,
    token: OwnerToken,
    journal: JournalSlot,
    invocation: InvocationSlot,
}

impl Backend for AgentBackend {
    fn call(&mut self, request: Request) -> Result<Value, ApiError> {
        self.owner
            .with_owned(&self.token, || ())
            .map_err(|error| ApiError::new(ErrorCode::Refused, error.to_string()))?;
        // Defence in depth: hiding a tool in the catalogue is not admission.
        // ProjectGet cannot open a second session: no open/create path is allowed.
        if !matches!(
            &request,
            Request::Version | Request::ProjectGet { .. } | Request::EditApplyGuarded { .. }
        ) {
            return Err(ApiError::new(
                ErrorCode::Refused,
                "this operation is not available to the embedded agent",
            ));
        }
        if matches!(&request, Request::EditApplyGuarded { .. }) {
            let invocation = self
                .invocation
                .lock()
                .map_err(|_| ApiError::failed("native invocation state failed"))?
                .clone()
                .ok_or_else(|| {
                    ApiError::new(
                        ErrorCode::Refused,
                        "guarded edits require the original Pi invocation context",
                    )
                })?;
            let journal = self
                .journal
                .lock()
                .map_err(|_| ApiError::failed("native journal state failed"))?
                .clone()
                .ok_or_else(|| {
                    ApiError::new(
                        ErrorCode::Refused,
                        "native edit journal is not open after recovery approval",
                    )
                })?;
            let result = journaled_edit(&journal, &invocation, request, |request| {
                dispatch(&mut self.api, request)
            });
            // An unknown native outcome must not lead to another tool or
            // provider call in this generation. Proven zero-change refusals
            // remain normal tool errors that the user can correct.
            if result.as_ref().is_err_and(|error| !proven_no_change(error)) {
                self.owner.revoke();
            }
            return result;
        }
        dispatch(&mut self.api, request)
    }

    fn describe(&self) -> Value {
        json!({ "mode": "embedded-current-window", "automaticRetry": false })
    }
}

fn dispatch(api: &mut Api, request: Request) -> Result<Value, ApiError> {
    match api.dispatch(request) {
        Response::Result(reply) => {
            serde_json::to_value(reply).map_err(|error| ApiError::failed(error.to_string()))
        }
        Response::Error(error) => Err(error),
    }
}

/// The intent is durably committed before dispatch enters the native editor.
/// Unknown or repeated outcomes never turn into a second edit.
fn journaled_edit(
    journal: &OperationJournal,
    invocation: &Invocation,
    request: Request,
    execute: impl FnOnce(Request) -> Result<Value, ApiError>,
) -> Result<Value, ApiError> {
    let Request::EditApplyGuarded {
        path,
        expected_session,
        expected_revision,
        ..
    } = &request
    else {
        return Err(ApiError::new(
            ErrorCode::Refused,
            "the journal accepts only guarded edits",
        ));
    };
    let encoded =
        serde_json::to_value(&request).map_err(|error| ApiError::failed(error.to_string()))?;
    let admission = journal
        .prepare(
            invocation,
            &encoded,
            path,
            *expected_session,
            *expected_revision,
        )
        .map_err(|error| {
            let code = match &error {
                JournalError::NeedsReconciliation { .. } => ErrorCode::Busy,
                JournalError::InvalidInvocation
                | JournalError::ConflictingInvocation
                | JournalError::Owner(_) => ErrorCode::Refused,
                _ => ErrorCode::Failed,
            };
            ApiError::new(code, error.to_string())
        })?;
    let expected = (*expected_session, *expected_revision);
    let operation_id = match admission {
        Admission::New(id) => id,
        Admission::AlreadyApplied { operation_id, .. } => {
            return Err(ApiError::new(
                ErrorCode::Refused,
                format!(
                    "operation {operation_id} was already applied; read the current project before a new plan"
                ),
            ));
        }
        Admission::PreviouslyRejected { error, .. } => {
            return Err(serde_json::from_value(error)
                .map_err(|_| ApiError::failed("native refusal record is invalid"))?);
        }
    };
    let reply = match execute(request) {
        Ok(reply) => reply,
        Err(error) => {
            if proven_no_change(&error) {
                let encoded = serde_json::to_value(&error)
                    .map_err(|failure| ApiError::failed(failure.to_string()))?;
                journal.record_rejected(operation_id, &encoded)
                    .map_err(|failure| ApiError::failed(format!("native edit was refused, but its terminal receipt was not persisted: {failure}")))?;
            }
            return Err(error);
        }
    };
    let session = reply
        .get("sessionId")
        .and_then(Value::as_u64)
        .ok_or_else(|| {
            ApiError::failed("native edit reply has no session identity; reconcile the project")
        })?;
    let revision = reply
        .get("revision")
        .and_then(Value::as_u64)
        .ok_or_else(|| {
            ApiError::failed("native edit reply has no revision; reconcile the project")
        })?;
    if (session, revision) == expected {
        journal
            .record_unchanged(operation_id, session, revision, &reply)
            .map_err(|error| {
                ApiError::failed(format!(
                    "native no-op result could not be persisted: {error}"
                ))
            })?;
        return Ok(reply);
    }
    journal.record_receipt(operation_id, session, revision, &reply)
        .map_err(|error| ApiError::failed(format!("native edit applied but its receipt was not persisted: {error}; reconcile before retry")))?;
    Ok(reply)
}

/// These structured codes are audited on the guarded current-window route:
/// Refused and NotOpen precede mutation, and Cancelled only retires a queued
/// ticket. A running timeout is Busy; a lost/failed result is never terminal.
fn proven_no_change(error: &ApiError) -> bool {
    matches!(
        error.code,
        ErrorCode::Refused | ErrorCode::NotOpen | ErrorCode::Invalid | ErrorCode::Cancelled
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct Scratch(std::path::PathBuf);
    impl Scratch {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "bk-cat-bridge-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir(&path).expect("new isolated test directory");
            Self(path)
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn embedded_endpoint_keeps_one_owner_and_never_approves_recovery() {
        let scratch = Scratch::new();
        let dirs = AppDirs::under(&scratch.0);
        let first = AgentBridge::acquire(&dirs, OpenProjects::default(), Exporter::new()).unwrap();
        assert_eq!(
            first.status().unwrap().phase,
            concat_agent::Phase::RecoveryRequired
        );
        assert!(AgentBridge::acquire(&dirs, OpenProjects::default(), Exporter::new()).is_err());
        let reply = first
            .handle(json!({
                "jsonrpc": "2.0", "id": 1, "method": "initialize",
                "params": { "protocolVersion": "2025-11-25", "capabilities": {},
                            "clientInfo": { "name": "native-isolated-test", "version": "1" } }
            }))
            .unwrap()
            .unwrap();
        assert_eq!(reply["result"]["protocolVersion"], "2025-11-25");
        assert_eq!(
            first.status().unwrap().phase,
            concat_agent::Phase::RecoveryRequired
        );
        first.stop();
        assert!(
            first
                .handle(json!({"jsonrpc":"2.0","id":2,"method":"ping"}))
                .is_err()
        );
        first.finish_handoff(Duration::ZERO).unwrap();
        let replacement =
            AgentBridge::acquire(&dirs, OpenProjects::default(), Exporter::new()).unwrap();
        assert!(replacement.status().unwrap().generation > first.status().unwrap().generation);
        assert_eq!(
            replacement.status().unwrap().phase,
            concat_agent::Phase::RecoveryRequired
        );
    }

    #[test]
    fn protocol_cancellation_does_not_wait_for_the_running_protocol_call() {
        let scratch = Scratch::new();
        let bridge = Arc::new(
            AgentBridge::acquire(
                &AppDirs::under(&scratch.0),
                OpenProjects::default(),
                Exporter::new(),
            )
            .unwrap(),
        );
        let held = bridge.protocol.lock().unwrap();
        let cancelling = Arc::clone(&bridge);
        let (sent, received) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let result = cancelling.handle(json!({ "jsonrpc": "2.0",
                "method": "notifications/cancelled", "params": { "requestId": 9 } }));
            sent.send(result).unwrap();
        });
        let result = received.recv_timeout(Duration::from_secs(2));
        drop(held);
        worker.join().unwrap();
        assert!(
            matches!(result, Ok(Ok(None))),
            "cancel never waits behind protocol dispatch"
        );
        assert_eq!(
            bridge.status().unwrap().phase,
            concat_agent::Phase::Draining
        );
    }
}
