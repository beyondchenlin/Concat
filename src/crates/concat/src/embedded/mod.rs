// SPDX-License-Identifier: AGPL-3.0-or-later

//! The native component owner and platform mailbox. This is a transport and
//! lifecycle boundary; the original Pi runtime remains the only scheduler.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, SyncSender};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use concat_api::AppDirs;
use serde_json::{Value, json};
#[cfg(any(target_os = "android", feature = "bk-cat-native-probe"))]
use slint::ComponentHandle;

use crate::agent::AgentBridge;
use crate::agent::native::{NativeSession, STORAGE_ID};
use crate::agent::tasks::{SubmissionIntent, TaskMode};
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
const MAX_FRAME_BYTES: usize = 2 * 1024 * 1024;
// Covers queued platform creation/focus and loading the controlled scripts. This is
// independent of the device smoke's unchanged 60-second production observation.
const LOAD_TIMEOUT: Duration = Duration::from_secs(30);
const LOAD_TIMEOUT_MESSAGE: &str = "聊天运行组件未在限定时间内完成加载。请通过系统提供的更新渠道更新网页组件，重启应用后重试；手动剪辑仍可使用。";

fn load_failure_message(code: &str) -> Option<&'static str> {
    match code {
        "web-platform" => Some(
            "系统网页组件缺少智能体所需的基础运行能力。请更新系统网页组件并重启应用；手动剪辑仍可使用。",
        ),
        "weak-references" => Some(
            "系统网页组件缺少智能体所需的弱引用或内存回收通知能力。请更新系统网页组件并重启应用；手动剪辑仍可使用。",
        ),
        "secure-random" => Some(
            "系统网页组件缺少安全随机标识能力。请更新系统网页组件并重启应用；手动剪辑仍可使用。",
        ),
        "cancellation" => Some(
            "系统网页组件缺少可靠的任务取消能力。请更新系统网页组件并重启应用；手动剪辑仍可使用。",
        ),
        "collection-methods" => Some(
            "系统网页组件缺少智能体所需的数据访问能力。请更新系统网页组件并重启应用；手动剪辑仍可使用。",
        ),
        "promise-resolvers" => Some(
            "系统网页组件缺少智能体所需的异步任务能力。请更新系统网页组件并重启应用；手动剪辑仍可使用。",
        ),
        "structured-clone" => Some(
            "系统网页组件缺少智能体所需的数据复制能力。请更新系统网页组件并重启应用；手动剪辑仍可使用。",
        ),
        "script-resource" => Some(
            "聊天运行资源加载失败，智能体已停止。请重新启动应用；问题持续时请重新安装当前版本，手动剪辑仍可使用。",
        ),
        "script-execution" | "runtime-missing" | "bootstrap-missing" => Some(
            "聊天运行脚本未能启动，智能体已停止。请更新系统网页组件并重启应用；手动剪辑仍可使用。",
        ),
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
// Set only during a real native project switch. No idle reconnect polling.
static REOPEN_AFTER_PROJECT_SWITCH: AtomicBool = AtomicBool::new(false);

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
        Err("当前平台没有安装受控聊天运行组件。".into())
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
            return Err(
                "模型已经结束，但本次没有可核实的原生剪辑修改；不能把文字回复当作剪辑成功。".into(),
            );
        }
    } else if !successful {
        return Err("模型任务未完成；请检查渠道、额度与当前工程后重新发送。".into());
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
    Frame(Value),
    ApprovalFinished {
        id: String,
        result: Result<Value, String>,
    },
    SavedScan,
    Submit,
    Closed,
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
                .ok_or("原版任务编号无效。".to_owned())
        };
        Ok(Self {
            task: value["taskId"]
                .as_str()
                .filter(|id| !id.is_empty() && id.len() <= 256)
                .ok_or("原生任务编号无效。")?
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
    view: Mutex<RuntimeView>,
    pending: Mutex<BTreeMap<String, CommandKind>>,
    seen: Mutex<BTreeSet<String>>,
    next: AtomicU64,
    revoked: Arc<AtomicBool>,
    platform_closed: AtomicBool,
    scan_queued: AtomicBool,
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
                let received = if awaiting_load {
                    receiver
                        .recv_timeout(loading_deadline.saturating_duration_since(Instant::now()))
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
                    if REOPEN_AFTER_PROJECT_SWITCH.swap(false, Ordering::AcqRel) {
                        let _ = slint::invoke_from_event_loop(|| {
                            let mut opened = false;
                            Shell::with(|shell, _| {
                                opened = shell.studio.borrow().session.is_some();
                            });
                            if opened {
                                show_ui_error(connect_from_ui());
                            }
                        });
                    }
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
                        awaiting_load = false;
                        runtime.fail_live_work(LOAD_TIMEOUT_MESSAGE);
                        continue;
                    }
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                };
                if awaiting_load && Instant::now() >= loading_deadline {
                    awaiting_load = false;
                    runtime.fail_live_work(LOAD_TIMEOUT_MESSAGE);
                    continue;
                }
                match work {
                    Work::Frame(envelope) => runtime.handle(envelope),
                    Work::ApprovalFinished { id, result } => {
                        // Reap BEFORE publishing the result. A subsequent tool
                        // cannot race the previous thread's final instructions.
                        match runtime.join_approval_worker() {
                            Ok(()) => runtime.reply_native(&id, result),
                            Err(error) => runtime.fail_terminal(&error),
                        }
                    }
                    Work::Submit => runtime.prepare_submission(),
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
            view: Mutex::new(RuntimeView {
                phase: "loading".into(),
                ..RuntimeView::default()
            }),
            pending: Mutex::new(BTreeMap::new()),
            seen: Mutex::new(BTreeSet::new()),
            next: AtomicU64::new(1),
            revoked: Arc::new(AtomicBool::new(false)),
            platform_closed: AtomicBool::new(false),
            scan_queued: AtomicBool::new(false),
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
                error: "聊天运行状态不可用。".into(),
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
                self.fail_live_work("拒绝了来源或格式无效的运行组件消息。");
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
        if self.incoming.try_send(Work::Frame(envelope)).is_err() {
            self.fail_live_work("聊天消息队列超过上限，已停止执行。");
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
                        .unwrap_or("运行组件报告了未审核的启动错误，智能体已停止。");
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
                        self.fail_live_work("运行组件重复启动。");
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
                        self.fail_live_work("运行组件报告了无效的启动诊断，会话已安全停止。");
                        return;
                    }
                    let stage = stage.expect("checked above");
                    let message = message.expect("checked above");
                    log::error!("bk-cat-agent: startup_failed stage={stage} reason={message}");
                    self.fail_live_work(&format!("智能体启动失败：{message}"));
                    return;
                }
                "conversation.presentation" => {
                    if let Err(error) = self.accept_presentation(&envelope) {
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
                            .unwrap_or("保存通知未得到确认，需要重新核对。"),
                    );
                    return;
                }
                _ => {
                    self.fail_live_work("运行组件发送了未审核的事件。");
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
            self.fail_live_work("运行组件消息缺少有效编号。");
            return;
        };
        if envelope.get("payload").is_some() {
            let fresh = self
                .seen
                .lock()
                .is_ok_and(|mut seen| seen.len() < 65536 && seen.insert(id.to_owned()));
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
                Err("native request was replayed or exceeded its admission limit".into())
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
            self.fail_live_work("运行组件返回了未请求的结果。");
            return;
        };
        before_command_result();
        if envelope.get("error").is_some() {
            let message = envelope
                .pointer("/error/message")
                .and_then(Value::as_str)
                .unwrap_or("聊天运行组件拒绝了操作。");
            if matches!(kind, CommandKind::Submit) {
                self.fail_live_work(
                    "模型任务提交未得到明确确认，已停止调用；输入保留，请重新核对会话。",
                );
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
            self.fail_live_work("运行组件未返回明确结果。");
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
                .map_err(|_| "审批工作线程不可用。")?;
            if worker.is_some() {
                return Err("已有剪辑审批正在等待，拒绝并行申请。".into());
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
                    .map_err(|error| format!("无法启动剪辑审批工作线程：{error}"))?,
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
            self.fail_live_work("审批结果无法送达运行组件，已停止执行。");
        }
    }

    fn join_approval_worker(&self) -> Result<(), String> {
        let worker = self
            .approval_worker
            .lock()
            .map_err(|_| "审批工作线程不可用。")?
            .take();
        if let Some(worker) = worker {
            worker
                .join()
                .map_err(|_| "审批工作线程异常退出，已保留恢复记录。")?;
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
                self.fail_live_work("原生执行资格已撤销，请重新核对当前工程和会话。");
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

    fn post(&self, mut envelope: Value) -> Result<(), String> {
        envelope["channel"] = json!(CHANNEL);
        envelope["component"] = json!(self.component);
        let serialized = serde_json::to_string(&envelope).map_err(|error| error.to_string())?;
        if serialized.len() > MAX_FRAME_BYTES {
            return Err("native response exceeds frame limit".into());
        }
        self.platform
            .lock()
            .map_err(|_| "runtime platform poisoned")?
            .as_ref()
            .ok_or("native platform is not ready")?
            .post_message(&serialized)
    }

    fn queue_submission(&self, intent: SubmissionIntent) -> Result<(), String> {
        self.queue_submission_with(intent, || self.incoming.try_send(Work::Submit))
    }

    fn queue_submission_with(
        &self,
        intent: SubmissionIntent,
        send: impl FnOnce() -> Result<(), mpsc::TrySendError<Work>>,
    ) -> Result<(), String> {
        if self.revoked.load(Ordering::Acquire) || self.view().phase != "ready" {
            return Err("请先连接会话并完成原生恢复核对。".into());
        }
        if let Some(generation) = &intent.generation {
            generation.bind_component_revocation(Arc::clone(&self.revoked))?;
        }
        let title = match intent.mode {
            TaskMode::Chat => "正在对话",
            TaskMode::Script => "正在生成文案候选",
            TaskMode::Edit => "正在剪辑当前工程",
        };
        {
            let mut submission = self.submission.lock().map_err(|_| "本次任务状态不可用。")?;
            if submission.is_some() {
                return Err("已有任务正在执行，请完成或停止后再发送。".into());
            }
            self.native.tool_approval.begin_task()?;
            *submission = Some(intent);
        }
        if let Ok(mut view) = self.view.lock() {
            if self.revoked.load(Ordering::Acquire) {
                return Err("本次任务已停止，输入未发送。".into());
            }
            view.task_running = true;
            view.task_title = title.into();
            view.error.clear();
        }
        if send().is_err() {
            self.submission
                .lock()
                .map_err(|_| "本次任务状态不可用。")?
                .take();
            let error = "聊天任务队列已满，本次未发送。";
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
            .prepare_submission(&intent)
            .and_then(|payload| self.command(payload, CommandKind::Submit));
        if let Err(error) = result {
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

    fn accept_submission(&self, result: &Value) -> Result<(), String> {
        let accepted = AcceptedSubmission::parse(result)?;
        let intent = self
            .submission
            .lock()
            .map_err(|_| "本次任务状态不可用。")?
            .clone()
            .ok_or("原版运行组件返回了无对应任务的提交结果。")?;
        if result["taskId"] != intent.id
            || result["submissionId"]
                .as_u64()
                .is_none_or(|id| id > 9_007_199_254_740_991)
        {
            return Err("原版任务提交编号无效。".into());
        }
        let conversation = result["conversationId"]
            .as_u64()
            .ok_or("原版任务会话编号缺失。")?;
        if !self.native.tasks.may_present(conversation, None) {
            return Err("原版任务会话未得到原生绑定。".into());
        }
        *self
            .observed_conversation
            .lock()
            .map_err(|_| "当前会话状态不可用。")? = Some(conversation);
        let mut receipt = self
            .accepted_submission
            .lock()
            .map_err(|_| "任务接受回执不可用。")?;
        if self.revoked.load(Ordering::Acquire) {
            return Err("本次任务已停止，迟到提交回执未接受。".into());
        }
        if receipt.is_some() {
            return Err("任务重复返回接受回执。".into());
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
            .ok_or("会话显示缺少原版编号。")?;
        let observed = *self
            .observed_conversation
            .lock()
            .map_err(|_| "当前会话状态不可用。")?;
        // A detached observation can have one already queued frame. It is
        // ignored instead of replacing another project's/task's presentation.
        if !self.native.tasks.may_present(conversation, observed) {
            return Ok(());
        }
        let messages = value["messages"]
            .as_array()
            .ok_or("会话显示记录格式无效。")?;
        if messages.len() > 100
            || messages.iter().any(|message| {
                !matches!(
                    message["role"].as_str(),
                    Some("user" | "assistant" | "tool")
                ) || message["text"]
                    .as_str()
                    .is_none_or(|text| text.len() > 128 * 1024)
            })
            || value["partialText"]
                .as_str()
                .is_none_or(|text| text.len() > 256 * 1024)
            || !value["busy"].is_boolean()
        {
            return Err("会话显示内容超过已审核的范围。".into());
        }
        self.view
            .lock()
            .map_err(|_| "会话显示状态不可用。")?
            .presentation = Some(value.clone());
        Ok(())
    }

    fn complete_submission(&self, event: &Value) {
        let result = (|| -> Result<(bool, bool, Option<String>), String> {
            let completed = AcceptedSubmission::parse(event)?;
            if self
                .accepted_submission
                .lock()
                .map_err(|_| "任务接受回执不可用。")?
                .as_ref()
                != Some(&completed)
            {
                return Err("任务终态与原版接受回执不一致，结果未写入。".into());
            }
            let id = event["taskId"].as_str().ok_or("任务完成编号缺失。")?;
            let conversation = event["conversationId"]
                .as_u64()
                .ok_or("任务完成会话编号缺失。")?;
            let successful = match event["status"].as_str() {
                Some("done") => true,
                Some("unanswered") => false,
                _ => return Err("任务没有返回明确终态。".into()),
            };
            let text = event["text"]
                .as_str()
                .filter(|text| text.len() <= 1024 * 1024)
                .ok_or("任务回答无效或超过本次上限。")?;
            let intent = self
                .submission
                .lock()
                .map_err(|_| "本次任务状态不可用。")?
                .clone()
                .filter(|intent| intent.id == id)
                .ok_or("任务完成不属于当前原生请求。")?;
            let native_edit_verified = self.native.tasks.finish(id, conversation)?;
            let edit_without_changes =
                successful && intent.mode == TaskMode::Edit && !native_edit_verified;
            let native_refusal = self.native.tool_approval.last_refusal(conversation);
            self.accepted_submission
                .lock()
                .map_err(|_| "任务接受回执不可用。")?
                .take();
            self.submission
                .lock()
                .map_err(|_| "本次任务状态不可用。")?
                .take();
            if let Some(generation) = intent.generation {
                deliver_generation(
                    generation,
                    if successful {
                        Ok(text.to_owned())
                    } else {
                        Err("文案生成没有成功完成，已保留原手稿；请查看模型错误后重新发送。".into())
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
            .map_err(|_| "本次任务状态不可用。".to_owned())
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
        if self.revoked.load(Ordering::Acquire) {
            return Err("聊天运行组件已停止。".into());
        }
        let sequence = self
            .next
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_add(1))
            .map_err(|_| "native command sequence exhausted")?;
        let id = format!("native:{sequence}");
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
        if let Err(error) = self.post(json!({"id":id,"payload":payload})) {
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
            return Err("当前没有待核对的恢复会话。".into());
        }
        self.command(json!({"method":"runtime.inspect"}), CommandKind::Inspect)
    }

    pub(crate) fn confirm(&self) -> Result<(), String> {
        self.confirm_decision("continue")
    }

    pub(crate) fn discard_interrupted(&self) -> Result<(), String> {
        self.confirm_decision("discard")
    }

    fn confirm_decision(&self, decision: &str) -> Result<(), String> {
        let view = self.view();
        if view.phase != "recovery" {
            return Err("当前会话不在恢复确认阶段。".into());
        }
        let snapshot = view.snapshot.ok_or("请先核对当前工程和会话。")?;
        let id = snapshot["snapshotId"]
            .as_str()
            .ok_or("恢复核对编号缺失。")?;
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
            // Teardown milestones are not failures, even if they are the first
            // callback or arrive after a completed close. A real failure may
            // replace an old placeholder but never the other way around.
            if !platform_reason_is_placeholder(reason)
                && (first || platform_reason_is_placeholder(&view.error))
            {
                view.error = reason.to_owned();
            }
        }
        if let Ok(mut submission) = self.submission.lock()
            && let Some(intent) = submission.take()
            && let Some(generation) = intent.generation
        {
            deliver_generation(generation, Err("文案生成已停止，未覆盖手稿。".into()));
        }
        self.publish();
    }

    fn remember_terminal_failure(&self, reason: &str) {
        if let Ok(mut view) = self.view.lock()
            && terminal_reason_replaces_placeholder(&view.error, reason)
        {
            log::error!("bk-cat-agent: platform_failed reason={reason}");
            view.error = format!("内置运行组件异常：{reason}");
        }
    }

    pub(crate) fn tool_permission_view(
        &self,
    ) -> Result<crate::agent::tool_approval::PermissionView, String> {
        self.native.tool_approval.view()
    }

    pub(crate) fn set_tool_mode_from_ui(&self, index: i32) -> Result<(), String> {
        if self.revoked.load(Ordering::Acquire) || self.view().phase != "ready" {
            return Err("请先连接并核对智能体会话，再修改操作权限。".into());
        }
        self.native.tool_approval.set_mode_from_ui(index)
    }

    pub(crate) fn decide_tool_from_ui(
        &self,
        request_id: &str,
        approve: bool,
    ) -> Result<(), String> {
        if self.revoked.load(Ordering::Acquire) || self.view().phase != "ready" {
            return Err("当前会话未就绪，不能批准剪辑。".into());
        }
        self.native
            .tool_approval
            .decide_from_ui(request_id, approve)
    }

    pub(crate) fn stop(&self) {
        self.revoke("");
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
        let view = self.view();
        let _ = slint::invoke_from_event_loop(move || {
            let mut refresh_permission = false;
            Shell::with(|shell, app| {
                let current = shell
                    .embedded
                    .borrow()
                    .as_ref()
                    .is_some_and(|runtime| runtime.component() == component);
                if !current {
                    return;
                }
                // The worker pushes actual state changes only. There is no
                // 250ms poll during ordinary editing or playback.
                let owner = shell
                    .agent
                    .borrow()
                    .as_ref()
                    .ok_or_else(|| "会话暂不可用，仍可使用手动编辑。".to_owned())
                    .and_then(|bridge| bridge.status());
                let mut pane = shell.agent_pane.borrow_mut();
                let was_ready = pane.readiness.runtime_ready;
                pane.apply_runtime(&view);
                refresh_permission = was_ready != pane.readiness.runtime_ready;
                pane.publish(&app, owner);
            });
            if refresh_permission {
                // The original poll also updated the tool's connected badge.
                // Preserve that update on actual connect/disconnect only.
                crate::agent::window::refresh_tool_permission();
            }
        });
    }
}

fn platform_reason_is_placeholder(reason: &str) -> bool {
    matches!(
        reason,
        "" | "Windows runtime stopping" | "Windows runtime closed"
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
                pane.error = "请先配置本项目渠道和系统凭据，并连接、核对当前会话。".into();
            }
        }
        pane.publish(
            &app,
            shell
                .agent
                .borrow()
                .as_ref()
                .ok_or_else(|| "会话暂不可用。".into())
                .and_then(|bridge| bridge.status()),
        );
    });
}

/// Only a trusted Slint window callback calls these; the web component never
/// receives a port to set modes or resolve approvals.
pub(crate) fn set_tool_mode_from_ui(index: i32) -> Result<(), String> {
    let mut result = Err("聊天运行组件尚未连接。".to_owned());
    Shell::with(|shell, _| {
        if let Some(runtime) = shell.embedded.borrow().as_ref() {
            result = runtime.set_tool_mode_from_ui(index);
        }
    });
    result
}

pub(crate) fn decide_tool_from_ui(request_id: String, approve: bool) -> Result<(), String> {
    let mut result = Err("聊天运行组件尚未连接。".to_owned());
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

fn submit_from_ui(input: String, mode: TaskMode, rewrite: bool) -> Result<(), String> {
    let mut result = Err("原生窗口不可用。".to_owned());
    Shell::with(|shell, app| {
        result = (|| {
            let runtime = shell
                .embedded
                .borrow()
                .clone()
                .ok_or("请先连接聊天会话。")?;
            let bridge = shell.agent.borrow().clone().ok_or("当前会话不可用。")?;
            // Pressing Send is the user's explicit intent for this model
            // request. Do not require a separate fee-consent click for each
            // ordinary prompt. Tool editing remains independently gated.
            {
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
                    return Err("模型调用授权暂不可用，请等待会话就绪后重新发送。".into());
                }
            }
            let text = shell
                .agent_pane
                .borrow()
                .validate_submission(bridge.status(), &input)?;
            let mut studio = shell.studio.borrow_mut();
            let (reference_keys, reference_context) = if mode == TaskMode::Script {
                (Vec::new(), String::new())
            } else {
                shell.agent_pane.borrow().resolve_references(&studio)?
            };
            let ui_draft = input.clone();
            let session = studio.session.as_ref().ok_or("请先打开原生工程。")?;
            let project = crate::live::CurrentProject {
                path: session.path().to_owned(),
                project_id: session.project_identity().to_owned(),
                session_id: session.session_id(),
                revision: session.revision(),
            };
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
                    return Err("请等待文案保存和选版操作完成。".into());
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
            let intent = SubmissionIntent {
                id,
                project,
                mode,
                input: prompt,
                ui_draft,
                reference_keys,
                selected,
                generation,
            };
            if let Err(error) = runtime.queue_submission(intent) {
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
            .ok_or_else(|| "会话暂不可用。".into())
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

/// Opening an editor project is the only automatic session-binding trigger.
/// The existing native owner is never replaced until actual platform close.
pub(crate) fn project_opened() {
    let mut previous = None;
    let mut connect_now = false;
    Shell::with(|shell, _| {
        let path = match shell.studio.borrow().session.as_ref() {
            Some(session) => session.path().to_owned(),
            None => return,
        };
        let runtime = shell.embedded.borrow().clone();
        match runtime {
            None => connect_now = true,
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
                    return;
                }
                if !view.closed {
                    REOPEN_AFTER_PROJECT_SWITCH.store(true, Ordering::Release);
                    previous = Some(runtime);
                } else if shell.agent.borrow().as_ref().is_some_and(|agent| {
                    agent
                        .status()
                        .is_ok_and(|status| status.phase == concat_agent::Phase::Released)
                }) {
                    connect_now = true;
                } else {
                    REOPEN_AFTER_PROJECT_SWITCH.store(true, Ordering::Release);
                }
            }
        }
    });
    if let Some(runtime) = previous {
        runtime.stop();
    }
    if connect_now {
        show_ui_error(connect_from_ui());
    }
}

/// Explicit native UI action. Reopening after background/stop first requires
/// the old platform's destruction and its database/ownership release.
pub(crate) fn connect_from_ui() -> Result<(), String> {
    let mut result = Err("原生窗口不可用。".into());
    Shell::with(|shell, _| {
        result = (|| {
            if let Some(runtime) = shell.embedded.borrow().as_ref()
                && !runtime.view().closed
            {
                return Err("聊天运行组件尚未关闭，请等待停止完成。".into());
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
                    _ => return Err("旧会话尚未排空，不能接管存储。".into()),
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

/// A desktop editor must not start Pi until the user actually starts using
/// it. Manual cutting and playback allocate no agent runtime or model thread.
/// Native probes and Android keep their existing initialization semantics.
#[cfg(all(target_os = "windows", not(feature = "bk-cat-native-probe")))]
pub(crate) fn start_for_window() {}

/// An explicit draft edit or script-workspace visit is user intent to use AI.
/// Do not silently retry after a stop, closed session, or failed connection.
#[cfg(all(target_os = "windows", not(feature = "bk-cat-native-probe")))]
pub(crate) fn activate_from_user_intent() {
    let mut should_connect = false;
    Shell::with(|shell, _| {
        should_connect = shell.studio.borrow().session.is_some()
            && shell.embedded.borrow().is_none()
            && shell.agent_pane.borrow().error.is_empty();
    });
    if should_connect {
        show_ui_error(connect_from_ui());
    }
}

/// No API key, model provider or fee authorization is inferred by startup.
#[cfg(any(target_os = "android", feature = "bk-cat-native-probe"))]
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
                    .ok_or_else(|| "聊天会话暂不可用。".to_owned())
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
                    .ok_or_else(|| "会话暂不可用。".to_owned())
                    .and_then(|bridge| bridge.status()),
            );
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_task_receipt_never_replaces_an_existing_native_or_runtime_error() {
        for (successful, edit_without_changes) in [(true, true), (true, false), (false, false)] {
            let mut view = RuntimeView {
                error: "工程版本已变化，原生操作未执行。".into(),
                task_running: true,
                task_title: "正在剪辑当前工程".into(),
                ..Default::default()
            };
            finish_task_view(
                &mut view,
                successful,
                edit_without_changes,
                Some("用户拒绝了本次剪辑操作，工程未修改。".into()),
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
            "用户拒绝了本次剪辑操作，工程未修改。",
            "等待剪辑批准超时，工程未修改。",
        ] {
            let mut view = RuntimeView {
                task_running: true,
                task_title: "正在剪辑当前工程".into(),
                ..Default::default()
            };
            finish_task_view(&mut view, true, true, Some(reason.into())).unwrap();
            assert!(!view.task_running);
            assert!(view.error.is_empty());
            assert_eq!(view.task_title, reason);

            let error = finish_task_view(&mut view, false, false, Some(reason.into())).unwrap_err();
            assert!(error.contains("模型任务未完成"));
            assert!(
                view.error.is_empty(),
                "classification cannot publish an error before winning retirement"
            );
            assert!(view.task_title.is_empty());
        }
        let mut view = RuntimeView::default();
        let error = finish_task_view(&mut view, true, true, None).unwrap_err();
        assert!(error.contains("没有可核实的原生剪辑修改"));
        assert!(view.error.is_empty());

        let mut view = RuntimeView {
            task_running: true,
            task_title: "正在剪辑当前工程".into(),
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
            task_title: "正在剪辑当前工程".into(),
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
        use slint::ComponentHandle;
        use slint::platform::{PointerEventButton, WindowEvent};
        use slint::private_unstable_api::re_exports::{
            AccessibleRole, AccessibleStringProperty, WindowInner,
        };
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
                && item.accessible_role() == AccessibleRole::Button
                && item
                    .accessible_string_property(AccessibleStringProperty::Label)
                    .is_some_and(|value| value == label)
            {
                // map_to_window maps the parent's coordinate space; include
                // this item's own origin before converting the real center.
                let point = item.map_to_window(item.geometry().center());
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
                app.set_phone(true);
                app.set_on_start(false);
                app.window().set_size(slint::LogicalSize::new(600., 800.));
                app.show().unwrap();
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
                    assert_eq!(agent.get_status().title, "操作未执行");
                    agent.set_composer_mode(1);
                    agent.set_draft(next.clone().into());
                    click_accessible_control(&app, "发送");
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
                app.set_phone(true);
                app.set_on_start(false);
                app.window().set_size(slint::LogicalSize::new(600., 800.));
                app.show().unwrap();
                let connects = std::rc::Rc::new(std::cell::Cell::new(0));
                let reviews = std::rc::Rc::new(std::cell::Cell::new(0));
                let received = std::rc::Rc::clone(&connects);
                app.global::<crate::ui::AgentUi>()
                    .on_connect_requested(move || received.set(received.get() + 1));
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
                        assert!(error.contains("原生会话恢复核验尚未完成"));
                    } else if case == "observe-failed" {
                        assert!(error.contains("会话历史读取失败"));
                    } else if case == "start-failed" {
                        assert!(error.contains("启动配置失败"));
                    } else if case == "queue-failed" {
                        assert!(error.contains("聊天任务队列已满"));
                    } else if case == "unanswered" {
                        assert!(error.contains("模型任务未完成"));
                    } else {
                        assert!(error.contains("没有可核实的原生剪辑修改"));
                    }
                    assert!(
                        fixture
                            .runtime
                            .native
                            .tool_approval
                            .last_refusal(9)
                            .is_none()
                    );

                    (fixture.callbacks.closed)("Windows runtime closed".into());
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
                    assert_eq!(agent.get_status().error, error);
                    agent.set_draft("恢复前不能直接执行".into());
                    click_accessible_control(&app, "发送");
                    click_accessible_control(&app, "连接会话");
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
                    click_accessible_control(&app, "核对工程");
                    assert_eq!(reviews.get(), index + 1);
                    replacement.finish_handoff(Duration::from_secs(1)).unwrap();
                    drop(replacement);
                    fixture.finish();
                }
                app.hide().unwrap();
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
                        .contains("没有可核实的原生剪辑修改")
                );
            }
            let before = fixture.runtime.view().error;
            (fixture.callbacks.closed)("Windows runtime closed".into());
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
        (fixture.callbacks.closed)("Windows runtime closed".into());
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
        (fixture.callbacks.closed)("Windows runtime closed".into());
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
            (fixture.callbacks.closed)("Windows runtime closed".into());
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
                .contains("模型任务提交未得到明确确认")
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
        (fixture.callbacks.closed)("Windows runtime closed".into());
        fixture.wait_for(|view| view.closed);
        assert!(fixture.runtime.view().error.contains("审批工作线程"));
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
                    && frame["result"]["approved"] == false
                    && frame["result"]["message"]
                        .as_str()
                        .is_some_and(|text| text.contains("会话未就绪"))
            })
        });
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
        let fixture = StartupFixture::new(Duration::from_secs(30));
        start_test_approval(&fixture, "approval-close");
        (fixture.callbacks.closed)("Native host closed".into());
        fixture.wait_for(|view| view.closed);
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
        assert!(fixture.posted.lock().unwrap().is_empty());
        fixture.finish();
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
            fixture.wait_for(|view| view.error.contains("接受回执不一致"));
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
    fn startup_failure_codes_publish_chinese_errors_without_untrusted_diagnostics() {
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
            assert!(error.contains("网页组件") || error.contains("未审核"));
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
            "智能体启动失败：原版 Pi 会话存储初始化失败"
        );
        assert_eq!(fixture.closes.load(Ordering::Acquire), 1);
        (fixture.callbacks.closed)("Windows runtime closed".into());
        fixture.wait_for(|view| view.closed);
        assert_eq!(
            fixture.runtime.view().error,
            "智能体启动失败：原版 Pi 会话存储初始化失败"
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
            (fixture.callbacks.closed)("Windows runtime closed".into());
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
        (fixture.callbacks.closed)("Windows runtime closed".into());
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
                    "原生执行资格已撤销，请重新核对当前工程和会话。"
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
                    "审批结果无法送达运行组件，已停止执行。"
                }
            };
            assert_eq!(fixture.runtime.view().error, expected);
            assert!(fixture.runtime.revoked.load(Ordering::Acquire));
            assert_eq!(fixture.closes.load(Ordering::Acquire), 1);
            (fixture.callbacks.closed)("Windows runtime closed".into());
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
            (fixture.callbacks.closed)("Windows runtime closed".into());
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
                    (fixture.callbacks.stopping)("Windows runtime stopping".into());
                    assert!(fixture.runtime.view().error.is_empty());
                    assert!(!fixture.runtime.view().closed);
                }
            }
            (fixture.callbacks.closed)("Windows runtime closed".into());
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
            (fixture.callbacks.stopping)("Windows runtime stopping".into());
            (fixture.callbacks.closed)("Windows runtime closed".into());
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
        (fixture.callbacks.stopping)("Windows runtime stopping".into());
        (fixture.callbacks.closed)("Windows runtime closed".into());
        fixture.wait_for(|view| view.closed);
        assert_eq!(fixture.runtime.view().error, "原生工程状态核对失败。");
        fixture.finish();

        for previous in ["", "Windows runtime stopping", "Windows runtime closed"] {
            let fixture = StartupFixture::new(Duration::from_secs(30));
            fixture.runtime.view.lock().unwrap().error = previous.into();
            (fixture.callbacks.stopping)("Windows runtime stopping".into());
            (fixture.callbacks.closed)("WebView2 process failed".into());
            fixture.wait_for(|view| view.closed);
            let failure = fixture.runtime.view().error;
            assert!(failure.contains("WebView2 process failed"));
            (fixture.callbacks.stopping)("Windows runtime stopping".into());
            (fixture.callbacks.closed)("Windows runtime closed".into());
            assert_eq!(fixture.runtime.view().error, failure);
            assert_eq!(fixture.runtime.view().phase, "closed");
            fixture.finish();
        }
    }

    #[test]
    fn completed_native_cleanup_clears_only_placeholders_and_keeps_shutdown_errors() {
        for previous in [
            "",
            "Windows runtime stopping",
            "Windows runtime closed",
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
            "Windows runtime stopping",
            "WebView2 initialisation timed out"
        ));
        assert!(!terminal_reason_replaces_placeholder(
            "智能体启动失败：原始错误",
            "WebView2 initialisation timed out"
        ));
        assert!(!terminal_reason_replaces_placeholder(
            "Windows runtime stopping",
            "Windows runtime closed"
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
