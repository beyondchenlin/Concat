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

use crate::agent::AgentBridge;
use crate::agent::native::{NativeSession, STORAGE_ID};
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
}

#[derive(Clone, Copy)]
enum CommandKind {
    Start,
    Inspect,
    Confirm,
}
enum Work {
    Frame(Value),
    SavedScan,
    Closed,
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
    revoked: AtomicBool,
    platform_closed: AtomicBool,
    scan_queued: AtomicBool,
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
                    let result = runtime.native.shutdown();
                    if result.is_err() {
                        log::error!("bk-cat-agent: state=failed");
                    }
                    log::info!("bk-cat-agent: state=closed");
                    if let Ok(mut view) = runtime.view.lock() {
                        view.closed = true;
                        view.phase = "closed".into();
                        if let Err(error) = result {
                            view.error = error;
                        }
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
                        awaiting_load = false;
                        runtime.fail(LOAD_TIMEOUT_MESSAGE);
                        continue;
                    }
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                };
                if awaiting_load && Instant::now() >= loading_deadline {
                    awaiting_load = false;
                    runtime.fail(LOAD_TIMEOUT_MESSAGE);
                    continue;
                }
                match work {
                    Work::Frame(envelope) => runtime.handle(envelope),
                    Work::SavedScan => {
                        runtime.scan_queued.store(false, Ordering::Release);
                        match runtime.native.take_saved_notifications() {
                            Ok(notifications) => {
                                for mut notification in notifications {
                                    notification["event"] = json!("operation.saved");
                                    if let Err(error) = runtime.post(notification) {
                                        runtime.fail(&error);
                                        break;
                                    }
                                }
                            }
                            Err(error) => runtime.fail(&error),
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
            revoked: AtomicBool::new(false),
            platform_closed: AtomicBool::new(false),
            scan_queued: AtomicBool::new(false),
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

    fn receive(&self, raw: String) {
        if self.revoked.load(Ordering::Acquire) {
            return;
        }
        let envelope = match authenticate(&raw, &self.component) {
            Ok(envelope) => envelope,
            Err(_) => {
                self.fail("拒绝了来源或格式无效的运行组件消息。");
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
        if self.incoming.try_send(Work::Frame(envelope)).is_err() {
            self.fail("聊天消息队列超过上限，已停止执行。");
        }
    }

    fn handle(&self, envelope: Value) {
        if let Some(event) = envelope.get("event").and_then(Value::as_str) {
            match event {
                "runtime.loadFailed" => {
                    let message = envelope["code"]
                        .as_str()
                        .and_then(load_failure_message)
                        .unwrap_or("运行组件报告了未审核的启动错误，智能体已停止。");
                    self.fail(message);
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
                        self.fail("运行组件重复启动。");
                        return;
                    }
                    if let Err(error) = self.command(
                        json!({"method":"runtime.start","storageId":STORAGE_ID}),
                        CommandKind::Start,
                    ) {
                        self.fail(&error);
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
                // Conversation events are only rendered after an explicit
                // native conversation selection is wired; no synthetic entries.
                "conversation.changed" => {}
                "operation.failed" => {
                    self.fail(
                        envelope["message"]
                            .as_str()
                            .unwrap_or("保存通知未得到确认，需要重新核对。"),
                    );
                    return;
                }
                _ => {
                    self.fail("运行组件发送了未审核的事件。");
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
            self.fail("运行组件消息缺少有效编号。");
            return;
        };
        if envelope.get("payload").is_some() {
            let fresh = self
                .seen
                .lock()
                .is_ok_and(|mut seen| seen.len() < 65536 && seen.insert(id.to_owned()));
            let result = if fresh {
                self.native.handle(envelope["payload"].clone())
            } else {
                Err("native request was replayed or exceeded its admission limit".into())
            };
            // A tool-level error still travels in a successful protocol
            // response. An uncertain native edit revokes the owner inside
            // that response, so the page must lose its entire component now.
            match self.bridge.status() {
                Ok(status)
                    if matches!(
                        status.phase,
                        concat_agent::Phase::Draining | concat_agent::Phase::Released
                    ) =>
                {
                    self.fail("原生执行资格已撤销，请重新核对当前工程和会话。");
                    return;
                }
                Err(error) => {
                    self.fail(&error);
                    return;
                }
                _ => {}
            }
            let reply = match result {
                Ok(result) => json!({"id":id,"result":result}),
                Err(message) => json!({"id":id,"error":{"code":"refused","message":message}}),
            };
            if !self.revoked.load(Ordering::Acquire)
                && let Err(error) = self.post(reply)
            {
                self.fail(&error);
            }
            return;
        }
        let kind = self
            .pending
            .lock()
            .ok()
            .and_then(|mut pending| pending.remove(id));
        let Some(kind) = kind else {
            self.fail("运行组件返回了未请求的结果。");
            return;
        };
        if envelope.get("error").is_some() {
            let message = envelope
                .pointer("/error/message")
                .and_then(Value::as_str)
                .unwrap_or("聊天运行组件拒绝了操作。");
            if let Ok(mut view) = self.view.lock() {
                view.error = message.to_owned();
            }
            self.publish();
            return;
        }
        let Some(result) = envelope.get("result") else {
            self.fail("运行组件未返回明确结果。");
            return;
        };
        if matches!(kind, CommandKind::Start | CommandKind::Inspect)
            && let Ok(mut view) = self.view.lock()
        {
            view.snapshot = Some(result.clone());
            view.error.clear();
        }
        if matches!(kind, CommandKind::Confirm)
            && let Ok(mut view) = self.view.lock()
        {
            view.error.clear();
        }
        self.publish();
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

    /// Poll only native completion receipts; this never dispatches a Pi task.
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
        let view = self.view();
        if view.phase != "recovery" {
            return Err("当前会话不在恢复确认阶段。".into());
        }
        let snapshot = view.snapshot.ok_or("请先核对当前工程和会话。")?;
        let id = snapshot["snapshotId"]
            .as_str()
            .ok_or("恢复核对编号缺失。")?;
        let approval = self.native.approve_from_ui(id, "continue")?;
        self.command(
            json!({"method":"runtime.confirmRecovery","approval":approval}),
            CommandKind::Confirm,
        )
    }

    fn revoke(&self, reason: &str) {
        // Native execution loses authority synchronously, before any page or
        // database cleanup. A late JavaScript callback cannot keep it alive.
        self.bridge.stop();
        let first = !self.revoked.swap(true, Ordering::AcqRel);
        log::info!("bk-cat-agent: state=stopping");
        if let Ok(mut view) = self.view.lock() {
            view.phase = "stopping".into();
            // The platform's generic closed acknowledgement must not overwrite
            // the actionable startup error that caused native retirement.
            if !reason.is_empty() && (first || view.error.is_empty()) {
                view.error = reason.to_owned();
            }
        }
        self.publish();
    }

    pub(crate) fn stop(&self) {
        self.revoke("");
        if let Ok(platform) = self.platform.lock()
            && let Some(platform) = platform.as_ref()
        {
            platform.close();
        }
    }

    fn fail(&self, message: &str) {
        log::error!("bk-cat-agent: state=failed");
        self.revoke(message);
        if let Ok(platform) = self.platform.lock()
            && let Some(platform) = platform.as_ref()
        {
            platform.close();
        }
    }

    fn publish(&self) {
        let component = self.component.clone();
        let view = self.view();
        let _ = slint::invoke_from_event_loop(move || {
            Shell::with(|shell, app| {
                let current = shell
                    .embedded
                    .borrow()
                    .as_ref()
                    .is_some_and(|runtime| runtime.component() == component);
                if !current {
                    return;
                }
                let owner = shell
                    .agent
                    .borrow()
                    .as_ref()
                    .ok_or_else(|| "会话暂不可用，仍可使用手动编辑。".to_owned())
                    .and_then(|bridge| bridge.status());
                let mut pane = shell.agent_pane.borrow_mut();
                pane.apply_runtime(&view);
                pane.publish(&app, owner);
            });
        });
    }
}

impl Drop for EmbeddedRuntime {
    fn drop(&mut self) {
        self.bridge.stop();
        if let Ok(platform) = self.platform.lock()
            && let Some(platform) = platform.as_ref()
        {
            platform.close();
        }
    }
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
            pane.can_review = false;
            pane.readiness.runtime_ready = false;
            Ok(())
        })();
    });
    result
}

/// No API key, model provider or fee authorization is inferred by startup.
#[cfg(any(
    target_os = "windows",
    target_os = "android",
    feature = "bk-cat-native-probe"
))]
pub(crate) fn start_for_window() {
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
