// SPDX-License-Identifier: AGPL-3.0-or-later

//! The native, process-local endpoint for the embedded agent runtime.
//!
//! This uses the real MCP implementation and current-window API, with a
//! separate allowlist at the backend boundary. It opens no listening socket,
//! never discovers an external server, and cannot fall back to a headless
//! editing session. Acquiring it does not open Pi storage or start a scheduler.
//! The platform runtime must acquire this endpoint before opening that storage,
//! reconcile it, and obtain a native user decision before admitting writes.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use concat_agent::{OwnerStatus, OwnerToken, Reconciliation, RecoveryDecision, SessionOwner};
use concat_api::{Api, ApiError, AppDirs, ErrorCode, OpenProjects, Request, Response};
use concat_host::export::Exporter;
use concat_mcp::Backend;
use serde_json::{Value, json};

use crate::host::Shell;
use crate::live::Live;

/// One internal protocol endpoint, bound to one persistent-storage owner.
///
/// Only trusted native platform glue may hold this object. Its recovery and
/// lifecycle methods are deliberately absent from the model's tool catalogue.
pub struct AgentBridge {
    owner: Arc<SessionOwner>,
    token: OwnerToken,
    live: Arc<Live>,
    protocol: Mutex<concat_mcp::Session>,
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
        let backend = AgentBackend {
            api,
            owner: Arc::clone(&owner),
            token: token.clone(),
        };
        Ok(Self {
            owner,
            token,
            live,
            protocol: Mutex::new(concat_mcp::Session::embedded(Box::new(backend))),
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
            .map_err(|error| error.to_string())
    }

    /// Revoke first, then permanently cancel this bridge's pending requests.
    /// Calls and callbacks holding another Arc cannot retain execution authority.
    pub fn stop(&self) {
        self.owner.revoke();
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

struct AgentBackend {
    api: Api,
    owner: Arc<SessionOwner>,
    token: OwnerToken,
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
        match self.api.dispatch(request) {
            Response::Result(reply) => {
                serde_json::to_value(reply).map_err(|error| ApiError::failed(error.to_string()))
            }
            Response::Error(error) => Err(error),
        }
    }

    fn describe(&self) -> Value {
        json!({ "mode": "embedded-current-window", "automaticRetry": false })
    }
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
