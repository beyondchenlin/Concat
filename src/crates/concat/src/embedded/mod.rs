// SPDX-License-Identifier: AGPL-3.0-or-later

//! The native component owner and platform mailbox. This is a transport and
//! lifecycle boundary; the original Pi runtime remains the only scheduler.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, SyncSender};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use concat_agent::configuration::ConfigurationSnapshot;
use concat_agent::transport::{
    ByteBudget, ByteReservation, MAX_BRIDGE_FRAME_BYTES, MAX_QUEUED_FRAME_BYTES,
};
use concat_api::AppDirs;
use serde_json::{Value, json};
use slint::ComponentHandle;

use crate::agent::AgentBridge;
use crate::agent::native::{NativeSession, STORAGE_ID};
use crate::agent::tasks::{SubmissionError, SubmissionIntent, TaskMode};
use crate::host::Shell;

mod resources;
pub use resources::RuntimeResources;
pub(crate) use resources::random_capability;
#[cfg(target_os = "windows")]
mod windows;

/// Sole origin of the compiled runtime resource bundle.
pub const RUNTIME_ORIGIN: &str = "https://bk-cat.invalid";
/// Sole navigation permitted for a native runtime component.
pub const RUNTIME_URL: &str = "https://bk-cat.invalid/index.html";
const CHANNEL: &str = "bk-cat-runtime-v1";
const MAX_FRAME_BYTES: usize = MAX_BRIDGE_FRAME_BYTES;
// Human command admission keeps its existing smaller bound; only persisted
// history/network/database frames need the larger transport envelope.
const MAX_COMMAND_FRAME_BYTES: usize = 2 * 1024 * 1024;

#[derive(Debug)]
pub(crate) enum FrameEncodingError {
    TooLarge,
    Serialization(serde_json::Error),
}

impl std::fmt::Display for FrameEncodingError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooLarge => formatter.write_str("native response exceeds frame limit"),
            Self::Serialization(error) => std::fmt::Display::fmt(error, formatter),
        }
    }
}

impl From<FrameEncodingError> for SubmissionError {
    fn from(error: FrameEncodingError) -> Self {
        match error {
            FrameEncodingError::TooLarge => Self::RejectedInput("runtime.inputTooLarge"),
            FrameEncodingError::Serialization(error) => Self::Failed(error.to_string()),
        }
    }
}

/// Use the real envelope for every bound check, including JSON escapes and
/// the component/command identifiers. Admitted commands send these same bytes.
pub(crate) fn encode_frame(
    component: &str,
    mut envelope: Value,
) -> Result<String, FrameEncodingError> {
    envelope["channel"] = json!(CHANNEL);
    envelope["component"] = json!(component);
    let serialized = serde_json::to_string(&envelope).map_err(FrameEncodingError::Serialization)?;
    if serialized.len() > MAX_FRAME_BYTES {
        return Err(FrameEncodingError::TooLarge);
    }
    Ok(serialized)
}

struct EncodedCommand {
    id: String,
    serialized: String,
}

// Covers queued platform creation/focus and loading the controlled scripts. This is
// independent of the device smoke's unchanged 60-second production observation.
const LOAD_TIMEOUT: Duration = Duration::from_secs(30);
const LOAD_TIMEOUT_MESSAGE: &str = "runtime.loadTimeout";
const SEND_WAIT_TIMEOUT_MESSAGE: &str = "runtime.waitTimeout";

fn load_failure_message(code: &str) -> Option<&'static str> {
    match code {
        "web-platform" | "weak-references" | "secure-random" | "cancellation"
        | "collection-methods" | "promise-resolvers" | "structured-clone" => {
            Some("runtime.webCapability")
        }
        "script-resource" => Some("runtime.scriptResource"),
        "script-execution" | "runtime-missing" | "bootstrap-missing" => {
            Some("runtime.scriptExecution")
        }
        _ => None,
    }
}

/// An invisible platform runtime. These calls must never wait on a UI thread.
pub trait PlatformHost: Send + Sync {
    /// Queue one serialized native frame without waiting for the webpage.
    fn post_message(&self, message: &str) -> Result<(), String>;
    /// Stop the platform asynchronously; closed fires only after destruction.
    fn close(&self);
}

/// stopping revokes before destruction; closed proves the old component is
/// destroyed and allows native databases to close and ownership to drain.
pub struct RuntimeCallbacks {
    /// A frame from the platform-verified local document.
    pub message: Arc<dyn Fn(String) + Send + Sync>,
    /// Synchronous revocation notification before component destruction.
    pub stopping: Arc<dyn Fn(String) + Send + Sync>,
    /// Destruction proof; only now may the native owner finish handoff.
    pub closed: Arc<dyn Fn(String) + Send + Sync>,
}

type Factory = dyn Fn(Arc<RuntimeResources>, RuntimeCallbacks) -> Result<Box<dyn PlatformHost>, String>
    + Send
    + Sync;
static FACTORY: OnceLock<Box<Factory>> = OnceLock::new();
// Android updates this before retiring the platform and on actual Activity
// resume. A queued UI callback must re-read it instead of trusting an old event.
static PLATFORM_FOREGROUND: AtomicBool = AtomicBool::new(true);
thread_local! {
    // One visible assistant's connection intent, bound to the current native
    // project. It waits for actual close/release, never for a polling timer.
    static ASSISTANT_CONNECTION_REQUEST: std::cell::Cell<Option<u64>> = const {
        std::cell::Cell::new(None)
    };
    // Visibility is not user intent: the desktop rail starts expanded and
    // startup settings inspection publishes without the user opening it.
    // Stop/project changes clear this marker before any queued event runs.
    static ASSISTANT_ACTIVE_SESSION: std::cell::Cell<Option<u64>> = const {
        std::cell::Cell::new(None)
    };
}

// A private construction seam: failure must happen before a native component
// claims this generation. Production always uses the real OS thread builder.
type MailboxJob = Box<dyn FnOnce() + Send + 'static>;

fn spawn_mailbox(job: MailboxJob) -> std::io::Result<()> {
    std::thread::Builder::new()
        .name("bk-cat-native-mailbox".into())
        .spawn(job)
        .map(|_| ())
}

/// Installed once by the Android native entry point before the editor starts.
pub fn install_factory(
    factory: impl Fn(Arc<RuntimeResources>, RuntimeCallbacks) -> Result<Box<dyn PlatformHost>, String>
    + Send
    + Sync
    + 'static,
) -> Result<(), String> {
    FACTORY
        .set(Box::new(factory))
        .map_err(|_| "runtime factory already installed".into())
}

fn spawn_platform(
    resources: Arc<RuntimeResources>,
    callbacks: RuntimeCallbacks,
) -> Result<Box<dyn PlatformHost>, String> {
    if let Some(factory) = FACTORY.get() {
        return factory(resources, callbacks);
    }
    #[cfg(target_os = "windows")]
    {
        windows::spawn(
            resources,
            callbacks.message,
            callbacks.stopping,
            callbacks.closed,
        )
        .map(|host| Box::new(host) as Box<dyn PlatformHost>)
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = (resources, callbacks);
        Err("runtime.unsupportedPlatform".into())
    }
}

#[cfg(target_os = "windows")]
impl PlatformHost for windows::WindowsRuntime {
    fn post_message(&self, message: &str) -> Result<(), String> {
        windows::WindowsRuntime::post_message(self, message)
    }
    fn close(&self) {
        windows::WindowsRuntime::close(self);
    }
}

#[derive(Clone, Default)]
pub(crate) struct RuntimeView {
    pub loaded: bool,
    pub phase: String,
    pub snapshot: Option<Value>,
    pub error: String,
    pub closed: bool,
    pub configuration: Option<Value>,
    pub presentation: Option<Value>,
    pub task_running: bool,
    pub task_title: String,
    /// An explicit send is retained locally; no model task or grant exists yet.
    pub waiting_for_ready: bool,
}

fn clean_startup_snapshot(snapshot: &Value) -> bool {
    snapshot["snapshotId"]
        .as_str()
        .is_some_and(|id| !id.is_empty())
        && snapshot["projectPath"]
            .as_str()
            .is_some_and(|path| !path.is_empty())
        && snapshot["hasInterruptedWork"] == false
        && snapshot.get("blockedReason") == Some(&Value::Null)
}

impl RuntimeView {
    pub(crate) fn may_wait_for_readiness(&self) -> bool {
        // Recovery and activation require actual clean native evidence.
        // Only earlier startup phases may still await their first snapshot.
        !self.closed
            && self.error.is_empty()
            && !self.task_running
            && match self.phase.as_str() {
                "loading" | "new" | "acquiring" => {
                    self.snapshot.as_ref().is_none_or(clean_startup_snapshot)
                }
                "recovery" | "activating" => {
                    self.snapshot.as_ref().is_some_and(clean_startup_snapshot)
                }
                _ => false,
            }
    }
}

fn finish_task_view(
    view: &mut RuntimeView,
    successful: bool,
    edit_without_changes: bool,
    native_refusal: Option<String>,
) -> Result<(), String> {
    view.task_running = false;
    view.task_title.clear();
    // A terminal model receipt cannot erase a native/runtime failure already
    // recorded for this task. queue_submission clears errors for a new task.
    if !view.error.is_empty() {
        return Err(view.error.clone());
    }
    if edit_without_changes {
        if let Some(reason) = native_refusal {
            // A native preflight rejection or human refusal/expiry, before
            // any execution permit was consumed, is a completed zero-write
            // outcome. Keep its reason visible without blocking new input.
            view.task_title = reason;
        } else {
            return Err("runtime.editUnverified".into());
        }
    } else if !successful {
        return Err("runtime.taskIncomplete".into());
    }
    Ok(())
}

#[derive(Clone, Copy)]
enum CommandKind {
    Start,
    Inspect,
    Confirm,
    Submit,
    Observe,
}
enum Work {
    Frame {
        envelope: Value,
        bytes: ByteReservation,
    },
    ApprovalFinished {
        id: String,
        result: Result<Value, String>,
    },
    SavedScan,
    Submit,
    Waiting,
    Closed,
}

/// Local user intent only. The snapshot contains no secret and grants no access.
struct WaitingSubmission {
    intent: SubmissionIntent,
    configuration: ConfigurationSnapshot,
    deadline: Instant,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct AcceptedSubmission {
    task: String,
    conversation: u64,
    submission: u64,
}

impl AcceptedSubmission {
    fn parse(value: &Value) -> Result<Self, String> {
        let integer = |field: &str| {
            value[field]
                .as_u64()
                .filter(|id| *id <= 9_007_199_254_740_991)
                .ok_or("runtime.protocolViolation".to_owned())
        };
        Ok(Self {
            task: value["taskId"]
                .as_str()
                .filter(|id| !id.is_empty() && id.len() <= 256)
                .ok_or("runtime.protocolViolation")?
                .to_owned(),
            conversation: integer("conversationId")?,
            submission: integer("submissionId")?,
        })
    }
}

/// One web component, one already-owned native session. Queues carry messages,
/// never model jobs. There is no automatic retry or recovery approval here.
pub(crate) struct EmbeddedRuntime {
    component: String,
    bridge: Arc<AgentBridge>,
    native: NativeSession,
    platform: Mutex<Option<Box<dyn PlatformHost>>>,
    incoming: SyncSender<Work>,
    incoming_bytes: ByteBudget,
    view: Mutex<RuntimeView>,
    pending: Mutex<BTreeMap<String, CommandKind>>,
    seen: Mutex<BTreeSet<String>>,
    next: AtomicU64,
    revoked: Arc<AtomicBool>,
    foreground_reconnect: AtomicBool,
    platform_closed: AtomicBool,
    scan_queued: AtomicBool,
    waiting: Mutex<Option<WaitingSubmission>>,
    submission: Mutex<Option<SubmissionIntent>>,
    observed_conversation: Mutex<Option<u64>>,
    accepted_submission: Mutex<Option<AcceptedSubmission>>,
    approval_worker: Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl EmbeddedRuntime {
    pub(crate) fn start(dirs: &AppDirs, bridge: Arc<AgentBridge>) -> Result<Arc<Self>, String> {
        let component = random_capability()?;
        let resources = Arc::new(RuntimeResources::bundled(dirs, &component)?);
        Self::start_with(bridge, component, resources, spawn_platform)
    }

    fn start_with(
        bridge: Arc<AgentBridge>,
        component: String,
        resources: Arc<RuntimeResources>,
        factory: impl FnOnce(
            Arc<RuntimeResources>,
            RuntimeCallbacks,
        ) -> Result<Box<dyn PlatformHost>, String>,
    ) -> Result<Arc<Self>, String> {
        Self::start_with_spawner(
            bridge,
            component,
            resources,
            factory,
            spawn_mailbox,
            LOAD_TIMEOUT,
        )
    }

    fn start_with_spawner(
        bridge: Arc<AgentBridge>,
        component: String,
        resources: Arc<RuntimeResources>,
        factory: impl FnOnce(
            Arc<RuntimeResources>,
            RuntimeCallbacks,
        ) -> Result<Box<dyn PlatformHost>, String>,
        spawn: impl FnOnce(MailboxJob) -> std::io::Result<()>,
        load_timeout: Duration,
    ) -> Result<Arc<Self>, String> {
        let (incoming, receiver) = mpsc::sync_channel(256);
        // First establish the cleanup worker. If thread creation fails, no
        // component has claimed the bridge and no platform/database exists;
        // the same recovery-only bridge can be retried without revocation.
        // A constructor error drops this sender and lets the worker exit.
        // The sole successful send occurs after platform creation/installation,
        // so mailbox dispatch and replies cannot race an absent platform handle.
        let (platform_ready, ready) = mpsc::channel::<std::sync::Weak<Self>>();
        spawn(Box::new(move || {
            let Ok(weak) = ready.recv() else {
                return;
            };
            let loading_deadline = Instant::now() + load_timeout;
            let mut awaiting_load = true;
            loop {
                // A send has its own deadline even after runtime.loaded. Read
                // the slot briefly; never retain an owner while blocking here.
                let waiting_deadline = weak
                    .upgrade()
                    .and_then(|runtime| runtime.waiting_deadline());
                let deadline = match (awaiting_load, waiting_deadline) {
                    (true, Some(waiting)) => Some(loading_deadline.min(waiting)),
                    (true, None) => Some(loading_deadline),
                    (false, waiting) => waiting,
                };
                let received = if let Some(deadline) = deadline {
                    receiver.recv_timeout(deadline.saturating_duration_since(Instant::now()))
                } else {
                    receiver
                        .recv()
                        .map_err(|_| mpsc::RecvTimeoutError::Disconnected)
                };
                let Some(runtime) = weak.upgrade() else {
                    break;
                };
                if runtime.platform_closed.load(Ordering::Acquire) {
                    // Revocation wakes the human waiter. Retain ownership until
                    // it has actually exited as well as the platform closing.
                    runtime.native.tool_approval.cancel();
                    let approval_result = runtime.join_approval_worker();
                    let shutdown_result = runtime.native.shutdown();
                    let result = approval_result.and(shutdown_result);
                    if result.is_err() {
                        log::error!("bk-cat-agent: state=failed");
                    }
                    log::info!("bk-cat-agent: state=closed");
                    if let Ok(mut view) = runtime.view.lock() {
                        finish_runtime_view(&mut view, result);
                    }
                    runtime.publish();
                    break;
                }
                if runtime.revoked.load(Ordering::Acquire) {
                    // Stop waiting for load, but keep the owner until the real
                    // platform destruction callback wakes this worker.
                    awaiting_load = false;
                    continue;
                }
                let work = match received {
                    Ok(work) => work,
                    Err(mpsc::RecvTimeoutError::Timeout) => {
                        if runtime
                            .waiting_deadline()
                            .is_some_and(|deadline| Instant::now() >= deadline)
                        {
                            runtime.fail_live_work(SEND_WAIT_TIMEOUT_MESSAGE);
                        } else if awaiting_load {
                            awaiting_load = false;
                            runtime.fail_live_work(LOAD_TIMEOUT_MESSAGE);
                        }
                        continue;
                    }
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                };
                if awaiting_load && Instant::now() >= loading_deadline {
                    awaiting_load = false;
                    runtime.fail_live_work(LOAD_TIMEOUT_MESSAGE);
                    continue;
                }
                if runtime
                    .waiting_deadline()
                    .is_some_and(|deadline| Instant::now() >= deadline)
                {
                    runtime.fail_live_work(SEND_WAIT_TIMEOUT_MESSAGE);
                    continue;
                }
                match work {
                    Work::Frame { envelope, bytes } => {
                        runtime.handle(envelope);
                        drop(bytes);
                    }
                    Work::ApprovalFinished { id, result } => {
                        // Reap BEFORE publishing the result. A subsequent tool
                        // cannot race the previous thread's final instructions.
                        match runtime.join_approval_worker() {
                            Ok(()) => runtime.reply_native(&id, result),
                            Err(error) => runtime.fail_terminal(&error),
                        }
                    }
                    Work::Submit => runtime.prepare_submission(),
                    Work::Waiting => runtime.publish(),
                    Work::SavedScan => {
                        runtime.scan_queued.store(false, Ordering::Release);
                        match runtime.native.take_saved_notifications() {
                            Ok(notifications) => {
                                for mut notification in notifications {
                                    notification["event"] = json!("operation.saved");
                                    if let Err(error) = runtime.post(notification) {
                                        runtime.fail_live_work(&error);
                                        break;
                                    }
                                }
                            }
                            Err(error) => runtime.fail_live_work(&error),
                        }
                    }
                    Work::Closed => {}
                }
                if runtime.view().loaded {
                    awaiting_load = false;
                }
            }
        }))
        .map_err(|error| format!("could not start native mailbox: {error}"))?;
        let native = NativeSession::new(Arc::clone(&bridge), component.clone())?;
        let runtime = Arc::new(Self {
            component,
            bridge,
            native,
            platform: Mutex::new(None),
            incoming,
            incoming_bytes: ByteBudget::new(MAX_QUEUED_FRAME_BYTES),
            view: Mutex::new(RuntimeView {
                phase: "loading".into(),
                ..RuntimeView::default()
            }),
            pending: Mutex::new(BTreeMap::new()),
            seen: Mutex::new(BTreeSet::new()),
            next: AtomicU64::new(1),
            revoked: Arc::new(AtomicBool::new(false)),
            foreground_reconnect: AtomicBool::new(false),
            platform_closed: AtomicBool::new(false),
            scan_queued: AtomicBool::new(false),
            waiting: Mutex::new(None),
            submission: Mutex::new(None),
            observed_conversation: Mutex::new(None),
            accepted_submission: Mutex::new(None),
            approval_worker: Mutex::new(None),
        });
        let weak_message = Arc::downgrade(&runtime);
        let weak_stopping = Arc::downgrade(&runtime);
        let weak_closed = Arc::downgrade(&runtime);
        let callbacks = RuntimeCallbacks {
            message: Arc::new(move |raw| {
                if let Some(runtime) = weak_message.upgrade() {
                    runtime.receive(raw);
                }
            }),
            stopping: Arc::new(move |reason| {
                if let Some(runtime) = weak_stopping.upgrade() {
                    runtime.revoke(&reason);
                }
            }),
            closed: Arc::new(move |reason| {
                if let Some(runtime) = weak_closed.upgrade() {
                    runtime.revoke(&reason);
                    runtime.remember_terminal_failure(&reason);
                    runtime.platform_closed.store(true, Ordering::Release);
                    // A full queue already wakes the worker, which checks the
                    // close flag before consuming any more privileged work.
                    let _ = runtime.incoming.try_send(Work::Closed);
                }
            }),
        };
        match factory(resources, callbacks) {
            Ok(host) => {
                *runtime
                    .platform
                    .lock()
                    .map_err(|_| "runtime platform poisoned")? = Some(host)
            }
            Err(error) => {
                runtime.revoke(&error);
                runtime.platform_closed.store(true, Ordering::Release);
                let _ = runtime.incoming.try_send(Work::Closed);
            }
        }
        let _ = platform_ready.send(Arc::downgrade(&runtime));
        Ok(runtime)
    }

    pub(crate) fn component(&self) -> &str {
        &self.component
    }

    pub(crate) fn view(&self) -> RuntimeView {
        self.view
            .lock()
            .map(|view| view.clone())
            .unwrap_or_else(|_| RuntimeView {
                error: "agent.sessionUnavailable".into(),
                ..RuntimeView::default()
            })
    }

    /// Read a tiny readiness snapshot without cloning the persisted conversation.
    /// A busy worker must never stall the editor's rendering thread.
    pub(crate) fn try_ready(&self) -> Option<bool> {
        match self.view.try_lock() {
            Ok(view) => Some(view.phase == "ready" && !view.closed),
            Err(std::sync::TryLockError::WouldBlock) => None,
            Err(std::sync::TryLockError::Poisoned(_)) => Some(false),
        }
    }

    fn receive(&self, raw: String) {
        if self.revoked.load(Ordering::Acquire) {
            return;
        }
        let envelope = match authenticate(&raw, &self.component) {
            Ok(envelope) => envelope,
            Err(_) => {
                self.fail_live_work("runtime.protocolViolation");
                return;
            }
        };
        // Cancellation must not wait behind a synchronous edit/SQLite request.
        let method = envelope.pointer("/payload/method").and_then(Value::as_str);
        let protocol_cancel = method == Some("mcp.send")
            && envelope
                .pointer("/payload/message/jsonrpc")
                .and_then(Value::as_str)
                == Some("2.0")
            && envelope
                .pointer("/payload/message/method")
                .and_then(Value::as_str)
                == Some("notifications/cancelled");
        if matches!(method, Some("request.cancel" | "ownership.revoke")) || protocol_cancel {
            self.stop();
            return;
        }
        if method == Some("network.cancel") {
            let result = self.native.cancel_network_frame(&envelope["payload"]);
            let reply = match result {
                Ok(value) => json!({"id":envelope["id"],"result":value}),
                Err(message) => {
                    json!({"id":envelope["id"],"error":{"code":"refused","message":message}})
                }
            };
            if let Err(error) = self.post(reply) {
                self.fail_live_work(&error);
            }
            return;
        }
        let Some(bytes) = self.incoming_bytes.reserve(raw.len()) else {
            self.fail_live_work("runtime.queueFull");
            return;
        };
        if self
            .incoming
            .try_send(Work::Frame { envelope, bytes })
            .is_err()
        {
            self.fail_live_work("runtime.queueFull");
        }
    }

    fn handle(self: &Arc<Self>, envelope: Value) {
        self.handle_with(envelope, || {});
    }

    fn handle_with(self: &Arc<Self>, envelope: Value, before_command_result: impl FnOnce()) {
        if let Some(event) = envelope.get("event").and_then(Value::as_str) {
            match event {
                "runtime.loadFailed" => {
                    let message = envelope["code"]
                        .as_str()
                        .and_then(load_failure_message)
                        .unwrap_or("runtime.protocolViolation");
                    self.fail_live_work(message);
                    return;
                }
                "runtime.loaded" => {
                    let first = self.view.lock().is_ok_and(|mut view| {
                        if view.loaded {
                            return false;
                        }
                        view.loaded = true;
                        view.phase = "acquiring".into();
                        true
                    });
                    if !first {
                        self.fail_live_work("runtime.protocolViolation");
                        return;
                    }
                    let start = self.native.tasks.model_configuration().and_then(|configuration| {
                        if let Ok(mut view) = self.view.lock() { view.configuration = Some(configuration.clone()); }
                        self.command(json!({"method":"runtime.start","storageId":STORAGE_ID,"modelConfiguration":configuration}), CommandKind::Start)
                    });
                    if let Err(error) = start {
                        self.fail_live_work(&error);
                    }
                }
                "runtime.phase" => {
                    if let Some(phase) = envelope["phase"].as_str().filter(|phase| {
                        [
                            "new",
                            "acquiring",
                            "recovery",
                            "activating",
                            "ready",
                            "stopping",
                            "closed",
                        ]
                        .contains(phase)
                    }) && let Ok(mut view) = self.view.lock()
                    {
                        view.phase = phase.to_owned();
                    }
                }
                "runtime.failed" => {
                    // The bundled runtime reports this before revoking an
                    // activation that cannot finish. Otherwise its cleanup
                    // closes WebView2 before the original error reaches UI.
                    let stage = envelope["stage"].as_str();
                    let message = envelope["message"].as_str();
                    if !matches!(stage, Some("acquire" | "activate"))
                        || message.is_none_or(|message| {
                            message.is_empty()
                                || message.len() > 1024
                                || message.chars().any(char::is_control)
                        })
                    {
                        self.fail_live_work("runtime.protocolViolation");
                        return;
                    }
                    let stage = stage.expect("checked above");
                    let message = message.expect("checked above");
                    log::error!("bk-cat-agent: startup_failed stage={stage} reason={message}");
                    self.fail_live_work(&format!("runtime.startupFailed\n{message}"));
                    return;
                }
                "conversation.presentation" => {
                    if let Err(error) = self.accept_presentation(&envelope) {
                        self.fail_live_work(&error);
                        return;
                    }
                }
                "runtime.reviewed" => {
                    if !self.native.reviewed_current_project() {
                        self.fail_live_work("agent.errorRecoveryChanged");
                        return;
                    }
                    if let Ok(mut view) = self.view.lock() {
                        view.task_running = false;
                        view.waiting_for_ready = false;
                        view.task_title = "agent.recoveryReviewedNotice".into();
                    }
                }
                "runtime.resumed" => {
                    if let Err(error) = self.accept_recovered_submission(&envelope) {
                        self.fail_live_work(&error);
                        return;
                    }
                }
                "task.completed" => {
                    self.complete_submission(&envelope);
                    return;
                }
                "operation.failed" => {
                    self.fail_live_work(
                        envelope["message"]
                            .as_str()
                            .unwrap_or("runtime.saveNotificationUnconfirmed"),
                    );
                    return;
                }
                _ => {
                    self.fail_live_work("runtime.protocolViolation");
                    return;
                }
            }
            self.publish();
            if event == "runtime.phase" && envelope["phase"] == "recovery" {
                let view = self.view();
                let snapshot_returned = view.snapshot.as_ref().is_some_and(|snapshot| {
                    snapshot["snapshotId"]
                        .as_str()
                        .is_some_and(|id| !id.is_empty())
                });
                // Production startup evidence: the fixed local page has loaded,
                // its native snapshot round trip finished, and this generation
                // remains recovery-only. No token, credential or project path.
                if view.loaded
                    && !view.closed
                    && snapshot_returned
                    && !self.revoked.load(Ordering::Acquire)
                    && let Ok(owner) = self.bridge.status()
                    && owner.phase == concat_agent::Phase::RecoveryRequired
                {
                    log::info!(
                        "bk-cat-agent: state=recovery generation={}",
                        owner.generation
                    );
                }
            }
            if event == "runtime.phase" && envelope["phase"] == "closed" {
                self.stop();
            }
            return;
        }
        let Some(id) = envelope["id"]
            .as_str()
            .filter(|id| !id.is_empty() && id.len() <= 256)
        else {
            self.fail_live_work("runtime.protocolViolation");
            return;
        };
        if envelope.get("payload").is_some() {
            let fresh = self
                .seen
                .lock()
                .is_ok_and(|mut seen| seen.insert(id.to_owned()));
            if fresh
                && envelope.pointer("/payload/method").and_then(Value::as_str)
                    == Some("mcp.authorize")
            {
                self.start_approval_worker(envelope);
                return;
            }
            let result = if fresh {
                self.native.handle(envelope["payload"].clone())
            } else {
                Err("native request was replayed".into())
            };
            if envelope
                .pointer("/payload/method")
                .and_then(Value::as_str)
                .is_some_and(|method| method.starts_with("network."))
                && let Err(message) = &result
            {
                self.record_live_error(message);
            }
            self.reply_native(id, result);
            return;
        }
        let kind = self
            .pending
            .lock()
            .ok()
            .and_then(|mut pending| pending.remove(id));
        let Some(kind) = kind else {
            self.fail_live_work("runtime.protocolViolation");
            return;
        };
        before_command_result();
        if envelope.get("error").is_some() {
            let message = envelope
                .pointer("/error/message")
                .and_then(Value::as_str)
                .unwrap_or("agent.errorUnexpected");
            if matches!(kind, CommandKind::Submit) {
                self.fail_live_work("runtime.submissionUnconfirmed");
                return;
            }
            if matches!(kind, CommandKind::Start | CommandKind::Observe) {
                // Early startup and idle history reads have no later task
                // terminal callback to repair disabled input. Recovery
                // inspection/confirmation refusals remain retryable below.
                self.fail_live_work(message);
                return;
            }
            if let Ok(mut view) = self.view.lock()
                && !self.revoked.load(Ordering::Acquire)
            {
                view.error = message.to_owned();
            }
            self.publish();
            return;
        }
        let Some(result) = envelope.get("result") else {
            self.fail_live_work("runtime.protocolViolation");
            return;
        };
        if matches!(kind, CommandKind::Start | CommandKind::Inspect)
            && let Ok(mut view) = self.view.lock()
            && !self.revoked.load(Ordering::Acquire)
        {
            view.snapshot = Some(result.clone());
            view.error.clear();
        }
        // The normal project-open path needs no extra recovery clicks. The
        // worker only admits a *clean* native snapshot; the existing confirm
        // handler re-hashes the project and all stores before granting access.
        // Real interrupted work still waits for a human recovery decision.
        if matches!(kind, CommandKind::Start)
            && let Some(id) = result["snapshotId"].as_str()
        {
            // If the project was opened while the component was acquiring an
            // empty launch snapshot, rebind after ownership fully retires.
            if result["projectPath"].is_null() {
                let _ = slint::invoke_from_event_loop(project_opened);
            }
            match self.native.approve_clean_project(id) {
                Ok(Some(approval)) => {
                    if let Err(error) = self.command(
                        json!({"method":"runtime.confirmRecovery","approval":approval}),
                        CommandKind::Confirm,
                    ) {
                        self.fail_live_work(&error);
                        return;
                    }
                }
                Ok(None) => {}
                Err(error) => {
                    self.fail_live_work(&error);
                    return;
                }
            }
        }
        if matches!(kind, CommandKind::Confirm)
            && let Ok(mut view) = self.view.lock()
            && !self.revoked.load(Ordering::Acquire)
        {
            view.error.clear();
        }
        if matches!(kind, CommandKind::Confirm) && result["reviewed"] == true {
            if !self.native.reviewed_current_project() {
                self.fail_live_work("agent.errorRecoveryChanged");
                return;
            }
            self.publish();
            return;
        }
        if matches!(kind, CommandKind::Confirm) && result["resumed"] == true {
            let valid = AcceptedSubmission::parse(result)
                .ok()
                .is_some_and(|receipt| {
                    self.native.tasks.recovered_task().is_some_and(|task| {
                        receipt.task == task.id
                            && receipt.conversation == task.conversation_id
                            && receipt.submission == task.submission_id
                            && !self.revoked.load(Ordering::Acquire)
                            && self.native.tasks.may_present(receipt.conversation, None)
                    })
                });
            if !valid {
                self.fail_live_work("agent.errorRecoveryChanged");
                return;
            }
            // The resumed original submission owns its observation and wait.
            // Observing the latest chat here would supersede that wait, and
            // could also select a different conversation from an edit/script.
            self.publish();
            return;
        }
        if matches!(kind, CommandKind::Confirm) {
            match self.native.tasks.latest() {
                Ok(Some(conversation)) => {
                    if let Ok(mut observed) = self.observed_conversation.lock() {
                        *observed = Some(conversation);
                    }
                    if let Err(error) = self.command(
                        json!({"method":"runtime.observe","conversationId":conversation}),
                        CommandKind::Observe,
                    ) {
                        self.fail_live_work(&error);
                        return;
                    }
                }
                Ok(None) => {}
                Err(error) => {
                    self.fail_live_work(&error);
                    return;
                }
            }
        }
        if matches!(kind, CommandKind::Submit)
            && let Err(error) = self.accept_submission(result)
        {
            self.fail_live_work(&error);
            return;
        }
        if matches!(kind, CommandKind::Observe)
            && let Err(error) = self.accept_presentation(result)
        {
            self.fail_live_work(&error);
            return;
        }
        self.publish();
    }

    fn start_approval_worker(self: &Arc<Self>, envelope: Value) {
        let id = envelope["id"]
            .as_str()
            .expect("validated request id")
            .to_owned();
        self.start_approval_operation(id, move |native| native.handle(envelope["payload"].clone()));
    }

    fn start_approval_operation(
        self: &Arc<Self>,
        id: String,
        operation: impl FnOnce(&NativeSession) -> Result<Value, String> + Send + 'static,
    ) {
        let started = (|| -> Result<(), String> {
            let mut worker = self
                .approval_worker
                .lock()
                .map_err(|_| "permission.stateUnavailable")?;
            if worker.is_some() {
                return Err("permission.alreadyPending".into());
            }
            // Native preflight may reject this attempt before authorize is
            // reached. Its result must not inherit a previous user refusal.
            self.native.tool_approval.begin_approval_attempt()?;
            let runtime = Arc::clone(self);
            let request_id = id.clone();
            *worker = Some(
                std::thread::Builder::new()
                    .name("bk-cat-tool-approval".into())
                    .spawn(move || {
                        let result = operation(&runtime.native);
                        runtime.dispatch_approval_result(
                            Work::ApprovalFinished {
                                id: request_id,
                                result,
                            },
                            |work| runtime.incoming.try_send(work),
                        );
                    })
                    .map_err(|error| format!("permission.stateUnavailable\n{error}"))?,
            );
            Ok(())
        })();
        if let Err(error) = started {
            self.reply_native(&id, Err(error));
        }
    }

    fn dispatch_approval_result(
        &self,
        work: Work,
        send: impl FnOnce(Work) -> Result<(), mpsc::TrySendError<Work>>,
    ) {
        if self.revoked.load(Ordering::Acquire) {
            return;
        }
        if send(work).is_err() {
            self.fail_live_work("runtime.approvalDeliveryFailed");
        }
    }

    fn join_approval_worker(&self) -> Result<(), String> {
        let worker = self
            .approval_worker
            .lock()
            .map_err(|_| "permission.stateUnavailable")?
            .take();
        if let Some(worker) = worker {
            worker.join().map_err(|_| "permission.stateUnavailable")?;
        }
        Ok(())
    }

    fn reply_native(&self, id: &str, result: Result<Value, String>) {
        self.reply_native_via(
            id,
            result,
            || self.bridge.status(),
            |reply| self.post(reply),
        );
    }

    fn reply_native_via(
        &self,
        id: &str,
        result: Result<Value, String>,
        status: impl FnOnce() -> Result<concat_agent::OwnerStatus, String>,
        post: impl FnOnce(Value) -> Result<(), String>,
    ) {
        if self.revoked.load(Ordering::Acquire) {
            return;
        }
        // A refused approval is a normal tool result. An uncertain native edit
        // still revokes the owner and enters the existing recovery protection.
        match status() {
            Ok(status)
                if matches!(
                    status.phase,
                    concat_agent::Phase::Draining | concat_agent::Phase::Released
                ) =>
            {
                self.fail_live_work("agent.connectAndReview");
                return;
            }
            Err(error) => {
                self.fail_live_work(&error);
                return;
            }
            _ => {}
        }
        let reply = match result {
            Ok(result) => json!({"id":id,"result":result}),
            Err(message) => json!({"id":id,"error":{"code":"refused","message":message}}),
        };
        if !self.revoked.load(Ordering::Acquire)
            && let Err(error) = post(reply)
        {
            self.fail_live_work(&error);
        }
    }

    fn post(&self, envelope: Value) -> Result<(), String> {
        let serialized =
            encode_frame(&self.component, envelope).map_err(|error| error.to_string())?;
        self.post_encoded(&serialized)
    }

    fn post_encoded(&self, serialized: &str) -> Result<(), String> {
        self.platform
            .lock()
            .map_err(|_| "runtime platform poisoned")?
            .as_ref()
            .ok_or("native platform is not ready")?
            .post_message(serialized)
    }

    fn waiting_deadline(&self) -> Option<Instant> {
        self.waiting
            .lock()
            .ok()
            .and_then(|waiting| waiting.as_ref().map(|waiting| waiting.deadline))
    }

    fn queue_waiting(
        &self,
        intent: SubmissionIntent,
        configuration: ConfigurationSnapshot,
    ) -> Result<(), String> {
        self.queue_waiting_with(intent, configuration, LOAD_TIMEOUT, || {
            self.incoming.try_send(Work::Waiting)
        })
    }

    fn queue_waiting_with(
        &self,
        intent: SubmissionIntent,
        configuration: ConfigurationSnapshot,
        timeout: Duration,
        send: impl FnOnce() -> Result<(), mpsc::TrySendError<Work>>,
    ) -> Result<(), String> {
        {
            // Match reject_submission's view-before-slot ordering. Waiting
            // never calls begin_task, installs a submission or grants a model.
            let mut view = self.view.lock().map_err(|_| "agent.sessionUnavailable")?;
            if self.revoked.load(Ordering::Acquire) || view.closed || !view.error.is_empty() {
                return Err("agent.closedDetail".into());
            }
            let mut waiting = self
                .waiting
                .lock()
                .map_err(|_| "agent.sessionUnavailable")?;
            if waiting.is_some() || view.waiting_for_ready {
                return Err("agent.alreadyWaiting".into());
            }
            if view.task_running
                || self
                    .submission
                    .lock()
                    .map_err(|_| "agent.sessionUnavailable")?
                    .is_some()
            {
                return Err("runtime.taskBusy".into());
            }
            if intent.mode == TaskMode::Script
                || !(view.may_wait_for_readiness()
                    || (view.phase == "ready"
                        && view.snapshot.as_ref().is_some_and(clean_startup_snapshot)))
            {
                return Err("agent.connectAndReview".into());
            }
            *waiting = Some(WaitingSubmission {
                intent,
                configuration,
                deadline: Instant::now() + timeout,
            });
            view.waiting_for_ready = true;
            view.task_title.clear();
        }
        // Wake the mailbox even if runtime.loaded already cancelled its load
        // deadline. Queue failure uses the same fail-closed cleanup as Submit.
        if send().is_err() {
            let error = "runtime.queueFull";
            self.fail_live_work(error);
            return Err(error.into());
        }
        self.publish();
        Ok(())
    }

    fn take_ready_waiting(&self) -> Result<Option<WaitingSubmission>, String> {
        let mut view = self.view.lock().map_err(|_| "agent.sessionUnavailable")?;
        let mut waiting = self
            .waiting
            .lock()
            .map_err(|_| "agent.sessionUnavailable")?;
        let Some(pending) = waiting.as_ref() else {
            return Ok(None);
        };
        if self.revoked.load(Ordering::Acquire) || view.closed {
            waiting.take();
            view.waiting_for_ready = false;
            return Ok(None);
        }
        if Instant::now() >= pending.deadline {
            return Err(SEND_WAIT_TIMEOUT_MESSAGE.into());
        }
        if !view.error.is_empty() {
            return Err(view.error.clone());
        }
        if view
            .snapshot
            .as_ref()
            .is_some_and(|snapshot| !clean_startup_snapshot(snapshot))
        {
            if view.phase == "ready" {
                return Err("runtime.protocolViolation".into());
            }
            // A legitimate recovery block is not consent to resume old work.
            // Leave its recovery controls intact and discard only this send.
            waiting.take();
            view.waiting_for_ready = false;
            view.task_title = "agent.sendNeedsRecovery".into();
            return Ok(None);
        }
        if view.phase != "ready" {
            return Ok(None);
        }
        let snapshot = view.snapshot.as_ref().ok_or("runtime.protocolViolation")?;
        let expected_session = pending.intent.project.session_id.to_string();
        let expected_revision = pending.intent.project.revision.to_string();
        if snapshot["projectPath"] != pending.intent.project.path
            || snapshot["projectSession"].as_str() != Some(expected_session.as_str())
            || snapshot["projectRevision"].as_str() != Some(expected_revision.as_str())
        {
            return Err("runtime.stateChanged".into());
        }
        // The UI thread rechecks current project/configuration after this
        // single take. No waiting/view lock crosses the normal submit chain.
        view.waiting_for_ready = false;
        Ok(waiting.take())
    }

    fn queue_submission(&self, intent: SubmissionIntent) -> Result<(), String> {
        self.queue_submission_with(intent, || self.incoming.try_send(Work::Submit))
    }

    fn queue_submission_with(
        &self,
        intent: SubmissionIntent,
        send: impl FnOnce() -> Result<(), mpsc::TrySendError<Work>>,
    ) -> Result<(), String> {
        let view = self.view();
        if self.revoked.load(Ordering::Acquire) || view.phase != "ready" || view.waiting_for_ready {
            return Err("agent.connectAndReview".into());
        }
        if let Some(generation) = &intent.generation {
            generation.bind_component_revocation(Arc::clone(&self.revoked))?;
        }
        let task_configuration = intent
            .model
            .as_ref()
            .map(|channel| serde_json::json!({"id": channel.id.as_str(), "model": channel.model}));
        let title = match intent.mode {
            TaskMode::Chat => "agent.runningChat",
            TaskMode::Script => "agent.runningWriting",
            TaskMode::Edit => "agent.runningEdit",
        };
        {
            let mut submission = self
                .submission
                .lock()
                .map_err(|_| "agent.sessionUnavailable")?;
            if submission.is_some() {
                return Err("runtime.taskBusy".into());
            }
            self.native.tool_approval.begin_task()?;
            *submission = Some(intent);
        }
        if let Ok(mut view) = self.view.lock() {
            if self.revoked.load(Ordering::Acquire) {
                return Err("agent.stopDetail".into());
            }
            view.task_running = true;
            if let Some(configuration) = task_configuration {
                view.configuration = Some(configuration);
            }
            view.task_title = title.into();
            view.error.clear();
        }
        if send().is_err() {
            self.submission
                .lock()
                .map_err(|_| "agent.sessionUnavailable")?
                .take();
            let error = "runtime.queueFull";
            self.fail_live_work(error);
            return Err(error.into());
        }
        self.publish();
        Ok(())
    }

    fn prepare_submission(&self) {
        let intent = self
            .submission
            .lock()
            .ok()
            .and_then(|intent| intent.clone());
        let Some(intent) = intent else {
            return;
        };
        let result = self
            .native
            .prepare_submission(&intent, |payload| self.encode_command(payload))
            .and_then(|command| {
                self.send_command(command, CommandKind::Submit)
                    .map_err(SubmissionError::Failed)
            });
        self.finish_submission_preparation(intent, result);
    }

    fn finish_submission_preparation(
        &self,
        intent: SubmissionIntent,
        result: Result<(), SubmissionError>,
    ) {
        match result {
            Ok(()) => {}
            Err(SubmissionError::RejectedInput(message)) => {
                if let Err(error) = self.reject_submission(&intent.id, message) {
                    self.fail_live_work(&error);
                }
            }
            Err(SubmissionError::Failed(error)) => {
                self.native.tasks.abandon();
                if let Ok(mut submission) = self.submission.lock() {
                    submission.take();
                }
                if let Some(generation) = intent.generation {
                    deliver_generation(generation, Err(error.clone()));
                }
                self.fail_live_work(&error);
            }
        }
    }

    fn reject_submission(&self, task: &str, message: &str) -> Result<(), String> {
        let rejected = {
            // Stop marks revoked before taking this view lock. Never restore a
            // phase or permission, and never clear a different task's state.
            let mut view = self.view.lock().map_err(|_| "agent.sessionUnavailable")?;
            if self.revoked.load(Ordering::Acquire) || view.closed {
                return Ok(());
            }
            if view.phase != "ready" || !view.error.is_empty() {
                return Err("runtime.stateChanged".into());
            }
            let mut submission = self
                .submission
                .lock()
                .map_err(|_| "agent.sessionUnavailable")?;
            if submission.as_ref().is_none_or(|intent| intent.id != task) {
                return Err("runtime.protocolViolation".into());
            }
            let rejected = submission.take().ok_or("runtime.protocolViolation")?;
            view.task_running = false;
            view.task_title = message.to_owned();
            rejected
        };
        // No accepted receipt was received, so the editor draft and reference
        // selection stay untouched. A script candidate likewise stays failed.
        if let Some(generation) = rejected.generation {
            deliver_generation(generation, Err(message.to_owned()));
        }
        self.publish();
        Ok(())
    }

    fn accept_submission(&self, result: &Value) -> Result<(), String> {
        let accepted = AcceptedSubmission::parse(result)?;
        let intent = self
            .submission
            .lock()
            .map_err(|_| "agent.sessionUnavailable")?
            .clone()
            .ok_or("runtime.protocolViolation")?;
        if result["taskId"] != intent.id
            || result["submissionId"]
                .as_u64()
                .is_none_or(|id| id > 9_007_199_254_740_991)
        {
            return Err("runtime.protocolViolation".into());
        }
        let conversation = result["conversationId"]
            .as_u64()
            .ok_or("runtime.protocolViolation")?;
        if !self.native.tasks.may_present(conversation, None) {
            return Err("runtime.protocolViolation".into());
        }
        *self
            .observed_conversation
            .lock()
            .map_err(|_| "agent.sessionUnavailable")? = Some(conversation);
        let mut receipt = self
            .accepted_submission
            .lock()
            .map_err(|_| "agent.sessionUnavailable")?;
        if self.revoked.load(Ordering::Acquire) {
            return Err("agent.stopDetail".into());
        }
        if receipt.is_some() {
            return Err("runtime.protocolViolation".into());
        }
        *receipt = Some(accepted);
        drop(receipt);
        let component = self.component.clone();
        if intent.mode != TaskMode::Script {
            let _ = slint::invoke_from_event_loop(move || {
                Shell::with(|shell, app| {
                    if shell
                        .embedded
                        .borrow()
                        .as_ref()
                        .is_some_and(|runtime| runtime.component() == component)
                    {
                        shell.agent_pane.borrow_mut().clear_submission_if_unchanged(
                            &intent.ui_draft,
                            &intent.reference_keys,
                            &app,
                        );
                    }
                });
            });
        }
        Ok(())
    }

    fn accept_presentation(&self, value: &Value) -> Result<(), String> {
        let conversation = value["conversationId"]
            .as_u64()
            .ok_or("runtime.protocolViolation")?;
        let observed = *self
            .observed_conversation
            .lock()
            .map_err(|_| "agent.sessionUnavailable")?;
        // A detached observation can have one already queued frame. It is
        // ignored instead of replacing another project's/task's presentation.
        if !self.native.tasks.may_present(conversation, observed) {
            return Ok(());
        }
        let messages = value["messages"]
            .as_array()
            .ok_or("runtime.protocolViolation")?;
        if messages.len() > 100
            || messages.iter().any(|message| {
                !matches!(
                    message["role"].as_str(),
                    Some("user" | "assistant" | "tool" | "compaction")
                ) || message["text"]
                    .as_str()
                    .is_none_or(|text| text.len() > 128 * 1024)
            })
            || value["partialText"]
                .as_str()
                .is_none_or(|text| text.len() > 256 * 1024)
            || !value["busy"].is_boolean()
        {
            return Err("runtime.protocolViolation".into());
        }
        self.view
            .lock()
            .map_err(|_| "agent.sessionUnavailable")?
            .presentation = Some(value.clone());
        Ok(())
    }

    /// Only the native recovery protocol can produce the task behind this
    /// event. Event-supplied model, language, path or writing data is ignored.
    fn accept_recovered_submission(self: &Arc<Self>, event: &Value) -> Result<(), String> {
        let receipt = AcceptedSubmission::parse(event)?;
        let task = self
            .native
            .tasks
            .recovered_task()
            .ok_or("agent.errorRecoveryScope")?;
        if receipt.task != task.id
            || receipt.conversation != task.conversation_id
            || receipt.submission != task.submission_id
            || self.revoked.load(Ordering::Acquire)
            || !self.native.tasks.may_present(receipt.conversation, None)
            || self
                .native
                .current_project()?
                .as_ref()
                .is_none_or(|current| {
                    current.path != task.project.path
                        || current.project_id != task.project.project_id
                        || current.session_id != task.project.session_id
                        || (task.mode == TaskMode::Script
                            && current.revision != task.project.revision)
                })
        {
            return Err("agent.errorRecoveryChanged".into());
        }
        let generation = if task.mode == TaskMode::Script {
            Some(self.recover_writing_context(&task)?)
        } else {
            if task.generation_snapshot.is_some() {
                return Err("agent.errorRecoverySnapshotInvalid".into());
            }
            None
        };
        let intent = SubmissionIntent {
            languages: task.languages,
            model: Some(task.model.clone()),
            id: task.id,
            project: task.project,
            mode: task.mode,
            input: task.input,
            ui_draft: task.ui_draft,
            reference_keys: Vec::new(),
            selected: task.selected,
            generation: generation.clone(),
        };
        let result = (|| -> Result<(), String> {
            let mut view = self.view.lock().map_err(|_| "runtime.protocolViolation")?;
            let mut submission = self
                .submission
                .lock()
                .map_err(|_| "runtime.protocolViolation")?;
            let mut accepted = self
                .accepted_submission
                .lock()
                .map_err(|_| "runtime.protocolViolation")?;
            let mut observed = self
                .observed_conversation
                .lock()
                .map_err(|_| "runtime.protocolViolation")?;
            if self.revoked.load(Ordering::Acquire)
                || view.closed
                || view.phase != "ready"
                || !view.error.is_empty()
                || view.waiting_for_ready
                || submission.is_some()
                || accepted.is_some()
            {
                return Err("agent.errorRecoveryChanged".into());
            }
            *observed = Some(receipt.conversation);
            *accepted = Some(receipt);
            *submission = Some(intent);
            view.task_running = true;
            view.task_title = match task.mode {
                TaskMode::Chat => "agent.runningChat",
                TaskMode::Script => "agent.runningWriting",
                TaskMode::Edit => "agent.runningEdit",
            }
            .into();
            view.configuration =
                Some(json!({"id":task.model.id.as_str(), "model":task.model.model}));
            Ok(())
        })();
        if let Err(error) = &result
            && let Some(generation) = generation
        {
            deliver_generation(generation, Err(error.clone()));
        }
        result
    }

    /// The mailbox waits only for a native UI capture, never for a model or
    /// approval. Subsequent completion frames stay ordered behind this event.
    fn recover_writing_context(
        &self,
        task: &crate::agent::tasks::RecoveredTask,
    ) -> Result<crate::panes::script::GenerationRequest, String> {
        let saved = task
            .generation_snapshot
            .clone()
            .ok_or("agent.errorRecoverySnapshotMissing")?;
        let expected = task.project.clone();
        let task_id = task.id.clone();
        let component = self.component.clone();
        let revoked = Arc::clone(&self.revoked);
        let (send, receive) = mpsc::sync_channel(1);
        slint::invoke_from_event_loop(move || {
            let mut result = Err("agent.errorRecoveryChanged".to_owned());
            Shell::with(|shell, app| {
                if revoked.load(Ordering::Acquire)
                    || shell
                        .embedded
                        .borrow()
                        .as_ref()
                        .is_none_or(|runtime| runtime.component() != component)
                {
                    return;
                }
                let mut studio = shell.studio.borrow_mut();
                let valid = studio.session.as_ref().is_some_and(|session| {
                    session.path() == expected.path
                        && session.project_identity() == expected.project_id
                        && session.session_id() == expected.session_id
                        && session.revision() == expected.revision
                });
                if !valid {
                    return;
                }
                result = studio
                    .script
                    .recover_generation(&task_id, expected.session_id, expected.revision, &saved)
                    .and_then(|request| {
                        if let Err(error) = request.bind_component_revocation(Arc::clone(&revoked))
                        {
                            studio.script.cancel_generation();
                            return Err(error);
                        }
                        Ok(request)
                    });
                studio.script.publish(&app);
            });
            // A timed-out or stopped mailbox cannot leave a writing spinner
            // behind when the UI finally processes its queued capture.
            if let Err(mpsc::SendError(Ok(request))) = send.send(result) {
                crate::panes::script::complete_generation(
                    request,
                    Err("agent.errorRecoveryChanged".into()),
                );
            }
        })
        .map_err(|_| "agent.errorRecoveryChanged".to_owned())?;
        receive
            .recv_timeout(Duration::from_secs(15))
            .map_err(|_| "agent.errorRecoveryChanged".to_owned())?
    }

    fn complete_submission(&self, event: &Value) {
        let result = (|| -> Result<(bool, bool, Option<String>), String> {
            let completed = AcceptedSubmission::parse(event)?;
            if self
                .accepted_submission
                .lock()
                .map_err(|_| "agent.sessionUnavailable")?
                .as_ref()
                != Some(&completed)
            {
                return Err("runtime.protocolViolation".into());
            }
            let id = event["taskId"]
                .as_str()
                .ok_or("runtime.protocolViolation")?;
            let conversation = event["conversationId"]
                .as_u64()
                .ok_or("runtime.protocolViolation")?;
            let successful = match event["status"].as_str() {
                Some("done") => true,
                Some("unanswered") => false,
                _ => return Err("runtime.protocolViolation".into()),
            };
            let text = event["text"]
                .as_str()
                .filter(|text| text.len() <= 1024 * 1024)
                .ok_or("runtime.protocolViolation")?;
            let intent = self
                .submission
                .lock()
                .map_err(|_| "agent.sessionUnavailable")?
                .clone()
                .filter(|intent| intent.id == id)
                .ok_or("runtime.protocolViolation")?;
            let native_edit_verified = self.native.tasks.finish(id, conversation)?;
            let edit_without_changes =
                successful && intent.mode == TaskMode::Edit && !native_edit_verified;
            let native_refusal = self.native.tool_approval.last_refusal(conversation);
            self.accepted_submission
                .lock()
                .map_err(|_| "agent.sessionUnavailable")?
                .take();
            self.submission
                .lock()
                .map_err(|_| "agent.sessionUnavailable")?
                .take();
            if let Some(generation) = intent.generation {
                deliver_generation(
                    generation,
                    if successful {
                        Ok(text.to_owned())
                    } else {
                        Err("script.generationFailed".into())
                    },
                );
            }
            Ok((successful, edit_without_changes, native_refusal))
        })();
        match result {
            Ok((successful, edit_without_changes, native_refusal)) => {
                self.finish_task_presentation(successful, edit_without_changes, native_refusal);
            }
            Err(error) => self.fail_live_work(&error),
        }
    }

    fn finish_task_presentation(
        &self,
        successful: bool,
        edit_without_changes: bool,
        native_refusal: Option<String>,
    ) {
        self.finish_task_presentation_with(successful, edit_without_changes, native_refusal, || {});
    }

    fn finish_task_presentation_with(
        &self,
        successful: bool,
        edit_without_changes: bool,
        native_refusal: Option<String>,
        before_view: impl FnOnce(),
    ) {
        if self.revoked.load(Ordering::Acquire) {
            return;
        }
        before_view();
        let result = self
            .view
            .lock()
            .map_err(|_| "agent.sessionUnavailable".to_owned())
            .and_then(|mut view| {
                finish_task_view(&mut view, successful, edit_without_changes, native_refusal)
            });
        // A real failure must retire through the native lifecycle, not leave
        // a ready owner with input disabled and no reconnect/review route.
        // Classification does not write a new error: only a failure winning
        // the atomic race with explicit stop may publish that diagnosis.
        // Drop the view lock before failure revokes and publishes the runtime.
        match result {
            Ok(()) => self.publish(),
            Err(error) => self.fail_live_work(&error),
        }
    }

    fn command(&self, payload: Value, kind: CommandKind) -> Result<(), String> {
        let command = self
            .encode_command(payload)
            .map_err(|error| error.to_string())?;
        self.send_command(command, kind)
    }

    fn encode_command(&self, payload: Value) -> Result<EncodedCommand, SubmissionError> {
        if self.revoked.load(Ordering::Acquire) {
            return Err("agent.stopDetail".into());
        }
        let sequence = self
            .next
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_add(1))
            .map_err(|_| "native command sequence exhausted")?;
        let id = format!("native:{sequence}");
        let serialized = encode_frame(&self.component, json!({"id":id,"payload":payload}))?;
        if serialized.len() > MAX_COMMAND_FRAME_BYTES {
            return Err(FrameEncodingError::TooLarge.into());
        }
        Ok(EncodedCommand { id, serialized })
    }

    fn send_command(&self, command: EncodedCommand, kind: CommandKind) -> Result<(), String> {
        if self.revoked.load(Ordering::Acquire) {
            return Err("agent.stopDetail".into());
        }
        let EncodedCommand { id, serialized } = command;
        {
            let mut pending = self
                .pending
                .lock()
                .map_err(|_| "runtime command registry poisoned")?;
            if pending.len() >= 32 {
                return Err("native command limit reached".into());
            }
            pending.insert(id.clone(), kind);
        }
        if let Err(error) = self.post_encoded(&serialized) {
            if let Ok(mut pending) = self.pending.lock() {
                pending.remove(&id);
            }
            return Err(error);
        }
        Ok(())
    }

    /// Triggered only by a real save completion, never by an idle timer.
    /// The brief native-view lock guarantees we do not silently lose a
    /// completion receipt if a worker is publishing at the same instant.
    pub(crate) fn poll_saved(&self) {
        if self.revoked.load(Ordering::Acquire) || self.view().phase != "ready" {
            return;
        }
        if self.scan_queued.swap(true, Ordering::AcqRel) {
            return;
        }
        if self.incoming.try_send(Work::SavedScan).is_err() {
            self.scan_queued.store(false, Ordering::Release);
        }
    }

    pub(crate) fn inspect(&self) -> Result<(), String> {
        if self.view().phase != "recovery" {
            return Err("agent.connectAndReview".into());
        }
        self.command(json!({"method":"runtime.inspect"}), CommandKind::Inspect)
    }

    pub(crate) fn confirm(&self) -> Result<(), String> {
        self.confirm_decision("continue")
    }

    pub(crate) fn confirm_current_review(&self) -> Result<(), String> {
        self.confirm_decision("review")
    }

    pub(crate) fn discard_interrupted(&self) -> Result<(), String> {
        self.confirm_decision("discard")
    }

    fn confirm_decision(&self, decision: &str) -> Result<(), String> {
        let view = self.view();
        if view.phase != "recovery" {
            return Err("agent.connectAndReview".into());
        }
        let snapshot = view.snapshot.ok_or("agent.reviewBeforeContinue")?;
        let id = snapshot["snapshotId"]
            .as_str()
            .ok_or("agent.errorRecoverySnapshotInvalid")?;
        let approval = self.native.approve_from_ui(id, decision)?;
        self.command(
            json!({"method":"runtime.confirmRecovery","approval":approval}),
            CommandKind::Confirm,
        )
    }

    fn revoke(&self, reason: &str) {
        // Publish retirement before revoking ownership or waking a human/model
        // waiter. Their late replies must observe stop before native drain.
        let first = !self.revoked.swap(true, Ordering::AcqRel);
        self.revoke_marked(reason, first);
    }

    fn revoke_marked(&self, reason: &str, first: bool) {
        // Native execution loses authority synchronously, before any page or
        // database cleanup. A late JavaScript callback cannot keep it alive.
        debug_assert!(self.revoked.load(Ordering::Acquire));
        if first {
            self.foreground_reconnect
                .store(platform_reason_is_background(reason), Ordering::Release);
        }
        self.bridge.stop();
        self.native.tool_approval.cancel();
        self.native.tasks.cancel();
        if let Ok(mut accepted) = self.accepted_submission.lock() {
            accepted.take();
        }
        log::info!("bk-cat-agent: state=stopping");
        if let Ok(mut view) = self.view.lock() {
            if !view.closed {
                view.phase = "stopping".into();
            }
            view.task_running = false;
            view.waiting_for_ready = false;
            // Teardown milestones are not failures, even if they are the first
            // callback or arrive after a completed close. A real failure may
            // replace an old placeholder but never the other way around.
            if !platform_reason_is_placeholder(reason)
                && (first || platform_reason_is_placeholder(&view.error))
            {
                view.error = reason.to_owned();
            }
        }
        if let Ok(mut waiting) = self.waiting.lock() {
            waiting.take();
        }
        if let Ok(mut submission) = self.submission.lock()
            && let Some(intent) = submission.take()
            && let Some(generation) = intent.generation
        {
            deliver_generation(generation, Err("script.stopped".into()));
        }
        self.publish();
    }

    fn remember_terminal_failure(&self, reason: &str) {
        if let Ok(mut view) = self.view.lock()
            && terminal_reason_replaces_placeholder(&view.error, reason)
        {
            log::error!("bk-cat-agent: platform_failed reason={reason}");
            view.error = format!("runtime.platformFailed\n{reason}");
        }
    }

    pub(crate) fn tool_permission_view(
        &self,
    ) -> Result<crate::agent::tool_approval::PermissionView, String> {
        self.native.tool_approval.view()
    }

    pub(crate) fn set_tool_mode_from_ui(&self, index: i32) -> Result<(), String> {
        if self.revoked.load(Ordering::Acquire) || self.view().phase != "ready" {
            return Err("agent.connectAndReview".into());
        }
        self.native.tool_approval.set_mode_from_ui(index)
    }

    pub(crate) fn decide_tool_from_ui(
        &self,
        request_id: &str,
        approve: bool,
    ) -> Result<(), String> {
        if self.revoked.load(Ordering::Acquire) || self.view().phase != "ready" {
            return Err("agent.notReady".into());
        }
        self.native
            .tool_approval
            .decide_from_ui(request_id, approve)
    }

    pub(crate) fn stop(&self) {
        self.revoke("");
        // An explicit stop/project change is never a foreground recovery.
        self.foreground_reconnect.store(false, Ordering::Release);
        self.close_platform();
    }

    fn close_platform(&self) {
        if let Ok(platform) = self.platform.lock()
            && let Some(platform) = platform.as_ref()
        {
            platform.close();
        }
    }

    fn fail_terminal(&self, message: &str) {
        // Joining a crashed approval worker is a real cleanup failure even
        // when the user stopped first. Platform/cleanup faults are never
        // filtered by the live-work cancellation race.
        log::error!("bk-cat-agent: state=failed");
        self.revoke(message);
        self.close_platform();
    }

    fn record_live_error(&self, message: &str) {
        self.record_live_error_with(message, || {});
    }

    fn record_live_error_with(&self, message: &str, before_view: impl FnOnce()) {
        before_view();
        if let Ok(mut view) = self.view.lock()
            && !self.revoked.load(Ordering::Acquire)
            && view.error.is_empty()
        {
            // Cancellation wakes blocked native reads with an error. A read
            // returning after native retirement is not a new runtime fault.
            // A live transport fault still reaches its original tool and is
            // retained for task completion; later errors cannot replace it.
            view.error = message.to_owned();
        }
    }

    fn fail_live_work(&self, message: &str) {
        // All failures from live frames, commands, deadlines and submissions
        // claim retirement atomically against explicit stop. A late callback
        // cannot relabel normal cancellation; a failure winning first keeps
        // its diagnosis. This is the single default work-failure entry point.
        // Platform destruction and native cleanup errors never use this gate.
        if self
            .revoked
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            log::error!("bk-cat-agent: state=failed");
            self.revoke_marked(message, true);
            self.close_platform();
        }
    }

    fn publish(&self) {
        let component = self.component.clone();
        let _ = slint::invoke_from_event_loop(move || {
            let mut refresh_permission = false;
            let mut task_finished = false;
            let mut claim_waiting = false;
            Shell::with(|shell, app| {
                let current = shell
                    .embedded
                    .borrow()
                    .as_ref()
                    .filter(|runtime| runtime.component() == component)
                    .cloned();
                let Some(current) = current else {
                    return;
                };
                // Read at delivery, not enqueue time. An older waiting update
                // must not clear the authorization of an already claimed send.
                let view = current.view();
                claim_waiting = view.waiting_for_ready;
                // The worker pushes actual state changes only. There is no
                // 250ms poll during ordinary editing or playback.
                let owner = shell
                    .agent
                    .borrow()
                    .as_ref()
                    .ok_or_else(|| "agent.sessionUnavailable".to_owned())
                    .and_then(|bridge| bridge.status());
                let mut pane = shell.agent_pane.borrow_mut();
                let was_ready = pane.readiness.runtime_ready;
                task_finished = pane.task.running && !view.task_running;
                pane.apply_runtime(&view);
                refresh_permission = was_ready != pane.readiness.runtime_ready;
                pane.publish(&app, owner);
            });
            if refresh_permission {
                // The original poll also updated the tool's connected badge.
                // Preserve that update on actual connect/disconnect only.
                crate::agent::window::refresh_tool_permission();
            }
            if task_finished {
                crate::panes::agent::refresh_model_choices();
            }
            if claim_waiting {
                resume_waiting_from_ui(&component);
            }
            finish_assistant_connection();
        });
    }
}

fn platform_reason_is_placeholder(reason: &str) -> bool {
    matches!(
        reason,
        "" | "lifecycle.windowsStopping" | "lifecycle.windowsClosed"
    ) || platform_reason_is_background(reason)
}

fn platform_reason_is_background(reason: &str) -> bool {
    matches!(
        reason,
        "lifecycle.activityPaused" | "lifecycle.activityStopped" | "lifecycle.activitySaved"
    )
}

fn terminal_reason_replaces_placeholder(current: &str, reason: &str) -> bool {
    platform_reason_is_placeholder(current) && !platform_reason_is_placeholder(reason)
}

fn finish_runtime_view(view: &mut RuntimeView, result: Result<(), String>) {
    // Only the actual platform-destruction path calls this, after joining the
    // approval worker and attempting native resource/ownership cleanup.
    view.closed = true;
    view.phase = "closed".into();
    match result {
        Err(error) => view.error = error,
        Ok(()) if platform_reason_is_placeholder(&view.error) => view.error.clear(),
        Ok(()) => {}
    }
}

impl Drop for EmbeddedRuntime {
    fn drop(&mut self) {
        self.bridge.stop();
        self.native.tool_approval.cancel();
        self.native.tasks.cancel();
        if let Ok(platform) = self.platform.lock()
            && let Some(platform) = platform.as_ref()
        {
            platform.close();
        }
    }
}

fn deliver_generation(
    request: crate::panes::script::GenerationRequest,
    result: Result<String, String>,
) {
    // Always marshal to the UI, including stop while Studio is already borrowed.
    let _ = slint::invoke_from_event_loop(move || {
        crate::panes::script::complete_generation(request, result)
    });
}

/// Re-check native task/script authority in the actual editing callback.
pub(crate) fn validate_current_edit(
    studio: &crate::studio::Studio,
    session: u64,
    revision: u64,
) -> Result<(), String> {
    let mut result = Ok(());
    Shell::with(|shell, _| {
        if let Some(runtime) = shell.embedded.borrow().as_ref() {
            result = runtime
                .native
                .tasks
                .validate_edit(studio, session, revision);
        }
    });
    // A direct trusted native bridge without a web component retains its
    // original owner/session guards (including the isolated native probe).
    // Every production page reaches NativeSession's task guard first.
    result
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ModelGrantAction {
    Grant,
    Revoke,
    RevokeAndStop,
    Refuse,
}

fn model_grant_action(authorized: bool, ready: bool, running: bool) -> ModelGrantAction {
    if authorized {
        if running {
            ModelGrantAction::RevokeAndStop
        } else {
            ModelGrantAction::Revoke
        }
    } else if ready {
        ModelGrantAction::Grant
    } else {
        ModelGrantAction::Refuse
    }
}

pub(crate) fn authorize_call_from_ui() {
    Shell::with(|shell, app| {
        let runtime = shell.embedded.borrow().clone();
        let mut pane = shell.agent_pane.borrow_mut();
        let ready = runtime
            .as_ref()
            .is_some_and(|runtime| runtime.view().phase == "ready" && !runtime.view().task_running)
            && pane.readiness.credentials_ready
            && !pane.readiness.provider.is_empty()
            && !pane.readiness.model.is_empty();
        let running = runtime
            .as_ref()
            .is_some_and(|runtime| runtime.view().task_running);
        match model_grant_action(pane.readiness.calls_authorized, ready, running) {
            ModelGrantAction::RevokeAndStop => {
                // Revoke synchronously: no further model request can cross this boundary.
                drop(pane);
                stop_current_task(shell, &app);
                return;
            }
            ModelGrantAction::Revoke => {
                pane.readiness.calls_authorized = false;
                pane.error.clear();
            }
            ModelGrantAction::Grant => {
                pane.readiness.calls_authorized = true;
                pane.error.clear();
                if let Some(runtime) = runtime
                    && let Ok(mut view) = runtime.view.lock()
                {
                    view.error.clear();
                }
            }
            ModelGrantAction::Refuse => {
                pane.readiness.calls_authorized = false;
                pane.error = "agent.configureBeforeSend".into();
            }
        }
        pane.publish(
            &app,
            shell
                .agent
                .borrow()
                .as_ref()
                .ok_or_else(|| "agent.sessionUnavailable".into())
                .and_then(|bridge| bridge.status()),
        );
    });
}

/// Only a trusted Slint window callback calls these; the web component never
/// receives a port to set modes or resolve approvals.
pub(crate) fn set_tool_mode_from_ui(index: i32) -> Result<(), String> {
    let mut result = Err("agent.chatNotReady".to_owned());
    Shell::with(|shell, _| {
        if let Some(runtime) = shell.embedded.borrow().as_ref() {
            result = runtime.set_tool_mode_from_ui(index);
        }
    });
    result
}

pub(crate) fn decide_tool_from_ui(request_id: String, approve: bool) -> Result<(), String> {
    let mut result = Err("agent.chatNotReady".to_owned());
    Shell::with(|shell, _| {
        if let Some(runtime) = shell.embedded.borrow().as_ref() {
            result = runtime.decide_tool_from_ui(&request_id, approve);
        }
    });
    result
}

pub(crate) fn submit_chat_from_ui(input: String, edit: bool) -> Result<(), String> {
    submit_from_ui(
        input,
        if edit { TaskMode::Edit } else { TaskMode::Chat },
        false,
    )
}

pub(crate) fn submit_script_generation(instruction: String, rewrite: bool) -> Result<(), String> {
    submit_from_ui(instruction, TaskMode::Script, rewrite)
}

fn start_first_chat_from_send(input: &str) -> Result<(), String> {
    // A new send after an explicit stop is fresh intent, not replay of the
    // cancelled task. Neither surface needs a separate connection control.
    #[cfg(all(
        any(target_os = "windows", target_os = "android"),
        not(feature = "bk-cat-native-probe")
    ))]
    {
        let mut reconnect = false;
        Shell::with(|shell, app| {
            reconnect = assistant_is_visible(&app)
                && shell
                    .embedded
                    .borrow()
                    .as_ref()
                    .is_some_and(|runtime| runtime.revoked.load(Ordering::Acquire));
        });
        if reconnect {
            activate_from_user_intent();
        }
    }
    let mut should_connect = false;
    let mut result: Result<(), String> = Ok(());
    Shell::with(|shell, app| {
        if shell.embedded.borrow().is_some() {
            return;
        }
        result = (|| {
            if app.global::<crate::ui::AgentSettingsUi>().get_busy() {
                return Err("runtime.modelSettingsBusy".into());
            }
            if shell.studio.borrow().session.is_none() {
                return Err("agent.openProject".into());
            }
            let owner = shell
                .agent
                .borrow()
                .as_ref()
                .ok_or("agent.sessionUnavailable")?
                .status();
            shell
                .agent_pane
                .borrow()
                .validate_waiting_submission(owner, input)?;
            should_connect = true;
            Ok(())
        })();
    });
    result?;
    // No RefCell borrow from the preflight survives a reentrant Shell lookup.
    if should_connect {
        connect_from_ui()?;
    }
    Ok(())
}

fn resume_waiting_from_ui(component: &str) {
    Shell::with(|shell, app| {
        let Some(runtime) = shell
            .embedded
            .borrow()
            .as_ref()
            .filter(|runtime| runtime.component() == component)
            .cloned()
        else {
            return;
        };
        let result = (|| -> Result<(), String> {
            let waiting = runtime.take_ready_waiting()?;
            // Publish the live state after taking/discarding the slot. A queued
            // ready notification cannot restore an already stopped component.
            shell.agent_pane.borrow_mut().apply_runtime(&runtime.view());
            let Some(waiting) = waiting else {
                return Ok(());
            };
            if runtime.revoked.load(Ordering::Acquire)
                || shell
                    .agent
                    .borrow()
                    .as_ref()
                    .is_none_or(|bridge| !Arc::ptr_eq(bridge, &runtime.bridge))
            {
                return Err("runtime.stateChanged".into());
            }
            if app.global::<crate::ui::AgentSettingsUi>().get_busy() {
                return Err("runtime.modelSettingsBusy".into());
            }
            let studio = shell.studio.borrow();
            let session = studio.session.as_ref().ok_or("agent.openProject")?;
            let current = crate::live::CurrentProject {
                path: session.path().to_owned(),
                project_id: session.project_identity().to_owned(),
                session_id: session.session_id(),
                revision: session.revision(),
            };
            if current != waiting.intent.project {
                return Err("runtime.stateChanged".into());
            }
            let selected = waiting
                .intent
                .model
                .as_ref()
                .or_else(|| waiting.configuration.active())
                .ok_or("agent.errorModelRequired")?;
            crate::panes::agent_settings::configuration_store(&studio.host.dirs)
                .assert_connection_current(selected)
                .map_err(|error| error.to_string())?;
            shell
                .agent_pane
                .borrow()
                .validate_submission(runtime.bridge.status(), &waiting.intent.ui_draft)?;
            drop(studio);
            // This is the original explicit Send, now admitted by fresh native
            // facts. Approval reset, exact frame sizing and credential/network
            // admission remain entirely in the unchanged normal submit chain.
            shell.agent_pane.borrow_mut().readiness.calls_authorized = true;
            if let Err(error) = runtime.queue_submission(waiting.intent) {
                shell.agent_pane.borrow_mut().readiness.calls_authorized = false;
                return Err(error);
            }
            log::info!("bk-cat-agent-audit event=user_send_authorized_model_call");
            Ok(())
        })();
        if let Err(error) = result {
            runtime.fail_live_work(&error);
        }
        let mut pane = shell.agent_pane.borrow_mut();
        pane.apply_runtime(&runtime.view());
        pane.publish(&app, runtime.bridge.status());
    });
}

fn submit_from_ui(input: String, mode: TaskMode, rewrite: bool) -> Result<(), String> {
    if mode != TaskMode::Script {
        start_first_chat_from_send(&input)?;
    }
    let mut result = Err("agent.sessionUnavailable".to_owned());
    Shell::with(|shell, app| {
        result = (|| {
            let runtime = shell
                .embedded
                .borrow()
                .clone()
                .ok_or("agent.connectAndReview")?;
            let bridge = shell
                .agent
                .borrow()
                .clone()
                .ok_or("agent.sessionUnavailable")?;
            if app.global::<crate::ui::AgentSettingsUi>().get_busy() {
                return Err("runtime.modelSettingsBusy".into());
            }
            let view = runtime.view();
            shell.agent_pane.borrow_mut().apply_runtime(&view);
            if view.waiting_for_ready {
                return Err("agent.alreadyWaiting".into());
            }
            let waiting = mode != TaskMode::Script && view.may_wait_for_readiness();
            // Pressing Send is the user's explicit intent for this model
            // request. Do not require a separate fee-consent click for each
            // ordinary prompt. Tool editing remains independently gated.
            if !waiting {
                let mut pane = shell.agent_pane.borrow_mut();
                if !pane.readiness.calls_authorized
                    && runtime.try_ready() == Some(true)
                    && pane.readiness.credentials_ready
                    && !pane.readiness.provider.is_empty()
                    && !pane.readiness.model.is_empty()
                    && !pane.task.running
                {
                    pane.readiness.calls_authorized = true;
                    log::info!("bk-cat-agent-audit event=user_send_authorized_model_call");
                }
                if !pane.readiness.calls_authorized {
                    return Err("agent.notReady".into());
                }
            }
            let text = if waiting {
                shell
                    .agent_pane
                    .borrow()
                    .validate_waiting_submission(bridge.status(), &input)?
            } else {
                shell
                    .agent_pane
                    .borrow()
                    .validate_submission(bridge.status(), &input)?
            };
            let mut studio = shell.studio.borrow_mut();
            let (reference_keys, reference_context) = if mode == TaskMode::Script {
                (Vec::new(), String::new())
            } else {
                shell.agent_pane.borrow().resolve_references(&studio)?
            };
            let ui_draft = input.clone();
            let session = studio.session.as_ref().ok_or("agent.openProject")?;
            let project = crate::live::CurrentProject {
                path: session.path().to_owned(),
                project_id: session.project_identity().to_owned(),
                session_id: session.session_id(),
                revision: session.revision(),
            };
            // All fallible model preflight precedes begin_generation: a removed
            // connection must never leave a writing task marked in progress.
            let configuration_store =
                crate::panes::agent_settings::configuration_store(&studio.host.dirs);
            let configuration = configuration_store
                .load()
                .map_err(|error| error.to_string())?;
            let model = crate::panes::agent::capture_session_model(&mut studio, &configuration)?;
            // Persist bookkeeping before any external writing/session work.
            // This does not save unrelated edits, change native revision, or
            // invalidate the task snapshot if the whole process exits now.
            concat_host::projects::drafts::retain_external_work(std::path::Path::new(
                &project.path,
            ))?;
            let generation = if mode == TaskMode::Script {
                Some(studio.script.begin_generation(
                    project.session_id,
                    project.revision,
                    &text,
                    rewrite,
                )?)
            } else {
                None
            };
            // Direct edits to the open timeline do not require a script.
            // When a script has been selected, keep its immutable binding for
            // template edits and reject a stale selection at native apply time.
            let selected = if mode == TaskMode::Edit {
                let selected = studio.script.selected_binding();
                // A direct edit does not depend on an unfinished script save.
                // Only a task bound to a script version must wait for it.
                if selected.is_some() && !studio.script.selection_settled() {
                    return Err("script.waitOperation".into());
                }
                selected
            } else {
                None
            };
            let id = match &generation {
                Some(generation) => generation.request_id().to_owned(),
                None => random_capability()?,
            };
            let prompt = generation.as_ref().map_or_else(
                || format!("{text}{reference_context}"),
                |generation| generation.prompt().to_owned(),
            );
            let languages = crate::agent::tasks::TaskLanguages::capture(
                studio
                    .prefs
                    .script_content_languages
                    .get(&project.project_id)
                    .map(String::as_str),
            );
            let intent = SubmissionIntent {
                languages,
                model: Some(model),
                id,
                project,
                mode,
                input: prompt,
                ui_draft,
                reference_keys,
                selected,
                generation,
            };
            let queued = if waiting {
                // Configuration metadata only: no vault read, model grant or
                // native task is created until this exact snapshot is rechecked.
                let configuration =
                    crate::panes::agent_settings::configuration_store(&studio.host.dirs)
                        .load()
                        .map_err(|error| error.to_string())?;
                runtime.queue_waiting(intent, configuration)
            } else {
                runtime.queue_submission(intent)
            };
            if let Err(error) = queued {
                if mode == TaskMode::Script {
                    studio.script.cancel_generation();
                }
                return Err(error);
            }
            // Keep this model authorization across sequential user messages.
            // Model changes, explicit revoke/stop and owner retirement clear it.
            studio.script.publish(&app);
            Ok(())
        })();
    });
    result
}

pub(crate) fn stop_from_ui() {
    Shell::with(|shell, app| stop_current_task(shell, &app));
}

// The same fail-closed stop is used when a user revokes an active model grant.
fn stop_current_task(shell: &Shell, app: &crate::ui::App) {
    ASSISTANT_CONNECTION_REQUEST.with(|pending| pending.set(None));
    ASSISTANT_ACTIVE_SESSION.with(|active| active.set(None));
    shell.agent_pane.borrow_mut().readiness.calls_authorized = false;
    shell.studio.borrow_mut().script.cancel_generation();
    if let Some(runtime) = shell.embedded.borrow().as_ref() {
        runtime.stop();
    } else if let Some(bridge) = shell.agent.borrow().as_ref() {
        bridge.stop();
    }
    shell.studio.borrow().script.publish(app);
    shell.agent_pane.borrow().publish(
        app,
        shell
            .agent
            .borrow()
            .as_ref()
            .ok_or_else(|| "agent.sessionUnavailable".into())
            .and_then(|bridge| bridge.status()),
    );
}

fn authenticate(raw: &str, component: &str) -> Result<Value, String> {
    if raw.len() > MAX_FRAME_BYTES {
        return Err("frame too large".into());
    }
    let value: Value = serde_json::from_str(raw).map_err(|_| "invalid frame JSON")?;
    if !value.is_object() || value["component"] != component || value["channel"] != CHANNEL {
        return Err("frame does not belong to this native component".into());
    }
    let has_payload = value.get("payload").is_some();
    let has_event = value.get("event").is_some();
    let has_result = value.get("result").is_some();
    let has_error = value.get("error").is_some();
    if [has_payload, has_event, has_result, has_error]
        .into_iter()
        .filter(|v| *v)
        .count()
        != 1
    {
        return Err("ambiguous native frame".into());
    }
    if has_payload && !value["payload"].is_object() {
        return Err("payload must be an object".into());
    }
    Ok(value)
}

/// Retire the bridge while its original project is still current. Called
/// with Studio mutably borrowed, so this path must never borrow it again.
pub(crate) fn stop_for_project_change(bridge: &Arc<AgentBridge>) {
    // Admission ends synchronously even if the window is already gone. The
    // platform's actual destruction still owns database cleanup and handoff.
    let mut has_shell = false;
    Shell::with(|shell, _| {
        has_shell = true;
        stop_project_runtime(bridge, shell.embedded.borrow().as_deref());
    });
    if !has_shell {
        bridge.stop();
    }
}

fn stop_project_runtime(bridge: &Arc<AgentBridge>, runtime: Option<&EmbeddedRuntime>) {
    match runtime {
        Some(runtime) if Arc::ptr_eq(&runtime.bridge, bridge) => runtime.stop(),
        Some(_) => {
            // A stale project teardown cannot close a replacement component,
            // even if independently acquired owners have equal generations.
            bridge.stop();
        }
        None => {
            bridge.stop();
            // Shell installs a component before any editor callback can run,
            // and retains it until a closed component is replaced. Therefore
            // None means this bridge never started a platform component (for
            // example, resource or mailbox creation failed). No closed event
            // will arrive; finish the unused owner's handoff off the UI thread.
            let bridge = Arc::clone(bridge);
            crate::host::spawn_detached(move || {
                if let Err(error) = bridge.finish_handoff(std::time::Duration::from_secs(3)) {
                    log::error!("bk-cat-agent: project handoff failed: {error}");
                }
            });
        }
    }
}

/// First use stays silent on desktop and Android. Phone project switches also
/// wait for new user intent; an active desktop assistant follows the new project.
/// Handoff uses the same cancellable, session-bound request as explicit opening,
/// so a late platform-close event cannot reconnect after the user has stopped.
pub(crate) fn project_opened() {
    ASSISTANT_CONNECTION_REQUEST.with(|pending| pending.set(None));
    let previously_active = ASSISTANT_ACTIVE_SESSION.with(|active| active.replace(None));
    let mut previous = None;
    let mut connect_now = false;
    Shell::with(|shell, app| {
        let (path, session_id) = match shell.studio.borrow().session.as_ref() {
            Some(session) => (session.path().to_owned(), session.session_id()),
            None => return,
        };
        let manual_phone = app.get_phone() && !cfg!(feature = "bk-cat-native-probe");
        let runtime = shell.embedded.borrow().clone();
        match runtime {
            None => {
                if app.get_phone() {
                    let ui = app.global::<crate::ui::AgentUi>();
                    ui.set_phone_sheet_state(0);
                    ui.set_phone_script(false);
                }
                connect_now = !cfg!(all(
                    any(target_os = "windows", target_os = "android"),
                    not(feature = "bk-cat-native-probe")
                ))
            }
            Some(runtime) => {
                let view = runtime.view();
                let bound = view
                    .snapshot
                    .as_ref()
                    .and_then(|snapshot| snapshot["projectPath"].as_str());
                let old_owner_draining = shell.agent.borrow().as_ref().is_some_and(|agent| {
                    agent
                        .status()
                        .is_ok_and(|status| status.phase == concat_agent::Phase::Draining)
                });
                if !view.closed
                    && !old_owner_draining
                    && (bound == Some(path.as_str())
                        || (bound.is_none() && view.snapshot.is_none()))
                {
                    ASSISTANT_ACTIVE_SESSION.with(|active| active.set(previously_active));
                    return;
                }
                if app.get_phone() {
                    // A new phone project starts with its native editor visible.
                    // Ownership still retires through actual platform close.
                    let ui = app.global::<crate::ui::AgentUi>();
                    ui.set_phone_sheet_state(0);
                    ui.set_phone_script(false);
                }
                if !manual_phone && previously_active.is_some() {
                    ASSISTANT_ACTIVE_SESSION.with(|active| active.set(Some(session_id)));
                    ASSISTANT_CONNECTION_REQUEST.with(|pending| pending.set(Some(session_id)));
                }
                if !view.closed {
                    previous = Some(runtime);
                }
            }
        }
    });
    if let Some(runtime) = previous {
        runtime.stop();
    }
    if connect_now {
        show_ui_error(connect_from_ui());
    } else {
        finish_assistant_connection();
    }
}

/// Explicit native UI action. Reopening after background/stop first requires
/// the old platform's destruction and its database/ownership release.
pub(crate) fn connect_from_ui() -> Result<(), String> {
    let mut result = Err("agent.sessionUnavailable".into());
    Shell::with(|shell, _| {
        result = (|| {
            if let Some(runtime) = shell.embedded.borrow().as_ref()
                && !runtime.view().closed
            {
                return Err("agent.stopping".into());
            }
            let existing = shell.agent.borrow().clone();
            let bridge = if let Some(existing) = existing {
                match existing.status()?.phase {
                    concat_agent::Phase::RecoveryRequired if shell.embedded.borrow().is_none() => {
                        existing
                    }
                    concat_agent::Phase::Released => {
                        let studio = shell.studio.borrow();
                        Arc::new(AgentBridge::acquire(
                            &studio.host.dirs,
                            studio.host.open_projects.clone(),
                            studio.host.exporter.clone(),
                        )?)
                    }
                    _ => return Err("agent.stopping".into()),
                }
            } else {
                let studio = shell.studio.borrow();
                Arc::new(AgentBridge::acquire(
                    &studio.host.dirs,
                    studio.host.open_projects.clone(),
                    studio.host.exporter.clone(),
                )?)
            };
            let dirs = shell.studio.borrow().host.dirs.clone();
            let runtime = EmbeddedRuntime::start(&dirs, Arc::clone(&bridge))?;
            ASSISTANT_CONNECTION_REQUEST.with(|pending| pending.set(None));
            ASSISTANT_ACTIVE_SESSION.with(|active| {
                active.set(
                    shell
                        .studio
                        .borrow()
                        .session
                        .as_ref()
                        .map(|session| session.session_id()),
                );
            });
            *shell.agent.borrow_mut() = Some(Arc::clone(&bridge));
            shell.studio.borrow_mut().host.agent = Some(bridge);
            *shell.embedded.borrow_mut() = Some(runtime);
            let mut pane = shell.agent_pane.borrow_mut();
            if let Some(runtime) = shell.embedded.borrow().as_ref() {
                pane.apply_runtime(&runtime.view());
            }
            pane.error.clear();
            pane.recovery_summary.clear();
            pane.can_recover = false;
            pane.can_discard = false;
            pane.can_review = false;
            pane.readiness.runtime_ready = false;
            Ok(())
        })();
    });
    result
}

/// The editor must not start Pi until the user actually starts using
/// it. Manual cutting and playback allocate no agent runtime or model thread.
/// Native probes retain their explicit test initialization semantics.
#[cfg(all(
    any(target_os = "windows", target_os = "android"),
    not(feature = "bk-cat-native-probe")
))]
pub(crate) fn start_for_window() {}

/// Opening either assistant, editing a draft or visiting the script workspace
/// is explicit user intent to connect. It never grants model or edit authority.
/// Both surfaces wait for the same real destruction/ownership handoff before
/// replacing a retired component, without restoring any stopped submission.
#[cfg(all(
    any(target_os = "windows", target_os = "android"),
    not(feature = "bk-cat-native-probe")
))]
pub(crate) fn activate_from_user_intent() {
    Shell::with(|shell, _| {
        let Some(session_id) = shell
            .studio
            .borrow()
            .session
            .as_ref()
            .map(|s| s.session_id())
        else {
            return;
        };
        ASSISTANT_ACTIVE_SESSION.with(|active| active.set(Some(session_id)));
        ASSISTANT_CONNECTION_REQUEST.with(|pending| pending.set(Some(session_id)));
    });
    finish_assistant_connection();
}

fn assistant_is_visible(app: &crate::ui::App) -> bool {
    let ui = app.global::<crate::ui::AgentUi>();
    if app.get_phone() {
        ui.get_phone_sheet_state() > 0
    } else {
        ui.get_sidebar_open()
    }
}

/// Called only by the native Android Activity observer. Foreground restoration
/// requests a new component for an already open assistant; it never resurrects
/// a retired component or resumes model/tool work. Both event orders are valid:
/// resume before destruction waits for publish(), and resume after it connects.
pub fn platform_foreground_changed(foreground: bool) {
    PLATFORM_FOREGROUND.store(foreground, Ordering::Release);
    if !foreground {
        return;
    }
    let _ = slint::invoke_from_event_loop(|| {
        if !PLATFORM_FOREGROUND.load(Ordering::Acquire) {
            return;
        }
        Shell::with(|shell, app| {
            if !app.get_phone() || app.global::<crate::ui::AgentUi>().get_phone_sheet_state() == 0 {
                return;
            }
            let lifecycle_retired = shell
                .embedded
                .borrow()
                .as_ref()
                .is_some_and(|runtime| runtime.foreground_reconnect.load(Ordering::Acquire));
            if lifecycle_retired && let Some(session) = shell.studio.borrow().session.as_ref() {
                ASSISTANT_CONNECTION_REQUEST
                    .with(|pending| pending.set(Some(session.session_id())));
            }
        });
        finish_assistant_connection();
    });
}

/// Resume only an assistant already used in this native project. A settings
/// read at startup is not user intent, even with a visible desktop rail. The
/// maintenance worker releases its owner before publishing completion.
pub(crate) fn resume_assistant_connection_after_settings() {
    Shell::with(|shell, app| {
        if assistant_is_visible(&app)
            && let Some(session) = shell.studio.borrow().session.as_ref()
            && ASSISTANT_ACTIVE_SESSION.with(|active| active.get()) == Some(session.session_id())
        {
            ASSISTANT_CONNECTION_REQUEST.with(|pending| pending.set(Some(session.session_id())));
        }
    });
    finish_assistant_connection();
}

fn finish_assistant_connection() {
    let Some(requested_session) = ASSISTANT_CONNECTION_REQUEST.with(|pending| pending.get()) else {
        return;
    };
    let mut should_connect = false;
    Shell::with(|shell, app| {
        let current_session = shell
            .studio
            .borrow()
            .session
            .as_ref()
            .map(|s| s.session_id());
        if (!assistant_is_visible(&app) && !cfg!(feature = "bk-cat-native-probe"))
            || current_session != Some(requested_session)
            || ASSISTANT_ACTIVE_SESSION.with(|active| active.get()) != Some(requested_session)
        {
            ASSISTANT_CONNECTION_REQUEST.with(|pending| pending.set(None));
            return;
        }
        if app.get_phone() && !PLATFORM_FOREGROUND.load(Ordering::Acquire) {
            return;
        }
        let settings = app.global::<crate::ui::AgentSettingsUi>();
        if settings.get_busy() || settings.get_open() {
            // The settings worker may still own storage exclusively. Its
            // completion/close event retries this one intent after release.
            return;
        }
        if let Some(runtime) = shell.embedded.borrow().as_ref() {
            // Draft edits revisit this gate. Read one flag without cloning a
            // growing transcript/configuration snapshot on every keystroke.
            let closed = runtime.view.lock().is_ok_and(|view| view.closed);
            if !closed {
                if !runtime.revoked.load(Ordering::Acquire) {
                    // Repeated taps reuse a loading/ready connection. An old
                    // owner that is actually draining retains the one intent.
                    ASSISTANT_CONNECTION_REQUEST.with(|pending| pending.set(None));
                }
                return;
            }
            if !shell.agent.borrow().as_ref().is_some_and(|bridge| {
                bridge
                    .status()
                    .is_ok_and(|status| status.phase == concat_agent::Phase::Released)
            }) {
                return;
            }
        }
        // Consume before creation. A failed replacement reports its actual
        // failure once; published error events cannot form a reconnect loop.
        ASSISTANT_CONNECTION_REQUEST.with(|pending| pending.set(None));
        should_connect = true;
    });
    if should_connect {
        show_ui_error(connect_from_ui());
    }
}

/// One explicit language-change projection. Unlike runtime publication this
/// cannot claim queued input, resume a conversation, load a directory or start
/// a model request. Native task snapshots and historical body text stay intact.
pub(crate) fn refresh_language_from_ui() {
    Shell::with(|shell, app| {
        let owner = shell
            .agent
            .borrow()
            .as_ref()
            .ok_or_else(|| "agent.sessionUnavailable".to_owned())
            .and_then(|bridge| bridge.status());
        // Titles, errors, authors and previews are rendered from their stable
        // state. Do not reapply RuntimeView: it can overwrite a still-valid
        // local refusal that has no corresponding runtime.error.
        shell.agent_pane.borrow().publish(&app, owner);
    });
    crate::panes::agent_settings::retranslate_from_ui();
    crate::agent::window::refresh_tool_permission();
}

/// No API key, model provider or fee authorization is inferred by startup.
#[cfg(feature = "bk-cat-native-probe")]
pub(crate) fn start_for_window() {
    // The welcome screen has no current project. Start the agent only after
    // the editor opens one; manual cutting never needs an idle AI component.
    let mut has_project = false;
    Shell::with(|shell, _| {
        has_project = shell.studio.borrow().session.is_some();
    });
    if !has_project {
        return;
    }
    if let Err(error) = connect_from_ui() {
        log::error!("bk-cat-agent: state=failed");
        Shell::with(|shell, app| {
            let mut pane = shell.agent_pane.borrow_mut();
            pane.error = error;
            pane.publish(
                &app,
                shell
                    .agent
                    .borrow()
                    .as_ref()
                    .ok_or_else(|| "agent.sessionUnavailable".to_owned())
                    .and_then(|bridge| bridge.status()),
            );
        });
    }
}

/// Preserve drafts and report an actual native action failure.
pub(crate) fn show_ui_error(result: Result<(), String>) {
    if let Err(error) = result {
        Shell::with(|shell, app| {
            let mut pane = shell.agent_pane.borrow_mut();
            pane.error = error;
            pane.publish(
                &app,
                shell
                    .agent
                    .borrow()
                    .as_ref()
                    .ok_or_else(|| "agent.sessionUnavailable".to_owned())
                    .and_then(|bridge| bridge.status()),
            );
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use slint::Model;

    #[test]
    fn a_task_receipt_never_replaces_an_existing_native_or_runtime_error() {
        for (successful, edit_without_changes) in [(true, true), (true, false), (false, false)] {
            let mut view = RuntimeView {
                error: "工程版本已变化，原生操作未执行。".into(),
                task_running: true,
                task_title: "agent.runningEdit".into(),
                ..Default::default()
            };
            finish_task_view(
                &mut view,
                successful,
                edit_without_changes,
                Some(crate::i18n::t("permission.userDenied").as_str().into()),
            )
            .unwrap_err();
            assert_eq!(view.error, "工程版本已变化，原生操作未执行。");
            assert!(!view.task_running);
            assert!(view.task_title.is_empty());
        }
    }

    #[test]
    fn only_completed_refused_edits_get_a_nonblocking_refusal_title() {
        for reason in [
            crate::i18n::t("permission.userDenied").as_str(),
            crate::i18n::t("permission.timeout").as_str(),
        ] {
            let mut view = RuntimeView {
                task_running: true,
                task_title: "agent.runningEdit".into(),
                ..Default::default()
            };
            finish_task_view(&mut view, true, true, Some(reason.into())).unwrap();
            assert!(!view.task_running);
            assert!(view.error.is_empty());
            assert_eq!(view.task_title, reason);

            let error = finish_task_view(&mut view, false, false, Some(reason.into())).unwrap_err();
            assert!(error.contains("runtime.taskIncomplete"));
            assert!(
                view.error.is_empty(),
                "classification cannot publish an error before winning retirement"
            );
            assert!(view.task_title.is_empty());
        }
        let mut view = RuntimeView::default();
        let error = finish_task_view(&mut view, true, true, None).unwrap_err();
        assert!(error.contains("runtime.editUnverified"));
        assert!(view.error.is_empty());

        let mut view = RuntimeView {
            task_running: true,
            task_title: "agent.runningEdit".into(),
            ..Default::default()
        };
        finish_task_view(&mut view, true, false, None).unwrap();
        assert!(!view.task_running);
        assert!(view.error.is_empty());
        assert!(view.task_title.is_empty());
    }

    #[test]
    fn model_grant_can_span_messages_but_revocation_stops_active_work() {
        assert_eq!(
            model_grant_action(false, true, false),
            ModelGrantAction::Grant
        );
        assert_eq!(
            model_grant_action(true, true, false),
            ModelGrantAction::Revoke
        );
        assert_eq!(
            model_grant_action(true, false, true),
            ModelGrantAction::RevokeAndStop
        );
        assert_eq!(
            model_grant_action(false, false, true),
            ModelGrantAction::Refuse
        );
        assert_eq!(
            model_grant_action(false, false, false),
            ModelGrantAction::Refuse
        );
    }

    mod project_retirement {
        use std::path::PathBuf;
        use std::time::{Duration, Instant};

        use concat_agent::{Phase, Reconciliation, RecoveryDecision};

        use super::*;

        struct Scratch(PathBuf);

        impl Scratch {
            fn new() -> Self {
                let root = std::env::temp_dir().join(format!(
                    "bk-cat-component-retirement-{}",
                    random_capability().unwrap()
                ));
                std::fs::create_dir(&root).unwrap();
                Self(root)
            }

            fn dirs(&self) -> AppDirs {
                AppDirs::under(&self.0)
            }
        }

        impl Drop for Scratch {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }

        fn bridge(dirs: &AppDirs) -> Arc<AgentBridge> {
            Arc::new(
                AgentBridge::acquire(
                    dirs,
                    concat_api::OpenProjects::default(),
                    concat_host::export::Exporter::new(),
                )
                .unwrap(),
            )
        }

        fn admit(bridge: &AgentBridge) {
            bridge
                .confirm_recovery(
                    Reconciliation {
                        project_identity: "native-project".into(),
                        project_revision: "1".into(),
                        session_revision: "1".into(),
                    },
                    RecoveryDecision::DiscardInterrupted,
                )
                .unwrap();
            assert_eq!(bridge.status().unwrap().phase, Phase::Ready);
        }

        struct HeldPlatform {
            posted: mpsc::Sender<String>,
            closes: Arc<AtomicU64>,
        }

        impl PlatformHost for HeldPlatform {
            fn post_message(&self, message: &str) -> Result<(), String> {
                self.posted
                    .send(message.to_owned())
                    .map_err(|error| error.to_string())
            }

            fn close(&self) {
                // A close request is deliberately separate from destruction.
                // The test must deliver the platform's closed callback itself.
                self.closes.fetch_add(1, Ordering::AcqRel);
            }
        }

        struct ControlledRuntime {
            runtime: Arc<EmbeddedRuntime>,
            callbacks: RuntimeCallbacks,
            posted: mpsc::Receiver<String>,
            closes: Arc<AtomicU64>,
        }

        impl ControlledRuntime {
            fn start(bridge: Arc<AgentBridge>) -> Self {
                let (posted, received) = mpsc::channel();
                let closes = Arc::new(AtomicU64::new(0));
                let mut callbacks = None;
                let runtime = EmbeddedRuntime::start_with(
                    bridge,
                    random_capability().unwrap(),
                    Arc::new(RuntimeResources::for_failed_factory()),
                    |_, received_callbacks| {
                        callbacks = Some(received_callbacks);
                        Ok(Box::new(HeldPlatform {
                            posted,
                            closes: Arc::clone(&closes),
                        }))
                    },
                )
                .unwrap();
                Self {
                    runtime,
                    callbacks: callbacks.unwrap(),
                    posted: received,
                    closes,
                }
            }

            fn acquire_frame(&self, id: &str) -> String {
                json!({"channel":CHANNEL,"component":self.runtime.component(),"id":id,
                    "payload":{"method":"ownership.acquire","storageId":STORAGE_ID}})
                .to_string()
            }

            fn prove_closed(&self) {
                (self.callbacks.closed)(String::new());
                wait_for(|| self.runtime.view().closed);
                assert_eq!(self.runtime.bridge.status().unwrap().phase, Phase::Released);
            }
        }

        fn wait_for(mut complete: impl FnMut() -> bool) {
            let deadline = Instant::now() + Duration::from_secs(5);
            while !complete() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(5));
            }
            assert!(
                complete(),
                "native lifecycle did not complete before deadline"
            );
        }

        #[test]
        fn retirement_matches_bridge_identity_and_waits_for_platform_destruction() {
            let old_storage = Scratch::new();
            let current_storage = Scratch::new();
            let old = bridge(&old_storage.dirs());
            let current = bridge(&current_storage.dirs());
            let controlled = ControlledRuntime::start(Arc::clone(&current));
            admit(&old);
            admit(&current);
            assert_eq!(
                old.status().unwrap().generation,
                current.status().unwrap().generation,
                "equal generation numbers are not shared bridge identity"
            );

            stop_for_project_change(&old);
            stop_project_runtime(&old, Some(controlled.runtime.as_ref()));
            assert_eq!(old.status().unwrap().phase, Phase::Draining);
            assert_eq!(current.status().unwrap().phase, Phase::Ready);
            assert_eq!(controlled.closes.load(Ordering::Acquire), 0);
            assert!(!controlled.runtime.revoked.load(Ordering::Acquire));
            (controlled.callbacks.message)(controlled.acquire_frame("js:active"));
            let reply: Value = serde_json::from_str(
                &controlled
                    .posted
                    .recv_timeout(Duration::from_secs(5))
                    .unwrap(),
            )
            .unwrap();
            assert!(reply["result"]["handle"].is_string(), "{reply}");

            stop_for_project_change(&current);
            stop_project_runtime(&current, Some(controlled.runtime.as_ref()));
            assert_eq!(controlled.closes.load(Ordering::Acquire), 1);
            assert!(!controlled.runtime.view().closed);
            assert_eq!(current.status().unwrap().phase, Phase::Draining);
            assert!(current.status().unwrap().active_operations > 0);
            assert!(
                AgentBridge::acquire(
                    &current_storage.dirs(),
                    concat_api::OpenProjects::default(),
                    concat_host::export::Exporter::new(),
                )
                .is_err(),
                "a close request cannot release live native storage"
            );
            (controlled.callbacks.message)(controlled.acquire_frame("js:late"));
            assert!(matches!(
                controlled.posted.try_recv(),
                Err(mpsc::TryRecvError::Empty)
            ));
            assert!(!controlled.runtime.seen.lock().unwrap().contains("js:late"));
            assert!(
                current
                    .handle(json!({"jsonrpc":"2.0","id":1,"method":"ping"}))
                    .is_err()
            );

            controlled.prove_closed();
            let replacement = bridge(&current_storage.dirs());
            assert!(
                replacement.status().unwrap().generation > current.status().unwrap().generation
            );
            let replacement_status = replacement.status().unwrap();
            assert_eq!(replacement_status.phase, Phase::RecoveryRequired);
            (controlled.callbacks.message)(controlled.acquire_frame("js:after-reconnect"));
            (controlled.callbacks.stopping)("late old-component callback".into());
            assert_eq!(replacement.status().unwrap(), replacement_status);
            old.finish_handoff(Duration::from_secs(1)).unwrap();
            replacement.finish_handoff(Duration::from_secs(1)).unwrap();
        }

        #[test]
        fn retirement_without_a_component_releases_the_unused_owner_on_a_worker() {
            let storage = Scratch::new();
            let bridge = bridge(&storage.dirs());
            let generation = bridge.status().unwrap().generation;
            stop_for_project_change(&bridge);
            stop_project_runtime(&bridge, None);
            wait_for(|| bridge.status().unwrap().phase == Phase::Released);
            let replacement = AgentBridge::acquire(
                &storage.dirs(),
                concat_api::OpenProjects::default(),
                concat_host::export::Exporter::new(),
            )
            .unwrap();
            assert!(replacement.status().unwrap().generation > generation);
            assert_eq!(replacement.status().unwrap().phase, Phase::RecoveryRequired);
            replacement.finish_handoff(Duration::from_secs(1)).unwrap();
        }
    }

    struct StartupHost {
        closes: Arc<AtomicU64>,
        posted: Arc<Mutex<Vec<String>>>,
    }

    impl PlatformHost for StartupHost {
        fn post_message(&self, message: &str) -> Result<(), String> {
            self.posted.lock().unwrap().push(message.to_owned());
            Ok(())
        }

        fn close(&self) {
            // Deliberately do not acknowledge destruction. The test separately
            // proves ownership survives this request until the real callback.
            self.closes.fetch_add(1, Ordering::AcqRel);
        }
    }

    struct StartupFixture {
        root: std::path::PathBuf,
        dirs: AppDirs,
        bridge: Arc<AgentBridge>,
        runtime: Arc<EmbeddedRuntime>,
        callbacks: RuntimeCallbacks,
        closes: Arc<AtomicU64>,
        posted: Arc<Mutex<Vec<String>>>,
    }

    impl StartupFixture {
        fn new(timeout: Duration) -> Self {
            let root = std::env::temp_dir()
                .join(format!("bk-cat-startup-{}", random_capability().unwrap()));
            std::fs::create_dir(&root).unwrap();
            let dirs = AppDirs::under(&root);
            let bridge = Arc::new(
                AgentBridge::acquire(
                    &dirs,
                    concat_api::OpenProjects::default(),
                    concat_host::export::Exporter::new(),
                )
                .unwrap(),
            );
            let closes = Arc::new(AtomicU64::new(0));
            let posted = Arc::new(Mutex::new(Vec::new()));
            let mut callbacks = None;
            let runtime = EmbeddedRuntime::start_with_spawner(
                Arc::clone(&bridge),
                random_capability().unwrap(),
                Arc::new(RuntimeResources::for_failed_factory()),
                |_, value| {
                    callbacks = Some(value);
                    Ok(Box::new(StartupHost {
                        closes: Arc::clone(&closes),
                        posted: Arc::clone(&posted),
                    }))
                },
                spawn_mailbox,
                timeout,
            )
            .unwrap();
            Self {
                root,
                dirs,
                bridge,
                runtime,
                callbacks: callbacks.unwrap(),
                closes,
                posted,
            }
        }

        fn message(&self, mut envelope: Value) {
            envelope["channel"] = json!(CHANNEL);
            envelope["component"] = json!(self.runtime.component());
            (self.callbacks.message)(envelope.to_string());
        }

        fn wait_for(&self, predicate: impl Fn(&RuntimeView) -> bool) {
            let deadline = Instant::now() + Duration::from_secs(5);
            while !predicate(&self.runtime.view()) && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(5));
            }
            assert!(
                predicate(&self.runtime.view()),
                "runtime did not reach the expected state"
            );
        }

        fn finish(self) {
            if !self.runtime.view().closed {
                (self.callbacks.closed)("Native host closed".into());
            }
            self.wait_for(|view| view.closed);
            assert_eq!(
                self.bridge.status().unwrap().phase,
                concat_agent::Phase::Released
            );
            drop(self.runtime);
            drop(self.bridge);
            std::fs::remove_dir_all(self.root).unwrap();
        }
    }

    fn waiting_intent(id: &str) -> SubmissionIntent {
        SubmissionIntent {
            languages: crate::agent::tasks::TaskLanguages::default(),
            model: None,
            id: id.into(),
            project: crate::live::CurrentProject {
                path: "isolated-waiting-project".into(),
                project_id: "isolated-waiting-identity".into(),
                session_id: 7,
                revision: 3,
            },
            mode: TaskMode::Chat,
            input: "首次明确发送".into(),
            ui_draft: "首次明确发送".into(),
            reference_keys: vec!["7:retained-media".into()],
            selected: None,
            generation: None,
        }
    }

    fn waiting_configuration(fixture: &StartupFixture) -> ConfigurationSnapshot {
        crate::panes::agent_settings::configuration_store(&fixture.dirs)
            .load()
            .unwrap()
    }

    fn ready_waiting_snapshot(intent: &SubmissionIntent) -> Value {
        json!({"snapshotId":"isolated-clean-snapshot", "projectPath":intent.project.path,
            "projectSession":intent.project.session_id.to_string(), "projectRevision":intent.project.revision.to_string(),
            "hasInterruptedWork":false, "blockedReason":null})
    }

    #[test]
    fn startup_waiting_retains_one_intent_without_installing_a_task_and_stop_discards_it() {
        let fixture = StartupFixture::new(Duration::from_secs(30));
        let intent = waiting_intent("waiting-once");
        fixture
            .runtime
            .queue_waiting(intent.clone(), waiting_configuration(&fixture))
            .unwrap();
        assert!(
            fixture
                .runtime
                .queue_waiting(intent.clone(), waiting_configuration(&fixture))
                .is_err()
        );
        assert!(fixture.runtime.view().waiting_for_ready);
        assert!(!fixture.runtime.view().task_running);
        assert!(fixture.runtime.submission.lock().unwrap().is_none());
        assert!(fixture.runtime.pending.lock().unwrap().is_empty());
        assert!(fixture.posted.lock().unwrap().is_empty());
        let pane = completion_pane(&fixture);
        assert!(!pane.readiness.calls_authorized);
        let status = pane.data(fixture.bridge.status());
        assert!(status.busy && status.can_stop && !status.can_send);
        assert!(!status.can_continue && !status.can_discard && !status.can_review);
        let retained = fixture.runtime.waiting.lock().unwrap();
        assert_eq!(retained.as_ref().unwrap().intent.ui_draft, intent.ui_draft);
        assert_eq!(
            retained.as_ref().unwrap().intent.reference_keys,
            intent.reference_keys
        );
        drop(retained);
        fixture.runtime.stop();
        fixture.message(json!({"event":"runtime.phase", "phase":"ready"}));
        assert!(fixture.runtime.waiting.lock().unwrap().is_none());
        assert!(!fixture.runtime.view().waiting_for_ready);
        assert!(fixture.runtime.submission.lock().unwrap().is_none());
        assert!(fixture.posted.lock().unwrap().is_empty());
        fixture.finish();
    }

    #[test]
    fn waiting_recovery_blocks_discard_only_the_new_send_without_resuming_old_work() {
        for evidence in [
            json!({"snapshotId":"review","projectPath":"isolated-waiting-project","hasInterruptedWork":true,"blockedReason":null}),
            json!({"snapshotId":"review","projectPath":"isolated-waiting-project","hasInterruptedWork":false,"blockedReason":"project changed"}),
            json!({"snapshotId":"review","projectPath":"isolated-waiting-project","blockedReason":null}),
        ] {
            let fixture = StartupFixture::new(Duration::from_secs(30));
            fixture
                .runtime
                .queue_waiting(
                    waiting_intent("blocked-wait"),
                    waiting_configuration(&fixture),
                )
                .unwrap();
            {
                let mut view = fixture.runtime.view.lock().unwrap();
                view.phase = "recovery".into();
                view.snapshot = Some(evidence);
            }
            assert!(fixture.runtime.take_ready_waiting().unwrap().is_none());
            assert!(fixture.runtime.waiting.lock().unwrap().is_none());
            assert!(!fixture.runtime.view().waiting_for_ready);
            assert!(!fixture.runtime.revoked.load(Ordering::Acquire));
            assert!(fixture.runtime.submission.lock().unwrap().is_none());
            assert!(fixture.posted.lock().unwrap().is_empty());
            assert!(
                fixture
                    .runtime
                    .queue_waiting(
                        waiting_intent("blocked-second"),
                        waiting_configuration(&fixture)
                    )
                    .is_err()
            );
            fixture.finish();
        }
    }

    #[test]
    fn ready_waiting_claim_is_once_only_and_stop_before_normal_queue_still_revokes_it() {
        let fixture = StartupFixture::new(Duration::from_secs(30));
        let intent = waiting_intent("ready-once");
        fixture
            .runtime
            .queue_waiting(intent.clone(), waiting_configuration(&fixture))
            .unwrap();
        {
            let mut view = fixture.runtime.view.lock().unwrap();
            view.phase = "ready".into();
            view.snapshot = Some(ready_waiting_snapshot(&intent));
        }
        let claimed = fixture.runtime.take_ready_waiting().unwrap().unwrap();
        assert_eq!(claimed.intent.id, intent.id);
        assert_eq!(claimed.intent.project, intent.project);
        assert!(fixture.runtime.take_ready_waiting().unwrap().is_none());
        fixture.runtime.stop();
        assert!(fixture.runtime.queue_submission(claimed.intent).is_err());
        assert!(fixture.runtime.submission.lock().unwrap().is_none());
        assert!(fixture.posted.lock().unwrap().is_empty());
        fixture.finish();
    }

    #[test]
    fn readiness_never_claims_waiting_without_matching_native_project_evidence() {
        for field in [
            "projectPath",
            "projectSession",
            "projectRevision",
            "snapshot",
        ] {
            let fixture = StartupFixture::new(Duration::from_secs(30));
            let intent = waiting_intent("wrong-ready");
            fixture
                .runtime
                .queue_waiting(intent.clone(), waiting_configuration(&fixture))
                .unwrap();
            {
                let mut view = fixture.runtime.view.lock().unwrap();
                view.phase = "ready".into();
                let mut snapshot = ready_waiting_snapshot(&intent);
                snapshot[field] = json!("changed");
                view.snapshot = (field != "snapshot").then_some(snapshot);
            }
            assert!(fixture.runtime.take_ready_waiting().is_err(), "{field}");
            assert!(fixture.runtime.submission.lock().unwrap().is_none());
            assert!(fixture.posted.lock().unwrap().is_empty());
            fixture.finish();
        }
    }

    #[test]
    fn waiting_deadline_survives_the_actual_loaded_event_and_late_ready_cannot_send() {
        let fixture = StartupFixture::new(Duration::from_secs(30));
        fixture
            .runtime
            .queue_waiting_with(
                waiting_intent("loaded-still-waiting"),
                waiting_configuration(&fixture),
                Duration::from_millis(300),
                || fixture.runtime.incoming.try_send(Work::Waiting),
            )
            .unwrap();
        fixture.message(json!({"event":"runtime.loaded"}));
        fixture.wait_for(|view| view.loaded);
        fixture.wait_for(|view| !view.waiting_for_ready && !view.error.is_empty());
        assert_eq!(fixture.runtime.view().error, SEND_WAIT_TIMEOUT_MESSAGE);
        assert!(fixture.runtime.revoked.load(Ordering::Acquire));
        assert!(fixture.runtime.waiting.lock().unwrap().is_none());
        assert!(fixture.runtime.submission.lock().unwrap().is_none());
        fixture.message(json!({"event":"runtime.phase", "phase":"ready"}));
        assert!(!fixture.posted.lock().unwrap().iter().any(|raw| {
            serde_json::from_str::<Value>(raw).unwrap()["payload"]["method"] == "runtime.submit"
        }));
        fixture.finish();
    }

    #[test]
    fn load_failure_and_queue_overflow_clear_the_waiting_slot_without_a_task() {
        for overflow in [false, true] {
            let fixture = StartupFixture::new(Duration::from_secs(30));
            let result = fixture.runtime.queue_waiting_with(
                waiting_intent("failed-wait"),
                waiting_configuration(&fixture),
                Duration::from_secs(30),
                || {
                    if overflow {
                        Err(mpsc::TrySendError::Full(Work::Waiting))
                    } else {
                        fixture.runtime.incoming.try_send(Work::Waiting)
                    }
                },
            );
            if overflow {
                assert!(result.is_err());
            } else {
                result.unwrap();
                fixture.message(json!({"event":"runtime.loadFailed","code":"script-execution"}));
            }
            fixture.wait_for(|view| !view.waiting_for_ready && !view.error.is_empty());
            assert!(fixture.runtime.revoked.load(Ordering::Acquire));
            assert!(fixture.runtime.waiting.lock().unwrap().is_none());
            assert!(fixture.runtime.submission.lock().unwrap().is_none());
            assert!(fixture.posted.lock().unwrap().is_empty());
            fixture.finish();
        }
    }

    fn queued_size_fixture(fixture: &StartupFixture, id: &str) -> SubmissionIntent {
        let intent = SubmissionIntent {
            languages: crate::agent::tasks::TaskLanguages::default(),
            model: None,
            id: id.into(),
            project: crate::live::CurrentProject {
                project_id: "isolated-size-project".into(),
                path: fixture.root.to_string_lossy().into_owned(),
                session_id: 1,
                revision: 1,
            },
            mode: TaskMode::Chat,
            input: "原稿仍保留在原生编辑器".into(),
            ui_draft: "原稿仍保留在原生编辑器".into(),
            reference_keys: vec!["retained-reference".into()],
            selected: None,
            generation: None,
        };
        *fixture.runtime.view.lock().unwrap() = RuntimeView {
            loaded: true,
            phase: "ready".into(),
            ..Default::default()
        };
        fixture
            .runtime
            .queue_submission_with(intent.clone(), || Ok(()))
            .unwrap();
        intent
    }

    fn oversized_submission_payload() -> Value {
        let input = "\u{1}".repeat(200_000);
        json!({
            "method":"runtime.submit", "taskId":"size-fixture",
            "networkPermit":"isolated-placeholder", "mode":"chat",
            "input":input, "displayText":input,
        })
    }

    #[test]
    fn oversized_frame_refuses_only_the_queued_input_and_exact_encoded_retry_is_sent() {
        let fixture = StartupFixture::new(Duration::from_secs(60));
        let intent = queued_size_fixture(&fixture, "size-fixture");
        let posted_before = fixture.posted.lock().unwrap().len();
        let result = fixture
            .runtime
            .encode_command(oversized_submission_payload())
            .map(|_| ());
        assert!(matches!(&result, Err(SubmissionError::RejectedInput(_))));
        fixture
            .runtime
            .finish_submission_preparation(intent.clone(), result);
        assert_eq!(fixture.runtime.view().phase, "ready");
        assert!(!fixture.runtime.view().task_running);
        assert!(
            fixture
                .runtime
                .view()
                .task_title
                .contains("runtime.inputTooLarge")
        );
        assert!(fixture.runtime.view().error.is_empty());
        assert!(!fixture.runtime.revoked.load(Ordering::Acquire));
        assert!(fixture.runtime.submission.lock().unwrap().is_none());
        assert!(
            fixture
                .runtime
                .accepted_submission
                .lock()
                .unwrap()
                .is_none()
        );
        assert!(fixture.runtime.pending.lock().unwrap().is_empty());
        assert_eq!(fixture.posted.lock().unwrap().len(), posted_before);
        assert_eq!(fixture.closes.load(Ordering::Acquire), 0);

        // The unchanged draft/reference snapshot can be queued again. The only
        // production path which clears the editor still requires acceptance.
        fixture
            .runtime
            .queue_submission_with(intent.clone(), || Ok(()))
            .unwrap();
        let retained = fixture.runtime.submission.lock().unwrap().clone().unwrap();
        assert_eq!(retained.ui_draft, intent.ui_draft);
        assert_eq!(retained.reference_keys, intent.reference_keys);
        let command = fixture
            .runtime
            .encode_command(json!({
                "method":"runtime.submit", "taskId":intent.id,
                "networkPermit":"isolated-placeholder", "mode":"chat",
                "input":"原生完整工程上下文\n合法中文请求", "displayText":"合法中文请求",
            }))
            .unwrap();
        let admitted_bytes = command.serialized.clone();
        let admitted_id = command.id.clone();
        fixture
            .runtime
            .send_command(command, CommandKind::Submit)
            .unwrap();
        assert_eq!(
            fixture.posted.lock().unwrap().last().unwrap(),
            &admitted_bytes
        );
        let frame: Value = serde_json::from_str(&admitted_bytes).unwrap();
        assert_eq!(frame["component"], fixture.runtime.component());
        assert_eq!(frame["channel"], CHANNEL);
        assert_eq!(frame["id"], admitted_id);
        assert!(
            fixture
                .runtime
                .pending
                .lock()
                .unwrap()
                .contains_key(&admitted_id)
        );
        fixture.finish();
    }

    #[test]
    fn stop_between_frame_encoding_and_input_refusal_never_restores_readiness() {
        let fixture = StartupFixture::new(Duration::from_secs(60));
        let intent = queued_size_fixture(&fixture, "size-stop-race");
        let result = fixture
            .runtime
            .encode_command(oversized_submission_payload())
            .map(|_| ());
        assert!(matches!(&result, Err(SubmissionError::RejectedInput(_))));
        fixture.runtime.stop();
        fixture
            .runtime
            .finish_submission_preparation(intent, result);
        assert!(fixture.runtime.revoked.load(Ordering::Acquire));
        assert_eq!(fixture.runtime.view().phase, "stopping");
        assert!(!fixture.runtime.view().task_running);
        assert!(fixture.runtime.view().error.is_empty());
        assert!(
            !fixture
                .runtime
                .view()
                .task_title
                .contains("runtime.inputTooLarge")
        );
        assert!(fixture.runtime.submission.lock().unwrap().is_none());
        assert_eq!(fixture.closes.load(Ordering::Acquire), 1);
        fixture.finish();
    }

    #[test]
    fn input_refusal_cannot_hide_a_security_failure_or_clear_another_task() {
        for wrong_identity in [false, true] {
            let fixture = StartupFixture::new(Duration::from_secs(60));
            let mut intent = queued_size_fixture(&fixture, "current-native-task");
            let error = if wrong_identity {
                intent.id = "unrelated-native-task".into();
                SubmissionError::RejectedInput("输入过长。")
            } else {
                SubmissionError::Failed("原生工程身份不匹配。".into())
            };
            fixture
                .runtime
                .finish_submission_preparation(intent, Err(error));
            assert!(fixture.runtime.revoked.load(Ordering::Acquire));
            assert_eq!(fixture.runtime.view().phase, "stopping");
            assert!(!fixture.runtime.view().error.is_empty());
            assert_eq!(fixture.closes.load(Ordering::Acquire), 1);
            fixture.finish();
        }
    }

    fn prepare_completion_task(
        fixture: &StartupFixture,
        id: &str,
    ) -> (concat_agent::operations::Invocation, Value) {
        const PROJECT: &str = "bkcat-project:0123456789abcdef0123456789abcdef";
        if fixture.bridge.status().unwrap().phase == concat_agent::Phase::RecoveryRequired {
            fixture
                .bridge
                .confirm_recovery(
                    concat_agent::Reconciliation {
                        project_identity: PROJECT.into(),
                        project_revision: "2".into(),
                        session_revision: "1".into(),
                    },
                    concat_agent::RecoveryDecision::DiscardInterrupted,
                )
                .unwrap();
        }
        let project = crate::live::CurrentProject {
            project_id: PROJECT.into(),
            path: fixture
                .root
                .join("completion-project")
                .to_string_lossy()
                .into_owned(),
            session_id: 1,
            revision: 2,
        };
        std::fs::create_dir_all(&project.path).unwrap();
        let manifest = std::path::Path::new(&project.path).join("concat.json");
        if !manifest.exists() {
            std::fs::write(&manifest, b"isolated native completion evidence").unwrap();
        }
        fixture.runtime.native.tool_approval.begin_task().unwrap();
        crate::agent::tasks::tests::install_bound_edit_task(
            &fixture.runtime.native.tasks,
            project.clone(),
            id,
            9,
        );
        *fixture.runtime.submission.lock().unwrap() = Some(SubmissionIntent {
            languages: crate::agent::tasks::TaskLanguages::default(),
            model: None,
            id: id.into(),
            project: project.clone(),
            mode: TaskMode::Edit,
            input: "isolated completion request".into(),
            ui_draft: "isolated completion request".into(),
            reference_keys: Vec::new(),
            selected: None,
            generation: None,
        });
        *fixture.runtime.accepted_submission.lock().unwrap() = Some(AcceptedSubmission {
            task: id.into(),
            conversation: 9,
            submission: 13,
        });
        *fixture.runtime.view.lock().unwrap() = RuntimeView {
            loaded: true,
            phase: "ready".into(),
            task_running: true,
            task_title: "agent.runningEdit".into(),
            ..Default::default()
        };
        let invocation = concat_agent::operations::Invocation {
            conversation_id: 9,
            task_id: 7,
            call_id: format!("{id}-tool"),
        };
        let frame = json!({"jsonrpc":"2.0","method":"tools/call","params":{
        "name":"edit_apply_guarded","arguments":{"path":project.path,
            "expectedSession":1,"expectedRevision":2,"command":{
                "op":"addTextClip","trackId":"t1","start":1.0,"duration":2.0,
                "style":{"content":"isolated completion test"}}
        }}});
        (invocation, frame)
    }

    fn completion_approval(
        fixture: &StartupFixture,
        request: &str,
        mut frame: Value,
        invocation: concat_agent::operations::Invocation,
        decision: Option<bool>,
    ) -> Value {
        fixture
            .runtime
            .start_approval_operation(request.to_owned(), move |native| {
                // Use the real native task/project preflight before the approval
                // gate. Only the final editor dispatch is absent from this test.
                native.tasks.authorize_tool(&mut frame, Some(&invocation))?;
                match native.tool_approval.authorize(&frame, Some(&invocation)) {
                    Ok(Some(ticket)) => {
                        native
                            .tool_approval
                            .consume(&frame, Some(&invocation), &ticket)?;
                        Ok(json!({"approved":true}))
                    }
                    Ok(None) => Err("guarded fixture call had no approval ticket".into()),
                    Err(message) => Ok(json!({"approved":false,"message":message})),
                }
            });
        if let Some(approve) = decision {
            fixture.wait_for(|_| {
                fixture
                    .runtime
                    .native
                    .tool_approval
                    .view()
                    .unwrap()
                    .pending_id
                    .is_some()
            });
            let id = fixture
                .runtime
                .native
                .tool_approval
                .view()
                .unwrap()
                .pending_id
                .unwrap();
            fixture
                .runtime
                .native
                .tool_approval
                .decide_from_ui(&id, approve)
                .unwrap();
        }
        fixture.wait_for(|_| {
            fixture
                .posted
                .lock()
                .unwrap()
                .iter()
                .any(|raw| serde_json::from_str::<Value>(raw).unwrap()["id"] == request)
        });
        fixture
            .posted
            .lock()
            .unwrap()
            .iter()
            .find_map(|raw| {
                let value: Value = serde_json::from_str(raw).unwrap();
                (value["id"] == request).then_some(value["result"].clone())
            })
            .unwrap()
    }

    fn complete_fixture_task(fixture: &StartupFixture, id: &str, successful: bool) {
        fixture.message(json!({"event":"task.completed","taskId":id,
            "conversationId":9,"submissionId":13,
            "status":if successful { "done" } else { "unanswered" },
            "text":"original task terminal fixture"}));
        fixture.wait_for(|view| !view.task_running);
    }

    fn completion_pane(fixture: &StartupFixture) -> crate::panes::agent::AgentPane {
        let mut pane = crate::panes::agent::AgentPane::default();
        pane.readiness.provider = "isolated-provider".into();
        pane.readiness.model = "fixture-only".into();
        pane.readiness.credentials_ready = true;
        pane.readiness.calls_authorized = true;
        pane.apply_runtime(&fixture.runtime.view());
        pane
    }

    fn click_accessible_control(app: &crate::ui::App, label: &str) {
        click_accessible_control_at(app, label, 0.5);
    }

    fn click_accessible_control_at(app: &crate::ui::App, label: &str, vertical_fraction: f32) {
        click_accessible_item_at(
            app,
            label,
            slint::private_unstable_api::re_exports::AccessibleRole::Button,
            vertical_fraction,
        );
    }

    fn click_accessible_item_at(
        app: &crate::ui::App,
        label: &str,
        role: slint::private_unstable_api::re_exports::AccessibleRole,
        vertical_fraction: f32,
    ) {
        use slint::ComponentHandle;
        use slint::platform::{PointerEventButton, WindowEvent};
        use slint::private_unstable_api::re_exports::{AccessibleStringProperty, WindowInner};
        assert!(vertical_fraction > 0. && vertical_fraction < 1.);
        let agent = app.global::<crate::ui::AgentUi>();
        let composer = (
            agent.get_draft(),
            agent.get_composer_mode(),
            agent.get_status().can_send,
            agent.get_attachment_pending(),
        );
        app.window().take_snapshot().unwrap();
        assert_eq!(
            (
                agent.get_draft(),
                agent.get_composer_mode(),
                agent.get_status().can_send,
                agent.get_attachment_pending()
            ),
            composer,
            "rendering the real composer must preserve the published input and admission state"
        );
        let root = WindowInner::from_pub(app.window())
            .window_item_rc()
            .unwrap();
        let mut points = Vec::new();
        root.visit_descendants::<()>(|item| {
            if item.is_visible()
                && item.accessible_role() == role
                && item
                    .accessible_string_property(AccessibleStringProperty::Label)
                    .is_some_and(|value| value == label)
            {
                // Probe the actual hit area, including its top/bottom edge.
                // map_to_window maps the parent's coordinate space, so the
                // local point includes this item's own origin.
                let geometry = item.geometry();
                let mut point = geometry.center();
                point.y = geometry.origin.y + geometry.size.height * vertical_fraction;
                let point = item.map_to_window(point);
                points.push(slint::LogicalPosition::new(point.x, point.y));
            }
            std::ops::ControlFlow::Continue(())
        });
        assert_eq!(points.len(), 1, "one visible production control: {label}");
        let position = points[0];
        let viewport = app.window().size();
        let scale = app.window().scale_factor();
        assert!(
            position.x >= 0.
                && position.y >= 0.
                && position.x < viewport.width as f32 / scale
                && position.y < viewport.height as f32 / scale,
            "production control {label} must be inside the rendered viewport: {position:?}"
        );
        app.window()
            .dispatch_event(WindowEvent::PointerMoved { position });
        app.window().dispatch_event(WindowEvent::PointerPressed {
            position,
            button: PointerEventButton::Left,
        });
        app.window().dispatch_event(WindowEvent::PointerReleased {
            position,
            button: PointerEventButton::Left,
        });
    }

    #[test]
    #[cfg(all(target_os = "windows", not(feature = "bk-cat-native-probe")))]
    fn manual_project_activation_and_first_send_use_native_window_paths() {
        const CHILD: &str = "BK_CAT_STARTUP_UI_TEST_CHILD";
        if std::env::var_os(CHILD).is_none() {
            // Slint's event proxy is process-global. Keep this actual Shell
            // test isolated even under cargo test's shared-process runner. Both
            // production window surfaces exercise the same owner and send path.
            for surface in ["desktop", "desktop-first-send", "phone"] {
                let output_dir = std::env::temp_dir().join(format!(
                    "bk-cat-startup-{surface}-ui-output-{}",
                    random_capability().unwrap()
                ));
                std::fs::create_dir(&output_dir).unwrap();
                let stdout_path = output_dir.join("stdout.log");
                let stderr_path = output_dir.join("stderr.log");
                let mut child = std::process::Command::new(std::env::current_exe().unwrap())
                    .args([
                        "--exact",
                        "embedded::tests::manual_project_activation_and_first_send_use_native_window_paths",
                        "--nocapture",
                        "--test-threads=1",
                    ])
                    .env(CHILD, surface)
                    .stdout(std::fs::File::create(&stdout_path).unwrap())
                    .stderr(std::fs::File::create(&stderr_path).unwrap())
                    .spawn()
                    .unwrap();
                let deadline = Instant::now() + Duration::from_secs(30);
                let status = loop {
                    if let Some(status) = child.try_wait().unwrap() {
                        break status;
                    }
                    if Instant::now() >= deadline {
                        child.kill().unwrap();
                        let _ = child.wait();
                        panic!(
                            "isolated {surface} startup UI exceeded 30 seconds; retained diagnostics at {}\n{}\n{}",
                            output_dir.display(),
                            std::fs::read_to_string(&stdout_path).unwrap_or_default(),
                            std::fs::read_to_string(&stderr_path).unwrap_or_default()
                        );
                    }
                    std::thread::sleep(Duration::from_millis(10));
                };
                let stdout = std::fs::read_to_string(&stdout_path).unwrap();
                let stderr = std::fs::read_to_string(&stderr_path).unwrap();
                assert!(
                    status.success() && stdout.contains("test result: ok. 1 passed"),
                    "isolated {surface} startup UI failed; retained diagnostics at {}\n{stdout}\n{stderr}",
                    output_dir.display()
                );
                std::fs::remove_dir_all(output_dir).unwrap();
            }
            return;
        }

        std::thread::Builder::new()
            .stack_size(32 << 20)
            .spawn(|| {
                use concat_agent::configuration::{ChannelDraft, ModelProtocol};
                use concat_agent::credentials::{
                    CredentialError, CredentialScope, CredentialStatus, CredentialStore, Secret,
                };
                use concat_host::playback::{Playback, PlaybackEvents};
                use slint::ComponentHandle;
                use slint::platform::software_renderer::{
                    MinimalSoftwareWindow, RepaintBufferType,
                };
                use slint::platform::{EventLoopProxy, Key, Platform, WindowAdapter, WindowEvent};
                use std::cell::RefCell;
                use std::collections::VecDeque;
                use std::rc::Rc;
                // The window fixture has no system-vault item. Keep availability
                // stable through real asynchronous refreshes, for only this
                // random connection and its complete service origin. This store
                // cannot read, write or delete a credential or authorize a call.
                struct ReadinessCredentials(CredentialScope);
                impl CredentialStore for ReadinessCredentials {
                    fn read(
                        &self,
                        _scope: &CredentialScope,
                    ) -> Result<Option<Secret>, CredentialError> {
                        Err(CredentialError::Unavailable)
                    }
                    fn write(
                        &self,
                        _scope: &CredentialScope,
                        _secret: &Secret,
                    ) -> Result<(), CredentialError> {
                        Err(CredentialError::Unavailable)
                    }
                    fn delete(&self, _scope: &CredentialScope) -> Result<(), CredentialError> {
                        Err(CredentialError::Unavailable)
                    }
                    fn status(
                        &self,
                        scope: &CredentialScope,
                    ) -> Result<CredentialStatus, CredentialError> {
                        if scope == &self.0 {
                            Ok(CredentialStatus::Present)
                        } else {
                            Err(CredentialError::InvalidScope)
                        }
                    }
                }
                type UiJobs = Arc<Mutex<VecDeque<Box<dyn FnOnce() + Send>>>>;
                #[derive(Clone)]
                struct Proxy(UiJobs);
                impl EventLoopProxy for Proxy {
                    fn quit_event_loop(&self) -> Result<(), slint::EventLoopError> {
                        Ok(())
                    }
                    fn invoke_from_event_loop(
                        &self,
                        event: Box<dyn FnOnce() + Send>,
                    ) -> Result<(), slint::EventLoopError> {
                        self.0.lock().unwrap().push_back(event);
                        Ok(())
                    }
                }
                struct SoftwarePlatform {
                    window: Rc<MinimalSoftwareWindow>,
                    proxy: Proxy,
                }
                impl Platform for SoftwarePlatform {
                    fn create_window_adapter(
                        &self,
                    ) -> Result<Rc<dyn WindowAdapter>, slint::PlatformError> {
                        Ok(self.window.clone())
                    }
                    fn new_event_loop_proxy(&self) -> Option<Box<dyn EventLoopProxy>> {
                        Some(Box::new(self.proxy.clone()))
                    }
                }
                fn pump(jobs: &UiJobs, mut done: impl FnMut() -> bool) {
                    let deadline = Instant::now() + Duration::from_secs(5);
                    loop {
                        loop {
                            let event = jobs.lock().unwrap().pop_front();
                            match event {
                                Some(event) => event(),
                                None => break,
                            }
                        }
                        slint::platform::update_timers_and_animations();
                        if done() {
                            return;
                        }
                        assert!(
                            Instant::now() < deadline,
                            "real startup UI event did not settle"
                        );
                        std::thread::sleep(Duration::from_millis(5));
                    }
                }
                fn frame(runtime: &EmbeddedRuntime, mut value: Value) {
                    value["channel"] = json!(CHANNEL);
                    value["component"] = json!(runtime.component());
                    runtime.receive(value.to_string());
                }
                fn reply(
                    jobs: &UiJobs,
                    posted: &Arc<Mutex<Vec<String>>>,
                    runtime: &EmbeddedRuntime,
                    id: &str,
                    payload: Value,
                ) -> Value {
                    frame(runtime, json!({"id":id,"payload":payload}));
                    let mut found = None;
                    pump(jobs, || {
                        found = posted.lock().unwrap().iter().find_map(|raw| {
                            let value: Value = serde_json::from_str(raw).unwrap();
                            (value["component"] == runtime.component() && value["id"] == id)
                                .then_some(value)
                        });
                        found.is_some()
                    });
                    let found = found.unwrap();
                    assert!(
                        found.get("error").is_none(),
                        "native startup refusal: {found}"
                    );
                    found["result"].clone()
                }
                fn command(
                    jobs: &UiJobs,
                    posted: &Arc<Mutex<Vec<String>>>,
                    runtime: &EmbeddedRuntime,
                    method: &str,
                ) -> Value {
                    let mut found = None;
                    pump(jobs, || {
                        found = posted.lock().unwrap().iter().find_map(|raw| {
                            let value: Value = serde_json::from_str(raw).unwrap();
                            (value["component"] == runtime.component()
                                && value["payload"]["method"] == method)
                                .then_some(value)
                        });
                        found.is_some()
                    });
                    found.unwrap()
                }
                fn activate_to_ready_boundary(
                    jobs: &UiJobs,
                    posted: &Arc<Mutex<Vec<String>>>,
                    runtime: &EmbeddedRuntime,
                ) -> Value {
                    frame(runtime, json!({"event":"runtime.loaded"}));
                    let start = command(jobs, posted, runtime, "runtime.start");
                    let lease = reply(
                        jobs,
                        posted,
                        runtime,
                        "web:acquire",
                        json!({"method":"ownership.acquire","storageId":STORAGE_ID}),
                    );
                    let bound = |method: &str, mut payload: Value| {
                        payload["method"] = json!(method);
                        payload["ownerHandle"] = lease["handle"].clone();
                        payload["generation"] = lease["generation"].clone();
                        payload
                    };
                    let snapshot = reply(
                        jobs,
                        posted,
                        runtime,
                        "web:read",
                        bound("recovery.read", json!({})),
                    );
                    assert!(
                        snapshot["projectSession"].is_string()
                            && snapshot["projectRevision"].is_string()
                    );
                    frame(runtime, json!({"event":"runtime.phase","phase":"recovery"}));
                    frame(runtime, json!({"id":start["id"],"result":snapshot}));
                    let confirm = command(jobs, posted, runtime, "runtime.confirmRecovery");
                    let grant = reply(
                        jobs,
                        posted,
                        runtime,
                        "web:confirm",
                        bound(
                            "ownership.confirm",
                            json!({"snapshot":snapshot,"approval":confirm["payload"]["approval"]}),
                        ),
                    );
                    frame(runtime, json!({"id":confirm["id"],"result":grant}));
                    // A following phase frame is a mailbox barrier after the
                    // confirm handler's real native history/project inspection.
                    frame(
                        runtime,
                        json!({"event":"runtime.phase","phase":"activating"}),
                    );
                    pump(jobs, || runtime.view().phase == "activating");
                    assert_eq!(
                        runtime.bridge.status().unwrap().phase,
                        concat_agent::Phase::Ready
                    );
                    snapshot
                }
                struct QuietPlayback;
                impl PlaybackEvents for QuietPlayback {
                    fn position(&self, _: f64) {}
                    fn error(&self, _: String) {}
                }

                let jobs: UiJobs = Arc::new(Mutex::new(VecDeque::new()));
                slint::platform::set_platform(Box::new(SoftwarePlatform {
                    window: MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer),
                    proxy: Proxy(Arc::clone(&jobs)),
                }))
                .unwrap();
                let surface = std::env::var(CHILD).unwrap();
                let phone = surface == "phone";
                let first_send_only = surface == "desktop-first-send";
                let app = crate::ui::App::new().unwrap();
                crate::i18n::install(&app);
                app.set_phone(phone);
                app.window().set_size(slint::LogicalSize::new(
                    if phone { 600. } else { 1400. },
                    800.,
                ));
                app.set_on_start(false);
                app.show().unwrap();
                let root = std::env::temp_dir().join(format!(
                    "bk-cat-startup-real-project-{}",
                    random_capability().unwrap()
                ));
                std::fs::create_dir(&root).unwrap();
                let dirs = AppDirs::under(&root.join("app"));
                std::fs::create_dir_all(&dirs.config).unwrap();
                let store = crate::panes::agent_settings::configuration_store(&dirs);
                let saved = store
                    .save_channel(
                        &store.load().unwrap(),
                        None,
                        ChannelDraft {
                            label: "Isolated startup UI fixture".into(),
                            protocol: ModelProtocol::OpenAiCompletions,
                            endpoint: "https://startup-fixture.invalid/v1".into(),
                            model: "fixture-only".into(),
                            request_timeout_ms: 1_000,
                            max_output_tokens: 128,
                        },
                    )
                    .unwrap();
                store.select(&saved, Some(&saved.channels()[0].id)).unwrap();
                let channel = &saved.channels()[0];
                let origin = channel.service_origin().unwrap();
                let scope = CredentialScope::new(channel.id.as_str(), &origin).unwrap();
                let credential_store_fixture = Arc::new(ReadinessCredentials(scope.clone()));
                assert_eq!(
                    credential_store_fixture.status(&scope),
                    Ok(CredentialStatus::Present)
                );
                for other in [
                    CredentialScope::new(&random_capability().unwrap(), &origin).unwrap(),
                    CredentialScope::new(channel.id.as_str(), &format!("{origin}/other")).unwrap(),
                ] {
                    assert_eq!(
                        credential_store_fixture.status(&other),
                        Err(CredentialError::InvalidScope)
                    );
                }
                assert!(matches!(
                    credential_store_fixture.read(&scope),
                    Err(CredentialError::Unavailable)
                ));
                assert_eq!(
                    credential_store_fixture.delete(&scope),
                    Err(CredentialError::Unavailable)
                );
                let open_projects = concat_api::OpenProjects::default();
                let exporter = concat_host::export::Exporter::new();
                let bridge = Arc::new(
                    AgentBridge::acquire(&dirs, open_projects.clone(), exporter.clone()).unwrap(),
                );
                let studio = crate::studio::Studio::new(crate::host::Host {
                    titles: Arc::new(concat_host::Titles::new(&dirs)),
                    cutouts: Arc::new(concat_host::Cutouts::new(&dirs.data)),
                    stabilizers: Arc::new(concat_host::Stabilizers::new()),
                    brushes: Arc::new(concat_host::Brushes::new(&dirs.data)),
                    enhancers: Arc::new(concat_host::Enhancers::new(&dirs.data)),
                    reversers: Arc::new(concat_host::Reversers::new()),
                    dirs: dirs.clone(),
                    playback: Playback::start(Arc::new(QuietPlayback)).unwrap(),
                    monitor: concat_host::preview::Monitor::new(),
                    exporter,
                    transcriber: Arc::new(concat_speech::Transcriber::new()),
                    speech: Arc::new(concat_speech::Speech::new()),
                    server: None,
                    open_projects,
                    agent: Some(Arc::clone(&bridge)),
                });
                let shell = Rc::new(Shell {
                    app: app.as_weak(),
                    agent: RefCell::new(Some(Arc::clone(&bridge))),
                    embedded: RefCell::new(None),
                    agent_pane: RefCell::new(Default::default()),
                    agent_settings: RefCell::new(Default::default()),
                    studio: RefCell::new(studio),
                    models: crate::studio::Models::new(),
                });
                shell.agent_pane.borrow_mut().credential_store_fixture =
                    Some(credential_store_fixture);
                Shell::install(Rc::clone(&shell));
                app.on_settings_language_changed(crate::panes::settings::language_changed_from_ui);
                app.global::<crate::ui::Editor>()
                    .on_key_chord(crate::editor_key_chord);
                app.global::<crate::ui::Focus>()
                    .on_release(|| Shell::with(|_, app| app.invoke_blur()));
                // Keep the production key dispatcher and native edit/save
                // operations; unrelated menus and OS pickers are outside this
                // isolated window fixture.
                let menu_actions = Rc::new(RefCell::new(Vec::new()));
                let observed_menu_actions = Rc::clone(&menu_actions);
                app.on_app_menu_selected(move |action| {
                    observed_menu_actions.borrow_mut().push(action.to_string());
                    Shell::with(|shell, app| {
                        let mut studio = shell.studio.borrow_mut();
                        match action.as_str() {
                            "delete" => studio.delete_selected(),
                            "save" => studio.save(false),
                            _ => panic!("unexpected menu in isolated keyboard fixture: {action}"),
                        }
                        studio.publish(&app, &shell.models);
                    });
                });
                let posted = Arc::new(Mutex::new(Vec::new()));
                let callbacks: Arc<Mutex<Vec<RuntimeCallbacks>>> = Arc::new(Mutex::new(Vec::new()));
                let closes = Arc::new(AtomicU64::new(0));
                let captured_callbacks = Arc::clone(&callbacks);
                let captured_posted = Arc::clone(&posted);
                let captured_closes = Arc::clone(&closes);
                install_factory(move |_, value| {
                    captured_callbacks.lock().unwrap().push(value);
                    Ok(Box::new(StartupHost {
                        posted: Arc::clone(&captured_posted),
                        closes: Arc::clone(&captured_closes),
                    }))
                })
                .unwrap();
                let first = concat_host::projects::create(
                    root.to_str().unwrap(),
                    "First",
                    64,
                    64,
                    30,
                    1,
                    [0, 0, 0, 255],
                )
                .unwrap();
                let second = concat_host::projects::create(
                    root.to_str().unwrap(),
                    "Second",
                    64,
                    64,
                    30,
                    1,
                    [0, 0, 0, 255],
                )
                .unwrap();
                shell.studio.borrow_mut().open_project(first).unwrap();
                pump(&jobs, || true);
                start_for_window();
                if phone {
                    platform_foreground_changed(false);
                    platform_foreground_changed(true);
                    pump(&jobs, || true);
                }
                assert!(
                    shell.embedded.borrow().is_none(),
                    "actual project-open and startup callbacks must keep manual editing silent"
                );
                assert!(callbacks.lock().unwrap().is_empty());
                assert!(!shell.agent_pane.borrow().readiness.calls_authorized);
                let _timer = crate::agent::window::install(&app);
                pump(&jobs, || {
                    !app.global::<crate::ui::AgentSettingsUi>().get_busy()
                });
                resume_assistant_connection_after_settings();
                pump(&jobs, || true);
                assert!(shell.embedded.borrow().is_none(),
                    "initial settings publication must keep even a visible desktop rail silent");
                assert!(callbacks.lock().unwrap().is_empty());
                {
                    let mut pane = shell.agent_pane.borrow_mut();
                    // Credential availability is a fixture fact. The actual next
                    // preflight rejects by full-frame size before opening a vault.
                    pane.readiness.provider = "Isolated startup UI fixture".into();
                    pane.readiness.model = "fixture-only".into();
                    pane.readiness.credentials_ready = true;
                    pane.readiness.calls_authorized = false;
                    pane.configuration_error.clear();
                    pane.error.clear();
                    pane.publish(&app, bridge.status());
                }
                let agent = app.global::<crate::ui::AgentUi>();
                agent.set_composer_mode(0);
                if !phone && !first_send_only {
                    click_accessible_control(&app, crate::i18n::t("agent.collapseAssistant").as_str());
                    assert!(shell.embedded.borrow().is_none());
                    click_accessible_control(&app, crate::i18n::t("agent.expandAssistant").as_str());
                    let opened = shell.embedded.borrow().clone().unwrap();
                    assert_eq!(callbacks.lock().unwrap().len(), 1);
                    assert!(!opened.view().task_running);
                    assert!(opened.waiting.lock().unwrap().is_none());
                    assert!(opened.submission.lock().unwrap().is_none());
                    assert!(posted.lock().unwrap().is_empty());
                    assert!(!shell.agent_pane.borrow().readiness.calls_authorized);
                    click_accessible_control(&app, crate::i18n::t("agent.collapseAssistant").as_str());
                    click_accessible_control(&app, crate::i18n::t("agent.expandAssistant").as_str());
                    assert_eq!(callbacks.lock().unwrap().len(), 1);
                    assert_eq!(shell.embedded.borrow().as_ref().unwrap().component(), opened.component());
                }
                if phone {
                    let clip = {
                        let mut studio = shell.studio.borrow_mut();
                        let clip = studio.apply(concat_project::Command::AddTextClip {
                            track_id: None,
                            start: 0.,
                            style: None,
                            duration: Some(2.),
                            offset_y: None,
                            above: true,
                        }).unwrap();
                        studio.selection = vec![clip.clone()];
                        studio.publish(&app, &shell.models);
                        clip
                    };
                    let revision = shell.studio.borrow().session.as_ref().unwrap().revision();
                    let key = |text: slint::SharedString| {
                        app.window().dispatch_event(WindowEvent::KeyPressed { text: text.clone() });
                        app.window().dispatch_event(WindowEvent::KeyReleased { text });
                    };
                    let delete: slint::SharedString = char::from(Key::Delete).to_string().into();
                    assert_eq!(agent.get_view(), crate::ui::AgentView::Editor);
                    assert_eq!(agent.get_phone_sheet_state(), 0);
                    click_accessible_control_at(&app, crate::i18n::t("agent.phoneOpen").as_str(), 0.1);
                    assert_eq!(agent.get_phone_sheet_state(), 1);
                    let opened = shell.embedded.borrow().clone().unwrap();
                    assert_eq!(callbacks.lock().unwrap().len(), 1);
                    assert!(!opened.view().task_running);
                    assert!(opened.submission.lock().unwrap().is_none());
                    assert!(posted.lock().unwrap().is_empty());
                    assert!(!shell.agent_pane.borrow().readiness.calls_authorized);
                    assert_eq!(opened.tool_permission_view().unwrap().mode, 0);
                    assert!(!crate::typing(&app), "the launcher leaves focus on the native root");
                    for level in [1, 2] {
                        agent.set_phone_sheet_state(level);
                        key(delete.clone());
                        key(" ".into());
                        let studio = shell.studio.borrow();
                        assert!(studio.clip(&clip).is_some());
                        assert!(!studio.playing);
                        assert_eq!(studio.session.as_ref().unwrap().revision(), revision);
                        assert!(menu_actions.borrow().is_empty());
                    }
                    // The existing save exception still reaches the real
                    // project while the assistant owns foreground input.
                    app.window().dispatch_event(WindowEvent::KeyPressed {
                        text: char::from(Key::Control).to_string().into(),
                    });
                    key("s".into());
                    app.window().dispatch_event(WindowEvent::KeyReleased {
                        text: char::from(Key::Control).to_string().into(),
                    });
                    assert_eq!(&*menu_actions.borrow(), &["save"]);
                    click_accessible_control_at(&app, crate::i18n::t("agent.collapseAssistant").as_str(), 0.9);
                    assert_eq!(agent.get_phone_sheet_state(), 0);
                    assert!(!opened.revoked.load(Ordering::Acquire));
                    assert_eq!(closes.load(Ordering::Acquire), 0);
                    key(" ".into());
                    assert!(shell.studio.borrow().playing, "manual playback resumes after collapse");
                    key(" ".into());
                    assert!(!shell.studio.borrow().playing);
                    key(delete);
                    assert!(shell.studio.borrow().clip(&clip).is_none());
                    assert_eq!(&*menu_actions.borrow(), &["save", "delete"]);
                    // Let the authorized delete finish its real autosave before
                    // taking the later approval-isolation disk baseline.
                    let deleted_revision =
                        shell.studio.borrow().session.as_ref().unwrap().revision();
                    assert_eq!(
                        app.get_project_status().as_str(),
                        crate::i18n::t("common.unsavedChanges").as_str()
                    );
                    pump(&jobs, || {
                        app.get_project_status().as_str()
                            == crate::i18n::t("common.saved").as_str()
                    });
                    assert_eq!(
                        shell.studio.borrow().session.as_ref().unwrap().revision(),
                        deleted_revision
                    );
                    click_accessible_control_at(&app, crate::i18n::t("agent.phoneOpen").as_str(), 0.9);
                    assert_eq!(callbacks.lock().unwrap().len(), 1);
                    assert_eq!(
                        shell.embedded.borrow().as_ref().unwrap().component(),
                        opened.component(),
                        "reopening a panel must reuse its current native component"
                    );
                }
                agent.set_draft("明确首次发送并等待".into());
                click_accessible_control_at(&app, crate::i18n::t("agent.send").as_str(), if phone { 0.1 } else { 0.5 });
                let first_runtime = shell.embedded.borrow().clone().unwrap();
                pump(&jobs, || agent.get_status().busy);
                assert!(first_runtime.waiting.lock().unwrap().is_some());
                assert!(!first_runtime.view().task_running);
                assert!(first_runtime.submission.lock().unwrap().is_none());
                assert!(!shell.agent_pane.borrow().readiness.calls_authorized);
                assert_eq!(
                    submit_chat_from_ui("重复发送".into(), false).unwrap_err(),
                    "agent.alreadyWaiting"
                );
                assert!(agent.get_draft() == "明确首次发送并等待");
                click_accessible_control_at(&app, crate::i18n::t("agent.stop").as_str(), if phone { 0.9 } else { 0.5 });
                assert!(first_runtime.waiting.lock().unwrap().is_none());
                assert!(!shell.agent_pane.borrow().readiness.calls_authorized);
                frame(
                    &first_runtime,
                    json!({"event":"runtime.phase","phase":"ready"}),
                );
                assert!(posted.lock().unwrap().is_empty());
                (callbacks.lock().unwrap()[0].closed)("lifecycle.windowsClosed".into());
                pump(&jobs, || first_runtime.view().closed);
                assert_eq!(callbacks.lock().unwrap().len(), 1,
                    "stopping a task must not itself create a replacement");
                if phone {
                    agent.set_phone_sheet_state(0);
                    click_accessible_control(&app, crate::i18n::t("agent.phoneOpen").as_str());
                    assert_eq!(
                        callbacks.lock().unwrap().len(),
                        2,
                        "opening the phone assistant must reconnect without a separate control"
                    );
                    agent.set_phone_sheet_state(2);
                } else {
                    resume_assistant_connection_after_settings();
                    pump(&jobs, || true);
                    assert_eq!(callbacks.lock().unwrap().len(), 1,
                        "a late settings event after explicit stop must not reconnect");
                    click_accessible_control(&app, crate::i18n::t("agent.collapseAssistant").as_str());
                    click_accessible_control(&app, crate::i18n::t("agent.expandAssistant").as_str());
                    assert_eq!(callbacks.lock().unwrap().len(), 2,
                        "a fresh desktop open reconnects without a separate control");
                }
                let runtime = shell.embedded.borrow().clone().unwrap();
                let draft = "\u{1}".repeat(200_000);
                agent.set_draft(draft.clone().into());
                shell
                    .agent_pane
                    .borrow()
                    .publish(&app, runtime.bridge.status());
                click_accessible_control(&app, crate::i18n::t("agent.send").as_str());
                pump(&jobs, || runtime.view().waiting_for_ready);
                activate_to_ready_boundary(&jobs, &posted, &runtime);

                // Queue two real publish callbacks while still waiting. Neither is
                // delivered until the live view is ready. The second must not replay
                // stale waiting state and clear the first callback's authorization.
                runtime.publish();
                runtime.publish();
                let command_sequence = runtime.next.load(Ordering::Acquire);
                frame(&runtime, json!({"event":"runtime.phase","phase":"ready"}));
                let ready_deadline = Instant::now() + Duration::from_secs(5);
                while runtime.view().phase != "ready" {
                    assert!(Instant::now() < ready_deadline);
                    std::thread::sleep(Duration::from_millis(2));
                }
                let first_update = jobs.lock().unwrap().pop_front().unwrap();
                first_update();
                assert!(runtime.waiting.lock().unwrap().is_none());
                assert!(shell.agent_pane.borrow().readiness.calls_authorized);
                let second_update = jobs.lock().unwrap().pop_front().unwrap();
                second_update();
                assert!(
                    shell.agent_pane.borrow().readiness.calls_authorized,
                    "a stale waiting publication cannot revoke an admitted send"
                );
                pump(&jobs, || runtime.view().task_title.contains("runtime.inputTooLarge"));
                assert_eq!(
                    runtime.next.load(Ordering::Acquire),
                    command_sequence + 1,
                    "the real full-frame preparation must be attempted once"
                );
                assert!(
                    runtime.view().error.is_empty() && !runtime.revoked.load(Ordering::Acquire)
                );
                assert!(runtime.submission.lock().unwrap().is_none());
                assert!(
                    agent.get_draft().as_str() == draft,
                    "rejected input keeps the exact original draft"
                );
                assert!(!posted.lock().unwrap().iter().any(|raw| {
                    serde_json::from_str::<Value>(raw).unwrap()["payload"]["method"]
                        == "runtime.submit"
                }));
                frame(&runtime, json!({"event":"runtime.phase","phase":"ready"}));
                frame(&runtime, json!({"event":"runtime.phase","phase":"ready"}));
                pump(&jobs, || true);
                assert_eq!(runtime.next.load(Ordering::Acquire), command_sequence + 1);
                agent.set_draft("审批期间保留的未发送消息".into());

                // Exercise an actual native approval notification while the
                // assistant is hidden. The isolated task fixture provides no
                // model socket or credential-store access; the real task guard,
                // approval worker, window callback and refusal remain in use.
                let project = {
                    let studio = shell.studio.borrow();
                    let session = studio.session.as_ref().unwrap();
                    crate::live::CurrentProject {
                        project_id: session.project_identity().into(),
                        path: session.path().into(),
                        session_id: session.session_id(),
                        revision: session.revision(),
                    }
                };
                let manifest = std::path::Path::new(&project.path).join("concat.json");
                let project_before = std::fs::read(&manifest).unwrap();
                crate::agent::tasks::tests::install_bound_edit_task(
                    &runtime.native.tasks,
                    project.clone(),
                    "native-window-approval",
                    9,
                );
                runtime.native.tool_approval.begin_task().unwrap();
                // Match queue_submission: the installed native task is also
                // running in the live view. A delayed model-choice refresh from
                // the preceding rejected input checks this current pane state
                // when delivered, so it cannot treat this task as an idle one.
                // This projection does not grant or restore model permission.
                {
                    let mut view = runtime.view.lock().unwrap();
                    view.task_running = true;
                    view.task_title = "agent.runningEdit".into();
                }
                let authorized_before = shell.agent_pane.borrow().readiness.calls_authorized;
                shell.agent_pane.borrow_mut().apply_runtime(&runtime.view());
                assert_eq!(shell.agent_pane.borrow().readiness.calls_authorized, authorized_before);
                let mut previous_request = String::new();
                for response_id in [
                    "native-window-approval-first",
                    "native-window-approval-next",
                    "native-window-approval-focused-script",
                ] {
                    let focused_script = phone && response_id.ends_with("focused-script");
                    if phone {
                        agent.set_phone_sheet_state(if focused_script { 2 } else { 0 });
                        agent.set_phone_script(true);
                        if focused_script {
                            app.global::<crate::ui::ScriptUi>().set_ready(true);
                            click_accessible_item_at(
                                &app,
                                crate::i18n::t("script.editorHint").as_str(),
                                slint::private_unstable_api::re_exports::AccessibleRole::TextInput,
                                0.2,
                            );
                            assert!(crate::typing(&app), "the production script field must really own focus");
                        }
                    } else {
                        agent.set_sidebar_open(false);
                        agent.set_view(crate::ui::AgentView::Script);
                    }
                    let mut approval_frame = json!({
                        "jsonrpc":"2.0","method":"tools/call","params":{
                            "name":"edit_apply_guarded","arguments":{
                                "path":project.path,
                                "expectedSession":project.session_id,
                                "expectedRevision":project.revision,
                                "command":{"op":"addTextClip","trackId":"fixture-track",
                                    "start":0.0,"duration":1.0,
                                    "style":{"content":"等待用户决定的隔离验收标题"}}
                            }
                        }
                    });
                    runtime.start_approval_operation(response_id.into(), move |native| {
                        let invocation = concat_agent::operations::Invocation {
                            conversation_id: 9,
                            task_id: 7,
                            call_id: format!("{response_id}-call"),
                        };
                        native.tasks.authorize_tool(&mut approval_frame, Some(&invocation))?;
                        match native.tool_approval.authorize(&approval_frame, Some(&invocation)) {
                            Err(reason) => Ok(json!({"approved":false,"reason":reason})),
                            Ok(_) => Err("window refusal test unexpectedly granted an operation".into()),
                        }
                    });
                    pump(&jobs, || agent.get_tool_pending());
                    let request_id = agent.get_tool_request_id().to_string();
                    assert!(!request_id.is_empty() && request_id != previous_request);
                    previous_request = request_id.clone();
                    if response_id == "native-window-approval-first" {
                        // Exercise the actual production settings callback
                        // while a native approval and an immutable submission
                        // exist. Language changes only their display projection.
                        let prior_language = crate::i18n::current();
                        let prior_view = runtime.view();
                        let history = "Historical body 中文 {1} remains literal";
                        let summary = "The conversation history before this point was compacted into the following summary:\n\n<summary>\nHistorical summary 中文 {1}\n</summary>";
                        let languages = crate::agent::tasks::TaskLanguages::capture(Some("ja"));
                        let model = saved.channels()[0].clone();
                        let prior_submission = runtime.submission.lock().unwrap().replace(SubmissionIntent {
                            languages: languages.clone(), model: Some(model.clone()),
                            id: "native-window-approval".into(), project: project.clone(),
                            mode: TaskMode::Edit, input: history.into(), ui_draft: history.into(),
                            reference_keys: Vec::new(), selected: None, generation: None,
                        });
                        let presentation = json!({"conversationId":9, "messages":[
                            {"role":"user","text":history},
                            {"role":"compaction","text":summary}
                        ], "partialText":"", "busy":true});
                        runtime.accept_presentation(&presentation).unwrap();
                        assert_eq!(runtime.view().presentation.as_ref(), Some(&presentation));
                        let mut invalid_role = presentation.clone();
                        invalid_role["messages"][1]["role"] = json!("system");
                        assert_eq!(runtime.accept_presentation(&invalid_role).unwrap_err(), "runtime.protocolViolation");
                        let mut other_conversation = presentation.clone();
                        other_conversation["conversationId"] = json!(10);
                        runtime.accept_presentation(&other_conversation).unwrap();
                        assert_eq!(runtime.view().presentation.as_ref(), Some(&presentation));
                        {
                            let mut view = runtime.view.lock().unwrap();
                            view.task_title = "agent.runningEdit".into();
                            view.task_running = true;
                        }
                        shell.agent_pane.borrow_mut().apply_runtime(&runtime.view());
                        let sent_before = posted.lock().unwrap().len();
                        let choices_before = runtime.view().configuration;
                        for language in ["zh-Hans", "en"] {
                            let index = shell.studio.borrow().languages.iter()
                                .position(|entry| entry.code == language).unwrap() as i32;
                            // Ensure the first projection also runs when the
                            // fixture already started in this language.
                            if crate::i18n::current() == language {
                                refresh_language_from_ui();
                            }
                            app.invoke_settings_language_changed(index);
                            pump(&jobs, || agent.get_messages().row_data(0)
                                .is_some_and(|row| row.author.as_str() == crate::i18n::t("agent.roleUser").as_str())
                                && agent.get_messages().row_data(1).is_some_and(|row|
                                    row.author.as_str() == crate::i18n::t("agent.roleCompaction").as_str()
                                    && row.preview.as_str() == crate::i18n::t("agent.compactionPreview").as_str())
                                && agent.get_status().title.as_str() == crate::i18n::t("agent.runningEdit").as_str());
                            let row = agent.get_messages().row_data(0).unwrap();
                            assert_eq!(row.body.as_str(), history);
                            let compacted = agent.get_messages().row_data(1).unwrap();
                            assert_eq!(compacted.role, crate::ui::AgentMessageRole::Compaction);
                            assert_eq!(compacted.body.as_str(), summary);
                            assert!(compacted.collapsible && !compacted.pending);
                            assert_eq!(runtime.view().presentation.as_ref(), Some(&presentation));
                            assert_eq!(agent.get_tool_request_id().as_str(), request_id);
                            assert!(agent.get_tool_pending());
                            let expected = runtime.tool_permission_view().unwrap().pending_summary.unwrap();
                            assert_eq!(agent.get_tool_summary().as_str(), expected);
                            let submission = runtime.submission.lock().unwrap();
                            let submission = submission.as_ref().unwrap();
                            assert_eq!(submission.languages, languages);
                            assert_eq!(submission.model.as_ref().unwrap().id, model.id);
                            assert_eq!(submission.model.as_ref().unwrap().model, model.model);
                            assert_eq!(submission.input, history);
                            assert_eq!(posted.lock().unwrap().len(), sent_before,
                                "a language change must not send a runtime or model command");
                            assert_eq!(runtime.view().configuration, choices_before);
                            assert_eq!(shell.agent_pane.borrow().readiness.calls_authorized, authorized_before,
                                "language projection must preserve the active task's model permission");
                        }
                        // A UI-only refusal is not present in RuntimeView.
                        // Retranslating must not erase it or re-enable Send.
                        let prior_error = shell.agent_pane.borrow().error.clone();
                        shell.agent_pane.borrow_mut().error = "runtime.submissionUnconfirmed".into();
                        assert!(runtime.view().error.is_empty());
                        for language in ["zh-Hans", "en"] {
                            let index = shell.studio.borrow().languages.iter()
                                .position(|entry| entry.code == language).unwrap() as i32;
                            app.invoke_settings_language_changed(index);
                            pump(&jobs, || agent.get_status().error.as_str()
                                == crate::i18n::t("runtime.submissionUnconfirmed").as_str());
                            assert_eq!(shell.agent_pane.borrow().error, "runtime.submissionUnconfirmed");
                            assert!(!agent.get_status().can_send);
                            assert!(runtime.view().error.is_empty());
                            assert_eq!(posted.lock().unwrap().len(), sent_before);
                            assert_eq!(shell.agent_pane.borrow().readiness.calls_authorized, authorized_before,
                                "retranslating a local refusal must not revoke the active task");
                        }
                        shell.agent_pane.borrow_mut().error = prior_error;
                        *runtime.submission.lock().unwrap() = prior_submission;
                        *runtime.view.lock().unwrap() = prior_view.clone();
                        shell.agent_pane.borrow_mut().apply_runtime(&prior_view);
                        let index = shell.studio.borrow().languages.iter()
                            .position(|entry| entry.code == prior_language).unwrap() as i32;
                        app.invoke_settings_language_changed(index);
                        refresh_language_from_ui();
                        pump(&jobs, || true);
                        assert_eq!(posted.lock().unwrap().len(), sent_before);
                    }
                    if phone {
                        assert_eq!(agent.get_phone_sheet_state(), if focused_script { 2 } else { 1 });
                        assert!(!agent.get_phone_script());
                        assert_eq!(agent.get_view(), crate::ui::AgentView::Editor);
                        if focused_script {
                            app.window().take_snapshot().unwrap();
                            assert!(!crate::typing(&app), "a real native approval must release the replaced script field");
                            let back: slint::SharedString = char::from(Key::Back).to_string().into();
                            app.window().dispatch_event(WindowEvent::KeyPressed { text: back.clone() });
                            app.window().dispatch_event(WindowEvent::KeyReleased { text: back });
                            assert_eq!(agent.get_phone_sheet_state(), 1);
                            assert!(agent.get_tool_pending(), "Back only changes presentation of a real pending operation");
                        }
                        // Once this exact request has appeared, a native
                        // refresh must respect both reading the script and
                        // collapsing the drawer. Only a new request reveals it.
                        agent.set_phone_script(true);
                        crate::agent::window::refresh_tool_permission();
                        assert!(agent.get_phone_script());
                        assert_eq!(agent.get_phone_sheet_state(), 1);
                        click_accessible_control_at(&app, crate::i18n::t("agent.collapseAssistant").as_str(), 0.1);
                        crate::agent::window::refresh_tool_permission();
                        assert_eq!(agent.get_phone_sheet_state(), 0);
                        assert!(agent.get_phone_script());
                        assert!(agent.get_tool_pending());
                        assert_eq!(agent.get_tool_request_id().as_str(), request_id.as_str());
                        click_accessible_control_at(&app, crate::i18n::t("agent.phoneOpen").as_str(), 0.9);
                        assert!(agent.get_phone_script());
                        click_accessible_control_at(&app, crate::i18n::t("agent.phoneChat").as_str(), 0.1);
                        assert!(!agent.get_phone_script());
                    } else {
                        assert!(agent.get_sidebar_open());
                        assert_eq!(agent.get_view(), crate::ui::AgentView::Chat);
                        agent.set_sidebar_open(false);
                        agent.set_view(crate::ui::AgentView::Script);
                        crate::agent::window::refresh_tool_permission();
                        assert!(agent.get_sidebar_open());
                        assert_eq!(agent.get_view(), crate::ui::AgentView::Chat);
                    }
                    assert_eq!(shell.agent_pane.borrow().readiness.calls_authorized, authorized_before);
                    assert_eq!(runtime.tool_permission_view().unwrap().mode, 0);
                    assert_eq!(std::fs::read(&manifest).unwrap(), project_before);
                    click_accessible_control(&app, crate::i18n::t("agent.reject").as_str());
                    pump(&jobs, || {
                        !agent.get_tool_pending() && posted.lock().unwrap().iter().any(|raw| {
                            let value: Value = serde_json::from_str(raw).unwrap();
                            value["id"] == response_id && value["result"]["approved"] == false
                        })
                    });
                }
                assert_eq!(std::fs::read(&manifest).unwrap(), project_before);
                assert_eq!(
                    shell.studio.borrow().session.as_ref().unwrap().revision(),
                    project.revision
                );
                stop_from_ui();
                assert!(!shell.agent_pane.borrow().readiness.calls_authorized);
                (callbacks.lock().unwrap()[1].closed)("lifecycle.windowsClosed".into());
                pump(&jobs, || runtime.view().closed);

                for fault in ["configuration", "revision"] {
                    connect_from_ui().unwrap();
                    let changed = shell.embedded.borrow().clone().unwrap();
                    let draft = format!("等待时核对{fault}变化");
                    agent.set_draft(draft.clone().into());
                    submit_chat_from_ui(draft.clone(), false).unwrap();
                    activate_to_ready_boundary(&jobs, &posted, &changed);
                    let expected = if fault == "configuration" {
                        let captured = changed
                            .waiting
                            .lock()
                            .unwrap()
                            .as_ref()
                            .unwrap()
                            .intent
                            .model
                            .clone()
                            .unwrap();
                        let current = store.load().unwrap();
                        // Change the waiting task's captured connection, not
                        // whichever connection is now the application default.
                        let channel = current.channel(&captured.id).unwrap();
                        store
                            .save_channel(
                                &current,
                                Some(&channel.id),
                                ChannelDraft {
                                    label: channel.label.clone(),
                                    protocol: channel.protocol,
                                    endpoint: channel.endpoint.clone(),
                                    model: channel.model.clone(),
                                    request_timeout_ms: 1_000,
                                    max_output_tokens: 256,
                                },
                            )
                            .unwrap();
                        store
                            .assert_connection_current(&captured)
                            .unwrap_err()
                            .to_string()
                    } else {
                        shell
                            .studio
                            .borrow_mut()
                            .session
                            .as_mut()
                            .unwrap()
                            .apply(concat_project::Command::AddTrack { bottom: false })
                            .unwrap();
                        "runtime.stateChanged".into()
                    };
                    let sequence = changed.next.load(Ordering::Acquire);
                    frame(&changed, json!({"event":"runtime.phase","phase":"ready"}));
                    pump(&jobs, || changed.revoked.load(Ordering::Acquire));
                    assert_eq!(changed.view().error, expected);
                    assert!(changed.waiting.lock().unwrap().is_none());
                    assert!(changed.submission.lock().unwrap().is_none());
                    assert_eq!(
                        changed.next.load(Ordering::Acquire),
                        sequence,
                        "changed binding must fail before normal preparation"
                    );
                    assert!(!shell.agent_pane.borrow().readiness.calls_authorized);
                    assert!(
                        agent.get_draft().as_str() == draft,
                        "changed binding retains the draft"
                    );
                    (callbacks.lock().unwrap().last().unwrap().closed)(
                        "lifecycle.windowsClosed".into(),
                    );
                    pump(&jobs, || changed.view().closed);
                }

                connect_from_ui().unwrap();
                let switching = shell.embedded.borrow().clone().unwrap();
                agent.set_draft("等待中的原稿不能跨工程执行".into());
                submit_chat_from_ui(agent.get_draft().to_string(), false).unwrap();
                shell.studio.borrow_mut().open_project(second).unwrap();
                pump(&jobs, || switching.revoked.load(Ordering::Acquire));
                if phone {
                    assert_eq!(agent.get_phone_sheet_state(), 0);
                    assert!(!agent.get_phone_script());
                }
                assert!(switching.waiting.lock().unwrap().is_none());
                let components_before_close = callbacks.lock().unwrap().len();
                if phone {
                    click_accessible_control(&app, crate::i18n::t("agent.phoneOpen").as_str());
                    assert_eq!(callbacks.lock().unwrap().len(), components_before_close);
                    assert_eq!(shell.embedded.borrow().as_ref().unwrap().component(), switching.component());
                    click_accessible_control(&app, crate::i18n::t("agent.collapseAssistant").as_str());
                }
                frame(&switching, json!({"event":"runtime.phase","phase":"ready"}));
                (callbacks.lock().unwrap().last().unwrap().closed)("lifecycle.windowsClosed".into());
                if phone {
                    pump(&jobs, || switching.view().closed);
                    assert_eq!(callbacks.lock().unwrap().len(), components_before_close,
                        "finishing the old component must not silently activate the new phone project");
                    assert_eq!(agent.get_phone_sheet_state(), 0);
                    assert!(!shell.agent_pane.borrow().readiness.calls_authorized);
                    click_accessible_control(&app, crate::i18n::t("agent.phoneOpen").as_str());
                    assert_eq!(callbacks.lock().unwrap().len(), components_before_close + 1);
                }
                pump(&jobs, || {
                    shell
                        .embedded
                        .borrow()
                        .as_ref()
                        .is_some_and(|current| current.component() != switching.component())
                });
                let replacement = shell.embedded.borrow().clone().unwrap();
                frame(&switching, json!({"event":"runtime.phase","phase":"ready"}));
                pump(&jobs, || true);
                assert!(replacement.waiting.lock().unwrap().is_none());
                assert!(replacement.submission.lock().unwrap().is_none());
                assert!(!shell.agent_pane.borrow().readiness.calls_authorized);
                assert!(agent.get_draft() == "等待中的原稿不能跨工程执行");
                let mut replacement = replacement;
                if phone {
                    // These are real native lifecycle/ownership paths with a
                    // controlled platform, not a claim about Android hardware.
                    for resume_before_close in [true, false] {
                        let retired = Arc::clone(&replacement);
                        let count = callbacks.lock().unwrap().len();
                        let draft = format!("后台前的原稿 {resume_before_close}");
                        agent.set_draft(draft.clone().into());
                        submit_chat_from_ui(draft.clone(), false).unwrap();
                        assert!(retired.waiting.lock().unwrap().is_some());
                        platform_foreground_changed(false);
                        (callbacks.lock().unwrap().last().unwrap().stopping)(
                            "lifecycle.activityPaused".into(),
                        );
                        assert!(retired.revoked.load(Ordering::Acquire));
                        assert!(retired.waiting.lock().unwrap().is_none());
                        assert_eq!(retired.bridge.status().unwrap().phase,
                            concat_agent::Phase::Draining);
                        if resume_before_close {
                            platform_foreground_changed(true);
                            pump(&jobs, || true);
                            assert_eq!(callbacks.lock().unwrap().len(), count,
                                "foreground resume is not proof of platform destruction");
                        }
                        (callbacks.lock().unwrap().last().unwrap().closed)(
                            "lifecycle.activityPaused".into(),
                        );
                        pump(&jobs, || retired.view().closed);
                        if !resume_before_close {
                            assert_eq!(callbacks.lock().unwrap().len(), count,
                                "a background app must not start a replacement");
                            platform_foreground_changed(true);
                        }
                        pump(&jobs, || callbacks.lock().unwrap().len() == count + 1);
                        assert_eq!(retired.bridge.status().unwrap().phase,
                            concat_agent::Phase::Released);
                        assert!(retired.view().error.is_empty());
                        replacement = shell.embedded.borrow().clone().unwrap();
                        assert_ne!(replacement.component(), retired.component());
                        assert!(replacement.waiting.lock().unwrap().is_none());
                        assert!(replacement.submission.lock().unwrap().is_none());
                        assert!(!replacement.view().task_running);
                        assert!(!shell.agent_pane.borrow().readiness.calls_authorized);
                        assert_eq!(replacement.tool_permission_view().unwrap().mode, 0);
                        assert_eq!(agent.get_draft().as_str(), draft.as_str());
                        let commands = posted.lock().unwrap().len();
                        frame(&retired, json!({"event":"runtime.phase","phase":"ready"}));
                        platform_foreground_changed(true);
                        platform_foreground_changed(true);
                        pump(&jobs, || true);
                        assert_eq!(callbacks.lock().unwrap().len(), count + 1,
                            "duplicate resume notifications reuse the new connection");
                        assert_eq!(posted.lock().unwrap().len(), commands,
                            "a late old callback cannot replay an interrupted send");
                    }
                    agent.set_phone_sheet_state(0);
                    let count = callbacks.lock().unwrap().len();
                    platform_foreground_changed(false);
                    (callbacks.lock().unwrap().last().unwrap().stopping)(
                        "lifecycle.activityPaused".into(),
                    );
                    (callbacks.lock().unwrap().last().unwrap().closed)(
                        "lifecycle.activityPaused".into(),
                    );
                    pump(&jobs, || replacement.view().closed);
                    platform_foreground_changed(true);
                    pump(&jobs, || true);
                    assert_eq!(callbacks.lock().unwrap().len(), count,
                        "returning to the manual editor keeps the assistant quiet");
                    click_accessible_control(&app, crate::i18n::t("agent.phoneOpen").as_str());
                    assert_eq!(callbacks.lock().unwrap().len(), count + 1);
                    replacement = shell.embedded.borrow().clone().unwrap();
                }
                // Real model-settings close callback reconnects only after
                // its dialog is gone; no separate connection button exists.
                let settings = app.global::<crate::ui::AgentSettingsUi>();
                settings.set_open(true);
                let count = callbacks.lock().unwrap().len();
                replacement.stop();
                (callbacks.lock().unwrap().last().unwrap().closed)(
                    "lifecycle.windowsClosed".into(),
                );
                pump(&jobs, || replacement.view().closed);
                resume_assistant_connection_after_settings();
                assert_eq!(callbacks.lock().unwrap().len(), count,
                    "an open settings dialog must not compete with maintenance");
                settings.invoke_close_requested();
                assert!(!settings.get_open());
                assert_eq!(callbacks.lock().unwrap().len(), count + 1);
                replacement = shell.embedded.borrow().clone().unwrap();
                replacement.stop();
                (callbacks.lock().unwrap().last().unwrap().closed)(
                    "lifecycle.windowsClosed".into(),
                );
                pump(&jobs, || replacement.view().closed && agent.get_status().can_connect);
                agent.set_draft("停止后明确发送新消息".into());
                click_accessible_control(&app, crate::i18n::t("agent.send").as_str());
                assert_eq!(callbacks.lock().unwrap().len(), count + 2,
                    "a new send also connects without reopening the assistant");
                replacement = shell.embedded.borrow().clone().unwrap();
                assert!(replacement.waiting.lock().unwrap().is_some());
                assert!(!replacement.view().task_running);
                let reopen = || {
                    if phone {
                        click_accessible_control(&app, crate::i18n::t("agent.collapseAssistant").as_str());
                        click_accessible_control(&app, crate::i18n::t("agent.phoneOpen").as_str());
                    } else {
                        click_accessible_control(&app, crate::i18n::t("agent.collapseAssistant").as_str());
                        click_accessible_control(&app, crate::i18n::t("agent.expandAssistant").as_str());
                    }
                };
                for cancel in [false, true] {
                    let retired = Arc::clone(&replacement);
                    let count = callbacks.lock().unwrap().len();
                    let draft = agent.get_draft();
                    stop_from_ui();
                    reopen();
                    assert_eq!(callbacks.lock().unwrap().len(), count,
                        "opening during retirement must await actual platform destruction");
                    assert_eq!(retired.bridge.status().unwrap().phase, concat_agent::Phase::Draining);
                    if cancel {
                        stop_from_ui();
                    }
                    (callbacks.lock().unwrap().last().unwrap().closed)("lifecycle.windowsClosed".into());
                    pump(&jobs, || retired.view().closed);
                    pump(&jobs, || true);
                    if cancel {
                        resume_assistant_connection_after_settings();
                        pump(&jobs, || true);
                        assert_eq!(callbacks.lock().unwrap().len(), count,
                            "stop cancels a pending open, including late close/settings events");
                        reopen();
                    }
                    pump(&jobs, || callbacks.lock().unwrap().len() == count + 1);
                    replacement = shell.embedded.borrow().clone().unwrap();
                    assert!(replacement.waiting.lock().unwrap().is_none());
                    assert!(replacement.submission.lock().unwrap().is_none());
                    assert!(!replacement.view().task_running);
                    assert!(!shell.agent_pane.borrow().readiness.calls_authorized);
                    assert_eq!(agent.get_draft(), draft, "reconnection never replays the stopped draft");
                }
                // A queued desktop project handoff must be cancellable before
                // its old platform-close publication reaches the UI loop.
                let third = concat_host::projects::create(
                    root.to_str().unwrap(), "Third", 64, 64, 30, 1, [0, 0, 0, 255],
                ).unwrap();
                let count = callbacks.lock().unwrap().len();
                shell.studio.borrow_mut().open_project(third).unwrap();
                pump(&jobs, || replacement.revoked.load(Ordering::Acquire));
                stop_from_ui();
                (callbacks.lock().unwrap().last().unwrap().closed)("lifecycle.windowsClosed".into());
                pump(&jobs, || replacement.view().closed);
                resume_assistant_connection_after_settings();
                pump(&jobs, || true);
                assert_eq!(callbacks.lock().unwrap().len(), count,
                    "a stopped project handoff cannot resurrect from its delayed close event");
                shell.studio.borrow_mut().close_project().unwrap();
                app.hide().unwrap();
                // Shell's thread-local owner drops at thread exit; the surrounding
                // child process then releases all isolated native fixtures.
                root
            })
            .unwrap()
            .join()
            .map(|root| {
                let _ = std::fs::remove_dir_all(root);
            })
            .unwrap();
    }

    #[test]
    fn preflight_rejection_completes_without_write_and_allows_the_next_native_request() {
        std::thread::Builder::new()
            .stack_size(32 << 20)
            .spawn(|| {
                use slint::ComponentHandle;
                use slint::platform::software_renderer::{
                    MinimalSoftwareWindow, RepaintBufferType,
                };
                let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
                slint::platform::set_platform(Box::new(
                    crate::agent::window::tests::NativeTestPlatform(window),
                ))
                .unwrap();
                let app = crate::ui::App::new().unwrap();
                crate::i18n::install(&app);
                app.set_phone(true);
                app.set_on_start(false);
                app.window().set_size(slint::LogicalSize::new(600., 800.));
                app.show().unwrap();
                app.global::<crate::ui::AgentUi>().set_phone_sheet_state(2);
                let sent = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
                let received = std::rc::Rc::clone(&sent);
                app.global::<crate::ui::AgentUi>()
                    .on_edit_requested(move |text| {
                        received.borrow_mut().push(text.to_string());
                    });
                for (index, op) in ["unsupported-style-operation", "addTextClip"]
                    .into_iter()
                    .enumerate()
                {
                    let fixture = StartupFixture::new(Duration::from_secs(30));
                    let (invocation, mut frame) =
                        prepare_completion_task(&fixture, "preflight-task");
                    let manifest = fixture.root.join("completion-project/concat.json");
                    let original = std::fs::read(&manifest).unwrap();
                    frame["params"]["arguments"]["command"]["op"] = json!(op);
                    if op == "addTextClip" {
                        frame["params"]["arguments"]["command"]["duration"] = json!(0);
                    }
                    let response = completion_approval(
                        &fixture,
                        "preflight",
                        frame.clone(),
                        invocation.clone(),
                        None,
                    );
                    assert_eq!(response["approved"], false);
                    let reason = response["message"].as_str().unwrap();
                    assert!(
                        fixture
                            .runtime
                            .native
                            .tool_approval
                            .view()
                            .unwrap()
                            .pending_id
                            .is_none()
                    );
                    let mut read = json!({"method":"tools/call","params":{"name":"timeline_read",
                    "arguments":{"path":frame["params"]["arguments"]["path"]}}});
                    fixture
                        .runtime
                        .native
                        .tasks
                        .authorize_tool(&mut read, Some(&invocation))
                        .unwrap();
                    assert_eq!(
                        fixture
                            .runtime
                            .native
                            .tool_approval
                            .authorize(&read, Some(&invocation))
                            .unwrap(),
                        None
                    );
                    complete_fixture_task(&fixture, "preflight-task", true);
                    assert!(fixture.runtime.view().error.is_empty());
                    assert_eq!(fixture.runtime.view().task_title, reason);
                    assert!(!fixture.runtime.revoked.load(Ordering::Acquire));
                    assert_eq!(fixture.closes.load(Ordering::Acquire), 0);
                    assert!(fixture.runtime.submission.lock().unwrap().is_none());
                    assert!(
                        fixture
                            .runtime
                            .accepted_submission
                            .lock()
                            .unwrap()
                            .is_none()
                    );
                    assert_eq!(std::fs::read(&manifest).unwrap(), original);

                    let pane = completion_pane(&fixture);
                    let next = format!("重新提交合法新增 {index}");
                    assert_eq!(
                        pane.validate_submission(fixture.bridge.status(), &next)
                            .unwrap(),
                        next
                    );
                    pane.publish(&app, fixture.bridge.status());
                    let agent = app.global::<crate::ui::AgentUi>();
                    assert!(agent.get_status().can_send);
                    assert_eq!(
                        agent.get_status().title,
                        crate::i18n::t("agent.actionRefused").as_str()
                    );
                    agent.set_composer_mode(1);
                    agent.set_draft(next.clone().into());
                    click_accessible_control(&app, crate::i18n::t("agent.send").as_str());
                    assert_eq!(
                        sent.borrow().last(),
                        Some(&next),
                        "the real composer must emit the edit request after the pointer click"
                    );

                    let (next_invocation, next_frame) =
                        prepare_completion_task(&fixture, "next-native-task");
                    assert!(
                        fixture
                            .runtime
                            .native
                            .tool_approval
                            .last_refusal(9)
                            .is_none()
                    );
                    let approved = completion_approval(
                        &fixture,
                        "next-approval",
                        next_frame,
                        next_invocation,
                        Some(true),
                    );
                    assert_eq!(
                        approved["approved"], true,
                        "the next task still requires and consumes real native consent"
                    );
                    assert_eq!(
                        std::fs::read(&manifest).unwrap(),
                        original,
                        "the fixture never dispatches an editor mutation"
                    );
                    fixture.finish();
                }
                app.hide().unwrap();
            })
            .unwrap()
            .join()
            .unwrap();
    }

    #[test]
    fn failed_submission_or_completion_retires_and_exposes_existing_recovery_controls() {
        std::thread::Builder::new()
            .stack_size(32 << 20)
            .spawn(|| {
                let _language = crate::i18n::language_lock();
                let previous_language = crate::i18n::current();
                let language_dirs = AppDirs::under(&std::env::temp_dir().join(format!(
                    "bkcut-completion-language-{}",
                    random_capability().unwrap()
                )));
                crate::i18n::select(crate::i18n::ENGLISH, &language_dirs);
                use slint::ComponentHandle;
                use slint::platform::software_renderer::{
                    MinimalSoftwareWindow, RepaintBufferType,
                };
                let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
                slint::platform::set_platform(Box::new(
                    crate::agent::window::tests::NativeTestPlatform(window),
                ))
                .unwrap();
                let app = crate::ui::App::new().unwrap();
                crate::i18n::install(&app);
                app.set_phone(true);
                app.set_on_start(false);
                app.window().set_size(slint::LogicalSize::new(600., 800.));
                app.show().unwrap();
                app.global::<crate::ui::AgentUi>().set_phone_sheet_state(2);
                let connects = std::rc::Rc::new(std::cell::Cell::new(0));
                let reviews = std::rc::Rc::new(std::cell::Cell::new(0));
                let received = std::rc::Rc::clone(&connects);
                app.global::<crate::ui::AgentUi>()
                    .on_assistant_open_requested(move || received.set(received.get() + 1));
                let received = std::rc::Rc::clone(&reviews);
                app.global::<crate::ui::AgentUi>()
                    .on_review_requested(move || received.set(received.get() + 1));
                for (index, case) in [
                    "unverified",
                    "consumed-before-refusal",
                    "existing-error",
                    "unanswered",
                    "prepare-failed",
                    "observe-failed",
                    "start-failed",
                    "queue-failed",
                ]
                .into_iter()
                .enumerate()
                {
                    let fixture = StartupFixture::new(Duration::from_secs(30));
                    let (invocation, frame) = prepare_completion_task(&fixture, "unsettled-task");
                    if case == "consumed-before-refusal" {
                        assert_eq!(
                            completion_approval(
                                &fixture,
                                "consumed",
                                frame.clone(),
                                invocation.clone(),
                                Some(true)
                            )["approved"],
                            true
                        );
                    }
                    if !matches!(case, "unverified" | "observe-failed" | "start-failed") {
                        let mut rejected = frame;
                        rejected["params"]["arguments"]["command"]["op"] =
                            json!("unsupported-style-operation");
                        let later = concat_agent::operations::Invocation {
                            call_id: "later-preview".into(),
                            ..invocation
                        };
                        assert_eq!(
                            completion_approval(&fixture, "later-rejection", rejected, later, None)
                                ["approved"],
                            false
                        );
                    }
                    let existing = "原生执行失败，必须核对当前工程。";
                    if case == "existing-error" {
                        fixture.runtime.record_live_error(existing);
                    }
                    if case == "prepare-failed" {
                        // A real native preparation failure has no task.completed
                        // event later. It must take the same safe retirement route.
                        fixture.runtime.prepare_submission();
                    } else if matches!(case, "observe-failed" | "start-failed") {
                        fixture.runtime.native.tasks.abandon();
                        fixture.runtime.submission.lock().unwrap().take();
                        fixture.runtime.accepted_submission.lock().unwrap().take();
                        fixture.runtime.view.lock().unwrap().task_running = false;
                        let kind = if case == "observe-failed" {
                            CommandKind::Observe
                        } else {
                            CommandKind::Start
                        };
                        if case == "start-failed" {
                            fixture.runtime.view.lock().unwrap().phase = "loading".into();
                        }
                        fixture
                            .runtime
                            .command(json!({"method":"runtime.fixture"}), kind)
                            .unwrap();
                        let request: Value =
                            serde_json::from_str(fixture.posted.lock().unwrap().last().unwrap())
                                .unwrap();
                        let error = if case == "observe-failed" {
                            "会话历史读取失败，请重新核对。"
                        } else {
                            "启动配置失败，请重新连接。"
                        };
                        fixture.message(json!({"id":request["id"],"error":{"message":error}}));
                    } else if case == "queue-failed" {
                        fixture.runtime.native.tasks.abandon();
                        let intent = fixture.runtime.submission.lock().unwrap().take().unwrap();
                        fixture.runtime.accepted_submission.lock().unwrap().take();
                        assert!(
                            fixture
                                .runtime
                                .queue_submission_with(intent, || Err(mpsc::TrySendError::Full(
                                    Work::Submit
                                )))
                                .is_err()
                        );
                    } else {
                        complete_fixture_task(&fixture, "unsettled-task", case != "unanswered");
                    }
                    fixture.wait_for(|_| fixture.closes.load(Ordering::Acquire) == 1);
                    assert!(fixture.runtime.revoked.load(Ordering::Acquire));
                    assert_eq!(
                        fixture.bridge.status().unwrap().phase,
                        concat_agent::Phase::Draining
                    );
                    assert!(
                        !fixture.runtime.view().closed,
                        "a close request is not proof of native destruction"
                    );
                    let error = fixture.runtime.view().error;
                    if case == "existing-error" {
                        assert_eq!(error, existing);
                    } else if case == "prepare-failed" {
                        assert!(error.contains("agent.errorRecoveryPending"));
                    } else if case == "observe-failed" {
                        assert!(error.contains("会话历史读取失败"));
                    } else if case == "start-failed" {
                        assert!(error.contains("启动配置失败"));
                    } else if case == "queue-failed" {
                        assert!(error.contains("runtime.queueFull"));
                    } else if case == "unanswered" {
                        assert!(error.contains("runtime.taskIncomplete"));
                    } else {
                        assert!(error.contains("runtime.editUnverified"));
                    }
                    assert!(
                        fixture
                            .runtime
                            .native
                            .tool_approval
                            .last_refusal(9)
                            .is_none()
                    );

                    (fixture.callbacks.closed)("lifecycle.windowsClosed".into());
                    fixture.wait_for(|view| view.closed);
                    assert_eq!(
                        fixture.bridge.status().unwrap().phase,
                        concat_agent::Phase::Released
                    );
                    assert_eq!(fixture.runtime.view().error, error);
                    let mut pane = completion_pane(&fixture);
                    pane.publish(&app, fixture.bridge.status());
                    let agent = app.global::<crate::ui::AgentUi>();
                    assert!(!agent.get_status().can_send);
                    assert!(agent.get_status().can_connect);
                    let expected_code = match case {
                        "existing-error" | "observe-failed" | "start-failed" => {
                            "agent.errorUnexpected"
                        }
                        "prepare-failed" => "agent.errorRecoveryPending",
                        "queue-failed" => "runtime.queueFull",
                        "unanswered" => "runtime.taskIncomplete",
                        _ => "runtime.editUnverified",
                    };
                    let expected_details = if expected_code == "agent.errorUnexpected" {
                        error.as_str()
                    } else {
                        let (code, details) = error.split_once('\n').unwrap_or((&error, ""));
                        assert_eq!(code, expected_code);
                        details
                    };
                    for language in [crate::i18n::ENGLISH, "zh-Hans"] {
                        crate::i18n::select(language, &language_dirs);
                        pane.publish(&app, fixture.bridge.status());
                        let status = agent.get_status();
                        assert_eq!(status.error_code.as_str(), expected_code);
                        assert_eq!(status.error.as_str(), crate::i18n::t(expected_code));
                        assert_eq!(status.error_details.as_str(), expected_details);
                        assert!(!status.can_send && status.can_connect);
                        assert_eq!(fixture.runtime.view().error, error);
                    }
                    crate::i18n::select(crate::i18n::ENGLISH, &language_dirs);
                    pane.publish(&app, fixture.bridge.status());
                    agent.set_draft("恢复前不能直接执行".into());
                    click_accessible_control(&app, crate::i18n::t("agent.send").as_str());
                    // Neither surface has a manual connection button. This
                    // fixture has no Shell; exercise the shared open callback.
                    // Both surfaces' reconnection is covered with a real Shell
                    // and actual open controls in the startup test above.
                    agent.invoke_assistant_open_requested();
                    assert_eq!(connects.get(), index + 1);

                    let replacement = AgentBridge::acquire(
                        &fixture.dirs,
                        concat_api::OpenProjects::default(),
                        concat_host::export::Exporter::new(),
                    )
                    .unwrap();
                    assert_eq!(
                        replacement.status().unwrap().phase,
                        concat_agent::Phase::RecoveryRequired
                    );
                    pane.apply_runtime(&RuntimeView {
                        loaded: true,
                        phase: "recovery".into(),
                        ..Default::default()
                    });
                    pane.publish(&app, replacement.status());
                    assert!(agent.get_status().can_review && !agent.get_status().can_send);
                    click_accessible_control(&app, crate::i18n::t("agent.reviewProject").as_str());
                    assert_eq!(reviews.get(), index + 1);
                    replacement.finish_handoff(Duration::from_secs(1)).unwrap();
                    drop(replacement);
                    fixture.finish();
                }
                app.hide().unwrap();
                crate::i18n::select(&previous_language, &language_dirs);
            })
            .unwrap()
            .join()
            .unwrap();
    }

    #[test]
    fn stopped_runtime_ignores_a_late_completion_failure() {
        let fixture = StartupFixture::new(Duration::from_secs(30));
        fixture.runtime.stop();
        let before = fixture.runtime.view();
        fixture.runtime.finish_task_presentation(true, true, None);
        assert_eq!(fixture.runtime.view().error, before.error);
        assert_eq!(fixture.closes.load(Ordering::Acquire), 1);
        fixture.finish();
    }

    #[test]
    fn completion_failure_and_stop_have_one_winner_before_any_new_error_is_published() {
        for stop_first in [true, false] {
            let fixture = StartupFixture::new(Duration::from_secs(30));
            if stop_first {
                let entered = Arc::new(std::sync::Barrier::new(2));
                let resume = Arc::new(std::sync::Barrier::new(2));
                let worker_entered = Arc::clone(&entered);
                let worker_resume = Arc::clone(&resume);
                let runtime = Arc::clone(&fixture.runtime);
                let worker = std::thread::spawn(move || {
                    runtime.finish_task_presentation_with(true, true, None, || {
                        worker_entered.wait();
                        worker_resume.wait();
                    });
                });
                entered.wait(); // Completion already observed a live runtime.
                fixture.runtime.stop();
                resume.wait();
                worker.join().unwrap();
                assert!(fixture.runtime.view().error.is_empty());
            } else {
                fixture.runtime.finish_task_presentation(true, true, None);
                fixture.runtime.stop();
                assert!(
                    fixture
                        .runtime
                        .view()
                        .error
                        .contains("runtime.editUnverified")
                );
            }
            let before = fixture.runtime.view().error;
            (fixture.callbacks.closed)("lifecycle.windowsClosed".into());
            fixture.wait_for(|view| view.closed);
            assert_eq!(fixture.runtime.view().error, before);
            assert_eq!(
                fixture.bridge.status().unwrap().phase,
                concat_agent::Phase::Released
            );
            fixture.finish();
        }
    }

    #[test]
    fn late_network_errors_after_stop_are_ignored_but_live_errors_still_reach_the_tool() {
        let fixture = StartupFixture::new(Duration::from_secs(30));
        let entered = Arc::new(std::sync::Barrier::new(2));
        let resume = Arc::new(std::sync::Barrier::new(2));
        let worker_entered = Arc::clone(&entered);
        let worker_resume = Arc::clone(&resume);
        let runtime = Arc::clone(&fixture.runtime);
        let worker = std::thread::spawn(move || {
            runtime.record_live_error_with("原生读取已因停止而取消。", || {
                worker_entered.wait();
                worker_resume.wait();
            });
        });
        entered.wait();
        fixture.runtime.stop();
        resume.wait();
        worker.join().unwrap();
        assert!(fixture.runtime.view().error.is_empty());
        (fixture.callbacks.closed)("lifecycle.windowsClosed".into());
        fixture.wait_for(|view| view.closed);
        assert!(fixture.runtime.view().error.is_empty());
        fixture.finish();

        let fixture = StartupFixture::new(Duration::from_secs(30));
        let original = "原生网络连接失败。";
        fixture.runtime.record_live_error(original);
        fixture
            .runtime
            .record_live_error("后续清理失败不能覆盖原始错误。");
        assert_eq!(fixture.runtime.view().error, original);
        fixture
            .runtime
            .reply_native("network-error", Err(original.into()));
        let reply: Value =
            serde_json::from_str(fixture.posted.lock().unwrap().last().unwrap()).unwrap();
        assert_eq!(reply["error"]["message"], original);
        assert!(!fixture.runtime.revoked.load(Ordering::Acquire));
        fixture.runtime.finish_task_presentation(false, false, None);
        assert!(fixture.runtime.revoked.load(Ordering::Acquire));
        (fixture.callbacks.closed)("lifecycle.windowsClosed".into());
        fixture.wait_for(|view| view.closed);
        assert_eq!(fixture.runtime.view().error, original);
        fixture.finish();
    }

    #[test]
    fn stop_wins_late_submit_error_and_success_receipts_without_hiding_live_failures() {
        for error_reply in [false, true] {
            let fixture = StartupFixture::new(Duration::from_secs(30));
            prepare_completion_task(&fixture, "submit-race");
            fixture.runtime.accepted_submission.lock().unwrap().take();
            fixture
                .runtime
                .command(json!({"method":"runtime.submit"}), CommandKind::Submit)
                .unwrap();
            let request: Value =
                serde_json::from_str(fixture.posted.lock().unwrap().last().unwrap()).unwrap();
            let mut reply = json!({"channel":CHANNEL,"component":fixture.runtime.component(),"id":request["id"]});
            if error_reply {
                reply["error"] = json!({"message":"submitted operation was refused"});
            } else {
                reply["result"] =
                    json!({"taskId":"submit-race","conversationId":9,"submissionId":13});
            }
            let authenticated =
                authenticate(&reply.to_string(), fixture.runtime.component()).unwrap();
            let entered = Arc::new(std::sync::Barrier::new(2));
            let resume = Arc::new(std::sync::Barrier::new(2));
            let worker_entered = Arc::clone(&entered);
            let worker_resume = Arc::clone(&resume);
            let runtime = Arc::clone(&fixture.runtime);
            let worker = std::thread::spawn(move || {
                runtime.handle_with(authenticated, || {
                    worker_entered.wait();
                    worker_resume.wait();
                });
            });
            entered.wait(); // A real pending command is already correlated.
            fixture.runtime.stop();
            resume.wait();
            worker.join().unwrap();
            assert!(fixture.runtime.view().error.is_empty());
            assert!(fixture.runtime.submission.lock().unwrap().is_none());
            assert!(
                fixture
                    .runtime
                    .accepted_submission
                    .lock()
                    .unwrap()
                    .is_none()
            );
            (fixture.callbacks.closed)("lifecycle.windowsClosed".into());
            fixture.wait_for(|view| view.closed);
            assert!(fixture.runtime.view().error.is_empty());
            fixture.finish();
        }

        let fixture = StartupFixture::new(Duration::from_secs(30));
        prepare_completion_task(&fixture, "live-submit-failure");
        fixture.runtime.accepted_submission.lock().unwrap().take();
        fixture
            .runtime
            .command(json!({"method":"runtime.submit"}), CommandKind::Submit)
            .unwrap();
        let request: Value =
            serde_json::from_str(fixture.posted.lock().unwrap().last().unwrap()).unwrap();
        fixture.message(json!({"id":request["id"],"error":{"message":"real submit refusal"}}));
        fixture.wait_for(|_| fixture.closes.load(Ordering::Acquire) == 1);
        assert!(
            fixture
                .runtime
                .view()
                .error
                .contains("runtime.submissionUnconfirmed")
        );
        assert!(fixture.runtime.revoked.load(Ordering::Acquire));
        fixture.finish();
    }

    #[test]
    fn a_crashed_approval_worker_is_still_reported_after_normal_stop() {
        let fixture = StartupFixture::new(Duration::from_secs(30));
        *fixture.runtime.approval_worker.lock().unwrap() = Some(std::thread::spawn(|| {
            panic!("isolated approval cleanup failure");
        }));
        fixture.runtime.stop();
        (fixture.callbacks.closed)("lifecycle.windowsClosed".into());
        fixture.wait_for(|view| view.closed);
        assert!(
            fixture
                .runtime
                .view()
                .error
                .contains("permission.stateUnavailable")
        );
        assert_eq!(
            fixture.bridge.status().unwrap().phase,
            concat_agent::Phase::Released
        );
        fixture.finish();
    }

    fn start_test_approval(fixture: &StartupFixture, call: &str) {
        let invocation = concat_agent::operations::Invocation {
            conversation_id: 9,
            task_id: 7,
            call_id: call.to_owned(),
        };
        fixture
            .runtime
            .start_approval_operation(call.to_owned(), move |native| {
                let frame = json!({"jsonrpc":"2.0","method":"tools/call","params":{
                "name":"edit_apply_guarded","arguments":{"path":"isolated-project",
                    "expectedSession":1,"expectedRevision":2,"command":{
                        "op":"addTextClip","trackId":"t1","start":1.0,"duration":2.0,
                        "style":{"content":"native mailbox approval test"}}
                }}});
                match native.tool_approval.authorize(&frame, Some(&invocation)) {
                    Ok(Some(ticket)) => {
                        // Consume the real one-shot grant without sending an edit.
                        native
                            .tool_approval
                            .consume(&frame, Some(&invocation), &ticket)?;
                        Ok(json!({"approved":true}))
                    }
                    Ok(None) => Err("test edit did not require admission".into()),
                    Err(message) => Ok(json!({"approved":false,"message":message})),
                }
            });
        fixture.wait_for(|_| {
            fixture
                .runtime
                .native
                .tool_approval
                .view()
                .unwrap()
                .pending_id
                .is_some()
        });
    }

    #[test]
    fn approval_wait_leaves_mailbox_live_and_reaps_before_delivering_each_decision() {
        let fixture = StartupFixture::new(Duration::from_secs(30));
        for (call, allow) in [("approval-first", true), ("approval-second", false)] {
            start_test_approval(&fixture, call);
            if allow {
                fixture.message(json!({"id":"during-approval","payload":{
                    "method":"ownership.acquire","storageId":STORAGE_ID}}));
                fixture.wait_for(|_| {
                    fixture.posted.lock().unwrap().iter().any(|raw| {
                        let frame: Value = serde_json::from_str(raw).unwrap();
                        frame["id"] == "during-approval" && frame["result"]["handle"].is_string()
                    })
                });
                assert!(
                    fixture
                        .runtime
                        .native
                        .tool_approval
                        .view()
                        .unwrap()
                        .pending_id
                        .is_some()
                );
            }
            let request = fixture
                .runtime
                .native
                .tool_approval
                .view()
                .unwrap()
                .pending_id
                .unwrap();
            fixture
                .runtime
                .native
                .tool_approval
                .decide_from_ui(&request, allow)
                .unwrap();
            fixture.wait_for(|_| {
                fixture.posted.lock().unwrap().iter().any(|raw| {
                    let frame: Value = serde_json::from_str(raw).unwrap();
                    frame["id"] == call && frame["result"]["approved"] == allow
                })
            });
            assert!(fixture.runtime.approval_worker.lock().unwrap().is_none());
            assert!(!fixture.runtime.revoked.load(Ordering::Acquire));
        }
        fixture.finish();
    }

    #[test]
    fn a_later_native_preflight_failure_discards_the_earlier_refusal_reason() {
        let fixture = StartupFixture::new(Duration::from_secs(30));
        start_test_approval(&fixture, "approval-denied");
        let request = fixture
            .runtime
            .native
            .tool_approval
            .view()
            .unwrap()
            .pending_id
            .unwrap();
        fixture
            .runtime
            .native
            .tool_approval
            .decide_from_ui(&request, false)
            .unwrap();
        fixture.wait_for(|_| {
            fixture.posted.lock().unwrap().iter().any(|raw| {
                let frame: Value = serde_json::from_str(raw).unwrap();
                frame["id"] == "approval-denied" && frame["result"]["approved"] == false
            })
        });
        assert!(
            fixture
                .runtime
                .native
                .tool_approval
                .last_refusal(9)
                .is_some()
        );

        let lease = fixture
            .runtime
            .native
            .handle(json!({
                "method":"ownership.acquire", "storageId":STORAGE_ID
            }))
            .unwrap();
        // This real native admission fails its readiness check before calling
        // tool_approval.authorize. The worker must still clear the old reason.
        fixture.runtime.start_approval_worker(json!({
            "id":"approval-preflight-failed", "payload":{
                "method":"mcp.authorize", "ownerHandle":lease["handle"],
                "generation":lease["generation"],
                "invocation":{"conversationId":9,"taskId":7,"callId":"later-write"},
                "message":{"jsonrpc":"2.0","method":"tools/call","params":{
                    "name":"edit_apply_guarded","arguments":{"path":"isolated-project",
                        "expectedSession":1,"expectedRevision":2,"command":{
                            "op":"addTextClip","trackId":"t1","start":1.0,"duration":2.0,
                            "style":{"content":"must not be written"}}
                    }}}
            }
        }));
        fixture.wait_for(|_| {
            fixture.posted.lock().unwrap().iter().any(|raw| {
                let frame: Value = serde_json::from_str(raw).unwrap();
                frame["id"] == "approval-preflight-failed"
            })
        });
        let response = fixture
            .posted
            .lock()
            .unwrap()
            .iter()
            .find_map(|raw| {
                let frame: Value = serde_json::from_str(raw).unwrap();
                (frame["id"] == "approval-preflight-failed").then_some(frame)
            })
            .unwrap();
        assert!(
            response.get("error").is_none(),
            "readiness refusal is a native result, not a transport failure"
        );
        assert_eq!(response["result"]["approved"], false);
        assert_eq!(response["result"]["message"], "agent.connectAndReview");
        assert!(
            fixture
                .runtime
                .native
                .tool_approval
                .last_refusal(9)
                .is_none()
        );
        assert!(
            fixture
                .runtime
                .native
                .tool_approval
                .view()
                .unwrap()
                .pending_id
                .is_none()
        );
        assert!(fixture.runtime.approval_worker.lock().unwrap().is_none());
        assert!(!fixture.runtime.revoked.load(Ordering::Acquire));
        fixture.finish();
    }

    #[test]
    fn platform_close_cancels_human_wait_before_releasing_native_ownership() {
        for reason in [
            "Native host closed",
            "lifecycle.activityPaused",
            "lifecycle.activitySaved",
        ] {
            let fixture = StartupFixture::new(Duration::from_secs(30));
            start_test_approval(&fixture, "approval-close");
            let pending = fixture
                .runtime
                .tool_permission_view()
                .unwrap()
                .pending_id
                .unwrap();
            (fixture.callbacks.stopping)(reason.into());
            assert_eq!(
                fixture.bridge.status().unwrap().phase,
                concat_agent::Phase::Draining
            );
            assert!(!fixture.runtime.view().closed);
            assert!(
                fixture.runtime.decide_tool_from_ui(&pending, true).is_err(),
                "foreground recovery can never approve the retired request"
            );
            (fixture.callbacks.closed)(reason.into());
            fixture.wait_for(|view| view.closed);
            assert!(fixture.runtime.approval_worker.lock().unwrap().is_none());
            assert!(
                fixture
                    .runtime
                    .tool_permission_view()
                    .unwrap()
                    .pending_id
                    .is_none()
            );
            assert_eq!(
                fixture.bridge.status().unwrap().phase,
                concat_agent::Phase::Released
            );
            assert!(fixture.posted.lock().unwrap().is_empty());
            if platform_reason_is_background(reason) {
                assert!(
                    fixture.runtime.view().error.is_empty(),
                    "normal Android pause must not become a permanent error"
                );
            }
            fixture.finish();
        }
    }

    #[test]
    fn task_terminal_requires_the_exact_original_acceptance_receipt() {
        // Drive the real authenticated native mailbox with synthetic original
        // receipts. No model, platform credential store or network is involved.
        for (receipt, changed_field) in [
            (false, None),
            (true, Some("submissionId")),
            (true, Some("conversationId")),
            (true, Some("taskId")),
        ] {
            let fixture = StartupFixture::new(Duration::from_secs(30));
            if receipt {
                *fixture.runtime.accepted_submission.lock().unwrap() = Some(AcceptedSubmission {
                    task: "native-task".into(),
                    conversation: 71,
                    submission: 83,
                });
            }
            let mut event = json!({"event":"task.completed","taskId":"native-task",
                "conversationId":71,"submissionId":83,"status":"done","text":"candidate"});
            if let Some(field) = changed_field {
                event[field] = if field == "taskId" {
                    json!("old-native-task")
                } else {
                    json!(99)
                };
            }
            fixture.message(event);
            fixture.wait_for(|view| view.error.contains("runtime.protocolViolation"));
            assert!(fixture.runtime.revoked.load(Ordering::Acquire));
            assert!(
                fixture
                    .runtime
                    .accepted_submission
                    .lock()
                    .unwrap()
                    .is_none()
            );
            assert!(fixture.posted.lock().unwrap().is_empty());
            assert!(!fixture.runtime.view().closed);
            assert_eq!(
                fixture.bridge.status().unwrap().phase,
                concat_agent::Phase::Draining
            );
            fixture.finish();
        }
    }

    #[test]
    fn stopped_task_discards_its_receipt_and_ignores_late_terminal_frames() {
        let fixture = StartupFixture::new(Duration::from_secs(30));
        *fixture.runtime.accepted_submission.lock().unwrap() = Some(AcceptedSubmission {
            task: "stopped-native-task".into(),
            conversation: 71,
            submission: 83,
        });
        fixture.runtime.stop();
        assert!(
            fixture
                .runtime
                .accepted_submission
                .lock()
                .unwrap()
                .is_none()
        );
        let before = fixture.runtime.view();
        fixture.message(
            json!({"event":"task.completed","taskId":"stopped-native-task",
            "conversationId":71,"submissionId":83,"status":"done","text":"late candidate"}),
        );
        assert_eq!(fixture.runtime.view().error, before.error);
        assert_eq!(fixture.closes.load(Ordering::Acquire), 1);
        assert!(fixture.posted.lock().unwrap().is_empty());
        assert!(!fixture.runtime.view().closed);
        fixture.finish();
    }

    #[test]
    fn silent_runtime_load_times_out_without_releasing_before_platform_destruction() {
        let fixture = StartupFixture::new(Duration::from_millis(30));
        fixture
            .wait_for(|view| !view.error.is_empty() && fixture.closes.load(Ordering::Acquire) == 1);
        assert_eq!(fixture.runtime.view().error, LOAD_TIMEOUT_MESSAGE);
        assert!(!fixture.runtime.view().loaded);
        assert!(!fixture.runtime.view().closed);
        assert_eq!(fixture.closes.load(Ordering::Acquire), 1);
        assert_eq!(
            fixture.bridge.status().unwrap().phase,
            concat_agent::Phase::Draining
        );
        assert!(
            AgentBridge::acquire(
                &fixture.dirs,
                concat_api::OpenProjects::default(),
                concat_host::export::Exporter::new(),
            )
            .is_err(),
            "a close request alone cannot transfer the real storage lock"
        );
        fixture.message(json!({"event":"runtime.loaded"}));
        assert!(
            !fixture.runtime.view().loaded,
            "late loaded cannot resurrect a retired owner"
        );
        assert!(
            fixture.posted.lock().unwrap().is_empty(),
            "no runtime.start or model call was sent"
        );
        (fixture.callbacks.closed)("Native host closed".into());
        fixture.wait_for(|view| view.closed);
        assert_eq!(
            fixture.runtime.view().error,
            LOAD_TIMEOUT_MESSAGE,
            "generic teardown preserves the actionable error"
        );
        let replacement = AgentBridge::acquire(
            &fixture.dirs,
            concat_api::OpenProjects::default(),
            concat_host::export::Exporter::new(),
        )
        .expect("actual platform destruction drains and releases ownership");
        drop(replacement);
        fixture.finish();
    }

    #[test]
    fn startup_failure_codes_publish_stable_errors_without_untrusted_diagnostics() {
        for code in ["weak-references", "script-execution", "credential-secret"] {
            let fixture = StartupFixture::new(Duration::from_secs(30));
            fixture.message(json!({"event":"runtime.loadFailed","code":code,
                "message":"credential-secret","filename":"private-project-path"}));
            fixture.wait_for(|view| {
                !view.error.is_empty() && fixture.closes.load(Ordering::Acquire) == 1
            });
            let error = fixture.runtime.view().error;
            assert!(!error.contains("credential-secret"));
            assert!(!error.contains("private-project-path"));
            assert!(matches!(
                error.as_str(),
                "runtime.webCapability" | "runtime.scriptExecution" | "runtime.protocolViolation"
            ));
            assert_eq!(fixture.closes.load(Ordering::Acquire), 1);
            assert!(fixture.posted.lock().unwrap().is_empty());
            (fixture.callbacks.closed)("Native host closed".into());
            fixture.wait_for(|view| view.closed);
            assert_eq!(fixture.runtime.view().error, error);
            fixture.finish();
        }
    }

    #[test]
    fn activation_failure_is_preserved_through_platform_teardown() {
        let fixture = StartupFixture::new(Duration::from_secs(30));
        fixture.message(json!({"event":"runtime.failed", "stage":"activate",
            "message":"原版 Pi 会话存储初始化失败"}));
        fixture.wait_for(|view| view.phase == "stopping" && !view.error.is_empty());
        assert_eq!(
            fixture.runtime.view().error,
            "runtime.startupFailed\n原版 Pi 会话存储初始化失败"
        );
        assert_eq!(fixture.closes.load(Ordering::Acquire), 1);
        (fixture.callbacks.closed)("lifecycle.windowsClosed".into());
        fixture.wait_for(|view| view.closed);
        assert_eq!(
            fixture.runtime.view().error,
            "runtime.startupFailed\n原版 Pi 会话存储初始化失败"
        );
        fixture.finish();
    }

    #[test]
    fn stop_wins_native_replies_already_waiting_for_status_or_platform_send() {
        for boundary in ["owner-drained", "owner-error", "platform-error"] {
            let fixture = StartupFixture::new(Duration::from_secs(30));
            let runtime = Arc::clone(&fixture.runtime);
            let bridge = Arc::clone(&fixture.bridge);
            let (entered, at_boundary) = mpsc::channel();
            let (resume, resumed) = mpsc::channel();
            let reply = std::thread::spawn(move || {
                if boundary == "platform-error" {
                    runtime.reply_native_via(
                        "in-flight",
                        Ok(Value::Null),
                        || bridge.status(),
                        |_| {
                            entered.send(()).unwrap();
                            resumed.recv_timeout(Duration::from_secs(5)).unwrap();
                            Err("平台已停止接收消息".into())
                        },
                    );
                } else {
                    runtime.reply_native_via(
                        "in-flight",
                        Ok(Value::Null),
                        || {
                            entered.send(()).unwrap();
                            resumed.recv_timeout(Duration::from_secs(5)).unwrap();
                            if boundary == "owner-drained" {
                                bridge.status()
                            } else {
                                Err("原生状态读取失败".into())
                            }
                        },
                        |_| panic!("a stopped reply must not reach the platform"),
                    );
                }
            });
            // The reply has passed its first live check. Stop completes before
            // its status/read or send error is allowed to return.
            at_boundary.recv_timeout(Duration::from_secs(5)).unwrap();
            fixture.runtime.stop();
            resume.send(()).unwrap();
            reply.join().unwrap();
            assert!(fixture.runtime.view().error.is_empty(), "{boundary}");
            assert!(fixture.posted.lock().unwrap().is_empty());
            assert_eq!(fixture.closes.load(Ordering::Acquire), 1);
            (fixture.callbacks.closed)("lifecycle.windowsClosed".into());
            fixture.wait_for(|view| view.closed);
            assert!(fixture.runtime.view().error.is_empty());
            fixture.finish();
        }
    }

    #[test]
    fn stop_wins_an_approval_enqueue_error_after_its_initial_live_check() {
        let fixture = StartupFixture::new(Duration::from_secs(30));
        let runtime = Arc::clone(&fixture.runtime);
        let (entered, at_boundary) = mpsc::channel();
        let (resume, resumed) = mpsc::channel();
        let delivery = std::thread::spawn(move || {
            runtime.dispatch_approval_result(
                Work::ApprovalFinished {
                    id: "late-approval".into(),
                    result: Ok(json!({"approved":false})),
                },
                |work| {
                    entered.send(()).unwrap();
                    resumed.recv_timeout(Duration::from_secs(5)).unwrap();
                    Err(mpsc::TrySendError::Disconnected(work))
                },
            );
        });
        at_boundary.recv_timeout(Duration::from_secs(5)).unwrap();
        fixture.runtime.stop();
        resume.send(()).unwrap();
        delivery.join().unwrap();
        assert!(fixture.runtime.view().error.is_empty());
        assert_eq!(fixture.closes.load(Ordering::Acquire), 1);
        (fixture.callbacks.closed)("lifecycle.windowsClosed".into());
        fixture.wait_for(|view| view.closed);
        assert!(fixture.runtime.view().error.is_empty());
        fixture.finish();
    }

    #[test]
    fn live_reply_and_enqueue_failures_still_retire_with_their_real_error() {
        for boundary in [
            "owner-drained",
            "owner-error",
            "platform-error",
            "approval-enqueue",
        ] {
            let fixture = StartupFixture::new(Duration::from_secs(30));
            let expected = match boundary {
                "owner-drained" => {
                    // Native revocation without runtime stop remains a fault.
                    fixture.bridge.stop();
                    fixture.runtime.reply_native("live", Ok(Value::Null));
                    "agent.connectAndReview"
                }
                "owner-error" => {
                    fixture.runtime.reply_native_via(
                        "live",
                        Ok(Value::Null),
                        || Err("原生状态读取失败".into()),
                        |_| panic!("invalid ownership cannot send a reply"),
                    );
                    "原生状态读取失败"
                }
                "platform-error" => {
                    fixture.runtime.reply_native_via(
                        "live",
                        Ok(Value::Null),
                        || fixture.bridge.status(),
                        |_| Err("平台发送失败".into()),
                    );
                    "平台发送失败"
                }
                _ => {
                    fixture.runtime.dispatch_approval_result(
                        Work::ApprovalFinished {
                            id: "live-approval".into(),
                            result: Ok(Value::Null),
                        },
                        |work| Err(mpsc::TrySendError::Full(work)),
                    );
                    "runtime.approvalDeliveryFailed"
                }
            };
            assert_eq!(fixture.runtime.view().error, expected);
            assert!(fixture.runtime.revoked.load(Ordering::Acquire));
            assert_eq!(fixture.closes.load(Ordering::Acquire), 1);
            (fixture.callbacks.closed)("lifecycle.windowsClosed".into());
            fixture.wait_for(|view| view.closed);
            assert_eq!(fixture.runtime.view().error, expected);
            fixture.finish();
        }
    }

    #[test]
    fn native_approval_waiter_observes_retirement_when_cancel_wakes_it() {
        for project_switch in [false, true] {
            let fixture = StartupFixture::new(Duration::from_secs(30));
            let weak = Arc::downgrade(&fixture.runtime);
            let (observed, observation) = mpsc::channel();
            fixture
                .runtime
                .start_approval_operation("cancel-order".into(), move |native| {
                    let invocation = concat_agent::operations::Invocation {
                        conversation_id: 9,
                        task_id: 7,
                        call_id: "cancel-order".into(),
                    };
                    let frame = json!({"jsonrpc":"2.0","method":"tools/call","params":{
                    "name":"edit_apply_guarded","arguments":{"path":"isolated-project",
                        "expectedSession":1,"expectedRevision":2,"command":{
                            "op":"addTextClip","trackId":"t1","start":1.0,"duration":2.0,
                            "style":{"content":"cancel ordering test"}}
                    }}});
                    let result = native.tool_approval.authorize(&frame, Some(&invocation));
                    observed
                        .send(weak.upgrade().unwrap().revoked.load(Ordering::Acquire))
                        .unwrap();
                    result.map(|_| Value::Null)
                });
            fixture.wait_for(|_| {
                fixture
                    .runtime
                    .native
                    .tool_approval
                    .view()
                    .unwrap()
                    .pending_id
                    .is_some()
            });
            if project_switch {
                stop_project_runtime(&fixture.bridge, Some(&fixture.runtime));
            } else {
                fixture.runtime.stop();
            }
            assert!(observation.recv_timeout(Duration::from_secs(5)).unwrap());
            (fixture.callbacks.closed)("lifecycle.windowsClosed".into());
            fixture.wait_for(|view| view.closed);
            assert!(fixture.runtime.view().error.is_empty());
            assert!(fixture.runtime.approval_worker.lock().unwrap().is_none());
            assert!(fixture.posted.lock().unwrap().is_empty());
            fixture.finish();
        }
    }

    #[test]
    fn normal_platform_close_cancels_approval_without_a_false_error_in_any_callback_order() {
        for (user_stop_first, notify_stopping) in [(true, true), (false, true), (false, false)] {
            let fixture = StartupFixture::new(Duration::from_secs(30));
            start_test_approval(&fixture, "normal-close");
            let request = fixture
                .runtime
                .native
                .tool_approval
                .view()
                .unwrap()
                .pending_id
                .unwrap();
            if user_stop_first {
                fixture.runtime.stop();
            }
            if notify_stopping {
                for _ in 0..2 {
                    (fixture.callbacks.stopping)("lifecycle.windowsStopping".into());
                    assert!(fixture.runtime.view().error.is_empty());
                    assert!(!fixture.runtime.view().closed);
                }
            }
            (fixture.callbacks.closed)("lifecycle.windowsClosed".into());
            fixture.wait_for(|view| view.closed);
            assert!(fixture.runtime.view().error.is_empty());
            assert_eq!(fixture.runtime.view().phase, "closed");
            assert!(fixture.runtime.approval_worker.lock().unwrap().is_none());
            assert!(
                fixture
                    .runtime
                    .native
                    .tool_approval
                    .view()
                    .unwrap()
                    .pending_id
                    .is_none()
            );
            assert!(fixture.runtime.decide_tool_from_ui(&request, true).is_err());
            assert!(fixture.posted.lock().unwrap().is_empty());

            // Late teardown notifications cannot recreate a red error or move
            // the completed terminal view back to an in-progress state.
            (fixture.callbacks.stopping)("lifecycle.windowsStopping".into());
            (fixture.callbacks.closed)("lifecycle.windowsClosed".into());
            assert!(fixture.runtime.view().error.is_empty());
            assert_eq!(fixture.runtime.view().phase, "closed");
            fixture.finish();
        }
    }

    #[test]
    fn normal_platform_callbacks_preserve_existing_errors_and_terminal_failures() {
        let fixture = StartupFixture::new(Duration::from_secs(30));
        fixture.runtime.view.lock().unwrap().error = "原生工程状态核对失败。".into();
        // This is the first revocation, so it also covers the old `first`
        // branch that could overwrite an already published task error.
        (fixture.callbacks.stopping)("lifecycle.windowsStopping".into());
        (fixture.callbacks.closed)("lifecycle.windowsClosed".into());
        fixture.wait_for(|view| view.closed);
        assert_eq!(fixture.runtime.view().error, "原生工程状态核对失败。");
        fixture.finish();

        for previous in ["", "lifecycle.windowsStopping", "lifecycle.windowsClosed"] {
            let fixture = StartupFixture::new(Duration::from_secs(30));
            fixture.runtime.view.lock().unwrap().error = previous.into();
            (fixture.callbacks.stopping)("lifecycle.windowsStopping".into());
            (fixture.callbacks.closed)("WebView2 process failed".into());
            fixture.wait_for(|view| view.closed);
            let failure = fixture.runtime.view().error;
            assert!(failure.contains("WebView2 process failed"));
            (fixture.callbacks.stopping)("lifecycle.windowsStopping".into());
            (fixture.callbacks.closed)("lifecycle.windowsClosed".into());
            assert_eq!(fixture.runtime.view().error, failure);
            assert_eq!(fixture.runtime.view().phase, "closed");
            fixture.finish();
        }
    }

    #[test]
    fn completed_native_cleanup_clears_only_placeholders_and_keeps_shutdown_errors() {
        for previous in [
            "",
            "lifecycle.windowsStopping",
            "lifecycle.windowsClosed",
            "原始运行错误",
        ] {
            let mut view = RuntimeView {
                error: previous.into(),
                ..RuntimeView::default()
            };
            finish_runtime_view(&mut view, Ok(()));
            assert!(view.closed);
            assert_eq!(view.phase, "closed");
            assert_eq!(
                view.error,
                if previous == "原始运行错误" {
                    previous
                } else {
                    ""
                }
            );

            // This is the production finalization branch, not a test copy of
            // its classification. Cleanup failure wins even after a clean
            // platform close or a previously recorded platform error.
            view.error = previous.into();
            finish_runtime_view(&mut view, Err("原生资源关闭失败".into()));
            assert!(view.closed);
            assert_eq!(view.phase, "closed");
            assert_eq!(view.error, "原生资源关闭失败");
            finish_runtime_view(&mut view, Ok(()));
            assert_eq!(view.error, "原生资源关闭失败");
        }
    }

    #[test]
    fn platform_terminal_reason_replaces_only_generic_placeholder() {
        assert!(terminal_reason_replaces_placeholder(
            "lifecycle.windowsStopping",
            "WebView2 initialisation timed out"
        ));
        assert!(!terminal_reason_replaces_placeholder(
            "智能体启动失败：原始错误",
            "WebView2 initialisation timed out"
        ));
        assert!(!terminal_reason_replaces_placeholder(
            "lifecycle.windowsStopping",
            "lifecycle.windowsClosed"
        ));
    }

    #[test]
    fn actual_loaded_event_cancels_only_the_loading_deadline_without_approving_recovery() {
        let fixture = StartupFixture::new(Duration::from_millis(500));
        fixture.message(json!({"event":"runtime.loaded"}));
        fixture.wait_for(|view| view.loaded);
        std::thread::sleep(Duration::from_millis(550));
        assert_eq!(fixture.closes.load(Ordering::Acquire), 0);
        assert!(fixture.runtime.view().error.is_empty());
        assert_eq!(
            fixture.bridge.status().unwrap().phase,
            concat_agent::Phase::RecoveryRequired
        );
        let posted = fixture.posted.lock().unwrap();
        assert_eq!(posted.len(), 1);
        let frame: Value = serde_json::from_str(&posted[0]).unwrap();
        assert_eq!(frame["payload"]["method"], "runtime.start");
        drop(posted);
        fixture.runtime.stop();
        fixture.finish();
    }

    #[test]
    fn lifecycle_retirement_wins_over_pending_load_and_late_failure() {
        let fixture = StartupFixture::new(Duration::from_millis(30));
        fixture.runtime.stop();
        fixture.message(json!({"event":"runtime.loadFailed","code":"script-execution"}));
        fixture.message(json!({"event":"runtime.loaded"}));
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(fixture.closes.load(Ordering::Acquire), 1);
        assert!(fixture.runtime.view().error.is_empty());
        assert!(!fixture.runtime.view().loaded);
        assert!(fixture.posted.lock().unwrap().is_empty());
        fixture.finish();
    }

    #[test]
    fn persisted_history_frames_fit_without_relaxing_origin_or_command_admission() {
        let body = serde_json::to_string(
            &json!({"messages":[{"role":"user", "content":"history ".repeat(460_000)}]}),
        )
        .unwrap();
        assert!(body.len() > 3 * 1024 * 1024);
        let payload = json!({"method":"network.open", "body":body});
        let encoded = encode_frame(
            "same-native-component",
            json!({"id":"js:large-history", "payload":payload}),
        )
        .unwrap();
        assert!(encoded.len() > MAX_COMMAND_FRAME_BYTES);
        let accepted = authenticate(&encoded, "same-native-component").unwrap();
        assert_eq!(accepted["payload"], payload);
        assert!(authenticate(&encoded, "other-native-component").is_err());
        let reply = encode_frame(
            "same-native-component",
            json!({"id":"js:database", "result":body}),
        )
        .unwrap();
        assert_eq!(
            authenticate(&reply, "same-native-component").unwrap()["result"],
            body
        );
    }

    #[test]
    fn origin_token_and_envelope_shape_are_checked_before_native_dispatch() {
        let valid = json!({"channel":CHANNEL,"component":"native-token","id":"js:1",
            "payload":{"method":"ownership.acquire","storageId":STORAGE_ID}});
        assert!(authenticate(&valid.to_string(), "native-token").is_ok());
        assert!(authenticate(&valid.to_string(), "retired-token").is_err());
        for addition in ["result", "error", "event"] {
            let mut mixed = valid.clone();
            mixed[addition] = Value::Null;
            assert!(authenticate(&mixed.to_string(), "native-token").is_err());
        }
        assert!(authenticate("[]", "native-token").is_err());
        assert!(authenticate(&" ".repeat(MAX_FRAME_BYTES + 1), "native-token").is_err());
    }

    #[test]
    fn mailbox_creation_failure_and_invalid_session_leave_the_same_bridge_retryable() {
        let root = std::env::temp_dir().join(format!(
            "bk-cat-mailbox-failure-{}",
            random_capability().unwrap()
        ));
        std::fs::create_dir(&root).unwrap();
        let dirs = AppDirs::under(&root);
        let bridge = Arc::new(
            AgentBridge::acquire(
                &dirs,
                concat_api::OpenProjects::default(),
                concat_host::export::Exporter::new(),
            )
            .unwrap(),
        );
        let generation = bridge.status().unwrap().generation;
        let factory_called = AtomicBool::new(false);
        let error = EmbeddedRuntime::start_with_spawner(
            Arc::clone(&bridge),
            random_capability().unwrap(),
            Arc::new(RuntimeResources::for_failed_factory()),
            |_, _| {
                factory_called.store(true, Ordering::Release);
                Err("platform must not start without a mailbox".into())
            },
            |_job| {
                Err(std::io::Error::new(
                    std::io::ErrorKind::WouldBlock,
                    "injected mailbox thread creation failure",
                ))
            },
            LOAD_TIMEOUT,
        )
        .err()
        .expect("thread creation must fail before native component declaration");
        assert!(error.contains("injected mailbox thread creation failure"));
        assert!(!factory_called.load(Ordering::Acquire));
        assert_eq!(
            bridge.status().unwrap().phase,
            concat_agent::Phase::RecoveryRequired
        );
        assert_eq!(bridge.status().unwrap().generation, generation);
        let storage = dirs.data.join("bk-cat").join("agent-session");
        for file in [
            concat_agent::sqlite::DATABASE_FILE_NAME,
            concat_agent::operations::OPERATIONS_FILE_NAME,
        ] {
            assert!(!storage.join(file).exists(), "startup did not open storage");
        }

        // NativeSession rejects this input before claiming the component. The
        // already-created real worker must exit when its one-shot sender drops.
        let (exited, worker_exited) = mpsc::channel();
        let error = EmbeddedRuntime::start_with_spawner(
            Arc::clone(&bridge),
            String::new(),
            Arc::new(RuntimeResources::for_failed_factory()),
            |_, _| {
                factory_called.store(true, Ordering::Release);
                Err("platform must not start for an invalid native session".into())
            },
            |job| {
                spawn_mailbox(Box::new(move || {
                    job();
                    let _ = exited.send(());
                }))
            },
            LOAD_TIMEOUT,
        )
        .err()
        .expect("invalid component must fail without retaining its claim");
        assert!(error.contains("native component identity is invalid"));
        worker_exited
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("constructor failure must retire the waiting worker");
        assert!(!factory_called.load(Ordering::Acquire));
        assert_eq!(
            bridge.status().unwrap().phase,
            concat_agent::Phase::RecoveryRequired
        );

        // Retry the exact same bridge, not a replacement generation. Reaching
        // the factory proves neither preceding failure retained a native claim.
        let runtime = EmbeddedRuntime::start_with(
            Arc::clone(&bridge),
            random_capability().unwrap(),
            Arc::new(RuntimeResources::for_failed_factory()),
            |_, _| {
                factory_called.store(true, Ordering::Release);
                Err("injected platform failure after mailbox retry".into())
            },
        )
        .expect("same bridge remains retryable");
        assert!(factory_called.load(Ordering::Acquire));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !runtime.view().closed && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(runtime.view().closed);
        assert!(
            runtime
                .view()
                .error
                .contains("injected platform failure after mailbox retry")
        );
        assert_eq!(bridge.status().unwrap().generation, generation);
        assert_eq!(
            bridge.status().unwrap().phase,
            concat_agent::Phase::Released
        );
        drop(runtime);
        drop(bridge);
        let replacement = AgentBridge::acquire(
            &dirs,
            concat_api::OpenProjects::default(),
            concat_host::export::Exporter::new(),
        )
        .expect("normal factory failure cleanup releases the real storage lock");
        assert!(replacement.status().unwrap().generation > generation);
        drop(replacement);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn failed_platform_creation_releases_ownership_on_worker_and_allows_reconnect() {
        let root = std::env::temp_dir().join(format!(
            "bk-cat-platform-failure-{}",
            random_capability().unwrap()
        ));
        std::fs::create_dir(&root).unwrap();
        let dirs = AppDirs::under(&root);
        for _ in 0..2 {
            let bridge = Arc::new(
                AgentBridge::acquire(
                    &dirs,
                    concat_api::OpenProjects::default(),
                    concat_host::export::Exporter::new(),
                )
                .unwrap(),
            );
            let runtime = EmbeddedRuntime::start_with(
                Arc::clone(&bridge),
                random_capability().unwrap(),
                Arc::new(RuntimeResources::for_failed_factory()),
                |_, _| Err("injected platform creation failure".into()),
            )
            .unwrap();
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while !runtime.view().closed && std::time::Instant::now() < deadline {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            assert!(runtime.view().closed);
            assert_eq!(
                bridge.status().unwrap().phase,
                concat_agent::Phase::Released
            );
            assert!(
                runtime
                    .view()
                    .error
                    .contains("injected platform creation failure")
            );
        }
        std::fs::remove_dir_all(root).unwrap();
    }
}
