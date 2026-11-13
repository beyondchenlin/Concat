// SPDX-License-Identifier: AGPL-3.0-or-later

//! Hidden, process-local WebView2 host. All COM objects, window handles and
//! callbacks stay on one dedicated STA thread; the Slint thread never pumps
//! a second event loop and the hidden window is never shown or activated.

use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU32, Ordering},
        mpsc::{self, Receiver, SyncSender, TryRecvError},
    },
    time::{Duration, Instant},
};

use webview2_com::{
    CoTaskMemPWSTR, CoreWebView2EnvironmentOptions, CreateCoreWebView2ControllerCompletedHandler,
    CreateCoreWebView2EnvironmentCompletedHandler, DownloadStartingEventHandler,
    LaunchingExternalUriSchemeEventHandler, Microsoft::Web::WebView2::Win32::*,
    NavigationCompletedEventHandler, NavigationStartingEventHandler,
    NewWindowRequestedEventHandler, PermissionRequestedEventHandler, ProcessFailedEventHandler,
    WebMessageReceivedEventHandler, WebResourceRequestedEventHandler,
    WindowCloseRequestedEventHandler,
};
use windows::{
    Win32::{
        Foundation::{E_ACCESSDENIED, E_FAIL, E_INVALIDARG, E_POINTER, HWND, LPARAM, RECT, WPARAM},
        System::{
            Com::{COINIT_APARTMENTTHREADED, CoInitializeEx, CoUninitialize},
            Threading::GetCurrentThreadId,
        },
        UI::{
            Shell::SHCreateMemStream,
            WindowsAndMessaging::{
                CreateWindowExW, DestroyWindow, DispatchMessageW, MSG, MWMO_INPUTAVAILABLE,
                MsgWaitForMultipleObjectsEx, PM_REMOVE, PeekMessageW, PostThreadMessageW,
                QS_ALLINPUT, TranslateMessage, WM_APP, WM_QUIT, WS_DISABLED, WS_EX_NOACTIVATE,
                WS_EX_TOOLWINDOW, WS_POPUP,
            },
        },
    },
    core::{BOOL, HSTRING, Interface, PCWSTR, PWSTR, w},
};

use super::RuntimeResources;

const ENTRY: &str = "https://bk-cat.invalid/index.html";
const ORIGIN: &str = "https://bk-cat.invalid";
const MAX_FRAME_BYTES: usize = 4 * 1024 * 1024;
const MAX_PENDING_FRAMES: usize = 128;
const STARTUP_TIMEOUT: Duration = Duration::from_secs(20);
const CSP: &str = "default-src 'none'; script-src 'self'; connect-src 'none'; frame-src 'none'; worker-src 'none'; object-src 'none'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'";

struct HostError(String);
impl From<windows::core::Error> for HostError {
    fn from(error: windows::core::Error) -> Self {
        Self(error.to_string())
    }
}
impl From<String> for HostError {
    fn from(error: String) -> Self {
        Self(error)
    }
}

struct Shared {
    stopping: AtomicBool,
    stopping_notified: AtomicBool,
    thread_id: AtomicU32,
    on_stopping: Arc<dyn Fn(String) + Send + Sync>,
}

impl Shared {
    fn new(on_stopping: Arc<dyn Fn(String) + Send + Sync>) -> Self {
        Self {
            stopping: AtomicBool::new(false),
            stopping_notified: AtomicBool::new(false),
            thread_id: AtomicU32::new(0),
            on_stopping,
        }
    }

    fn notify_stopping(&self, reason: &str) {
        // Close can synchronously reenter COM events. Revoke delivery before
        // calling native code, and native authority before dropping a controller.
        self.stopping.store(true, Ordering::Release);
        if !self.stopping_notified.swap(true, Ordering::AcqRel) {
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                (self.on_stopping)(reason.to_owned())
            }));
        }
    }

    fn wake(&self) {
        let id = self.thread_id.load(Ordering::Acquire);
        if id != 0 {
            // SAFETY: only a wake-up, with no borrowed pointer or ownership
            // transfer. Failure after thread exit is harmless.
            let _ = unsafe { PostThreadMessageW(id, WM_APP, WPARAM(0), LPARAM(0)) };
        }
    }
}

/// A Send + Sync command handle, never a cross-thread COM pointer.
///
/// Starting this handle does not claim the page or model is ready. Initialisation
/// errors arrive through on_closed; the page's checked protocol announces its
/// own readiness. Neither posting nor closing waits for the native UI thread.
pub struct WindowsRuntime {
    shared: Arc<Shared>,
    outgoing: SyncSender<String>,
}

impl WindowsRuntime {
    /// Queue a native response for the original trusted document.
    pub fn post_message(&self, message: &str) -> Result<(), String> {
        if message.len() > MAX_FRAME_BYTES {
            return Err("native browser message exceeds its size limit".to_owned());
        }
        if self.shared.stopping.load(Ordering::Acquire) {
            return Err("the Windows runtime has closed".to_owned());
        }
        self.outgoing
            .try_send(message.to_owned())
            .map_err(|_| "the Windows runtime message queue is full or closed".to_owned())?;
        self.shared.wake();
        Ok(())
    }

    /// Revoke delivery immediately and request asynchronous thread teardown.
    pub fn close(&self) {
        self.shared.stopping.store(true, Ordering::Release);
        self.shared.wake();
    }
}

impl Drop for WindowsRuntime {
    fn drop(&mut self) {
        self.close();
    }
}

/// Start an isolated hidden WebView2 runtime without creating a visible editor.
///
/// on_stopping synchronously revokes native authority before destruction;
/// on_closed runs once after thread-owned resources have actually been released.
/// No host is restarted here and no queued command survives this thread.
pub fn spawn(
    resources: Arc<RuntimeResources>,
    on_message: Arc<dyn Fn(String) + Send + Sync>,
    on_stopping: Arc<dyn Fn(String) + Send + Sync>,
    on_closed: Arc<dyn Fn(String) + Send + Sync>,
) -> Result<WindowsRuntime, String> {
    // Environment overrides can defeat the local-profile/debugging boundary.
    // Reject them without modifying this process's or the user's environment.
    for key in [
        "WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS",
        "WEBVIEW2_BROWSER_EXECUTABLE_FOLDER",
        "WEBVIEW2_USER_DATA_FOLDER",
        "WEBVIEW2_PIPE_FOR_SCRIPT_DEBUGGER",
        "WEBVIEW2_WAIT_FOR_SCRIPT_DEBUGGER",
    ] {
        if std::env::var_os(key).is_some_and(|value| !value.is_empty()) {
            return Err(format!(
                "the controlled runtime refuses environment override {key}"
            ));
        }
    }
    let shared = Arc::new(Shared::new(on_stopping));
    let (outgoing, incoming) = mpsc::sync_channel(MAX_PENDING_FRAMES);
    let thread_state = Arc::clone(&shared);
    std::thread::Builder::new()
        .name("bk-cat-webview2".to_owned())
        .spawn(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                run(resources, on_message, &thread_state, incoming)
            }));
            let reason = match result {
                Ok(Ok(())) => "Windows runtime closed".to_owned(),
                Ok(Err(error)) => error.0,
                Err(_) => "Windows runtime thread failed".to_owned(),
            };
            thread_state.notify_stopping(&reason);
            thread_state.thread_id.store(0, Ordering::Release);
            // Exactly one terminal callback, after all thread-owned objects
            // have been dropped. A callback cannot unwind into a COM boundary.
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| on_closed(reason)));
        })
        .map_err(|error| format!("start the Windows runtime thread: {error}"))?;
    Ok(WindowsRuntime { shared, outgoing })
}

struct Apartment;
impl Apartment {
    fn enter() -> Result<Self, String> {
        // SAFETY: this newly created thread owns its COM apartment until Drop.
        unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok() }
            .map_err(|error| format!("initialise WebView2 apartment: {error}"))?;
        Ok(Self)
    }
}
impl Drop for Apartment {
    fn drop(&mut self) {
        // SAFETY: paired with the successful initialisation on this same thread.
        unsafe { CoUninitialize() };
    }
}

struct HiddenWindow(HWND);
impl HiddenWindow {
    fn create() -> Result<Self, String> {
        // SAFETY: the built-in STATIC class requires no retained application
        // pointer. The window is owned/destroyed on this thread. WS_VISIBLE is
        // deliberately absent; disabled/no-activate keep keyboard input in Slint.
        let hwnd = unsafe {
            CreateWindowExW(
                WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
                w!("STATIC"),
                w!("BK CAT controlled runtime"),
                WS_POPUP | WS_DISABLED,
                0,
                0,
                1,
                1,
                None,
                None,
                None,
                None,
            )
        }
        .map_err(|error| format!("create hidden runtime window: {error}"))?;
        Ok(Self(hwnd))
    }
}
impl Drop for HiddenWindow {
    fn drop(&mut self) {
        // SAFETY: this thread exclusively owns the window.
        let _ = unsafe { DestroyWindow(self.0) };
    }
}

/// Rust runs this destructor before dropping inner, including during unwind.
/// Every controller is wrapped at the creation callback, so even a failed or
/// late creation result revokes before its last COM reference can be released.
struct RevokeBeforeDrop<T> {
    inner: T,
    shared: Arc<Shared>,
}
impl<T> RevokeBeforeDrop<T> {
    fn new(inner: T, shared: Arc<Shared>) -> Self {
        Self { inner, shared }
    }
}
impl<T> Drop for RevokeBeforeDrop<T> {
    fn drop(&mut self) {
        self.shared.notify_stopping("Windows runtime stopping");
    }
}

struct Controller(ICoreWebView2Controller);
impl Drop for Controller {
    fn drop(&mut self) {
        // SAFETY: all COM interfaces are used on their owning STA thread.
        let _ = unsafe { self.0.Close() };
    }
}

fn pump(shared: &Shared) -> Result<(), String> {
    if shared.stopping.load(Ordering::Acquire) {
        return Err("Windows runtime closed".to_owned());
    }
    let mut message = MSG::default();
    // Bound each turn so a noisy document cannot starve close/revocation.
    for _ in 0..128 {
        // SAFETY: this thread owns the message queue and the stack MSG.
        if !unsafe { PeekMessageW(&mut message, None, 0, 0, PM_REMOVE) }.as_bool() {
            break;
        }
        if message.message == WM_QUIT {
            return Err("Windows runtime message loop ended".to_owned());
        }
        if message.message != WM_APP {
            // SAFETY: messages come from this thread's Windows message queue.
            unsafe {
                let _ = TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        }
        if shared.stopping.load(Ordering::Acquire) {
            return Err("Windows runtime closed".to_owned());
        }
    }
    // The bounded wait also handles a close racing thread queue creation.
    // SAFETY: no handle pointers; we only wait for this thread's messages.
    unsafe { MsgWaitForMultipleObjectsEx(None, 25, QS_ALLINPUT, MWMO_INPUTAVAILABLE) };
    Ok(())
}

fn await_created<T>(
    slot: &Rc<RefCell<Option<windows::core::Result<T>>>>,
    shared: &Shared,
) -> Result<T, String> {
    let deadline = Instant::now() + STARTUP_TIMEOUT;
    loop {
        if shared.stopping.load(Ordering::Acquire) {
            return Err("Windows runtime closed during initialisation".to_owned());
        }
        if let Some(result) = slot.borrow_mut().take() {
            return result.map_err(|error| format!("create WebView2: {error}"));
        }
        if Instant::now() >= deadline {
            return Err("WebView2 initialisation timed out".to_owned());
        }
        pump(shared)?;
    }
}

fn read_string(
    read: impl FnOnce(&mut PWSTR) -> windows::core::Result<()>,
) -> windows::core::Result<String> {
    let mut raw = PWSTR::null();
    let result = read(&mut raw);
    let owned = CoTaskMemPWSTR::from(raw);
    result?;
    Ok(owned.to_string())
}

fn allowed_resource<'a>(uri: &'a str, method: &str) -> Option<&'a str> {
    if method != "GET" {
        return None;
    }
    match uri.strip_prefix(ORIGIN)? {
        path @ ("/index.html" | "/runtime.js" | "/bootstrap.js" | "/transport.js") => Some(path),
        _ => None,
    }
}

fn run(
    resources: Arc<RuntimeResources>,
    on_message: Arc<dyn Fn(String) + Send + Sync>,
    shared: &Arc<Shared>,
    incoming: Receiver<String>,
) -> Result<(), HostError> {
    // Covers startup failures before any controller exists. Controller wrappers
    // independently revoke earlier when an actual browser is being destroyed.
    let _stop_on_exit = RevokeBeforeDrop::new((), Arc::clone(shared));
    let _apartment = RevokeBeforeDrop::new(Apartment::enter()?, Arc::clone(shared));
    let window = RevokeBeforeDrop::new(HiddenWindow::create()?, Arc::clone(shared));
    // Creating the window also creates the Windows thread's message queue.
    shared
        .thread_id
        .store(unsafe { GetCurrentThreadId() }, Ordering::Release);
    let profile_path = resources.user_data_dir();
    std::fs::create_dir_all(&profile_path)
        .map_err(|error| format!("create application runtime profile: {error}"))?;
    let profile = HSTRING::from(profile_path.as_os_str());

    let options: ICoreWebView2EnvironmentOptions = CoreWebView2EnvironmentOptions::default().into();
    // SAFETY: newly created options have not been shared with the browser.
    unsafe {
        options.SetAllowSingleSignOnUsingOSPrimaryAccount(false)?;
        options.SetAdditionalBrowserArguments(w!(
            "--disable-background-networking --disable-component-update --disable-sync --no-first-run"
        ))?;
        options
            .cast::<ICoreWebView2EnvironmentOptions2>()?
            .SetExclusiveUserDataFolderAccess(true)?;
        options
            .cast::<ICoreWebView2EnvironmentOptions3>()?
            .SetIsCustomCrashReportingEnabled(true)?;
        options
            .cast::<ICoreWebView2EnvironmentOptions6>()?
            .SetAreBrowserExtensionsEnabled(false)?;
    }
    let environment_slot = Rc::new(RefCell::new(None));
    let target = Rc::clone(&environment_slot);
    let handler = CreateCoreWebView2EnvironmentCompletedHandler::create(Box::new(
        move |status, environment| {
            *target.borrow_mut() =
                Some(status.and_then(|()| environment.ok_or_else(|| E_POINTER.into())));
            Ok(())
        },
    ));
    // SAFETY: options and strings live through this call; callback owns its
    // result slot. WebView2 copies the option data during creation.
    unsafe {
        CreateCoreWebView2EnvironmentWithOptions(PCWSTR::null(), &profile, &options, &handler)
    }
    .map_err(|error| format!("start WebView2 environment: {error}"))?;
    let environment = await_created(&environment_slot, shared)?;

    let controller_slot = Rc::new(RefCell::new(None));
    let target = Rc::clone(&controller_slot);
    let stop = Arc::clone(shared);
    let handler = CreateCoreWebView2ControllerCompletedHandler::create(Box::new(
        move |status, controller| {
            let controller = controller
                .map(|controller| RevokeBeforeDrop::new(Controller(controller), Arc::clone(&stop)));
            if stop.stopping.load(Ordering::Acquire) {
                stop.notify_stopping("Windows runtime closed during initialisation");
                drop(controller);
                *target.borrow_mut() = Some(Err(E_POINTER.into()));
            } else {
                *target.borrow_mut() =
                    Some(status.and_then(|()| controller.ok_or_else(|| E_POINTER.into())));
            }
            Ok(())
        },
    ));
    let environment10 = environment
        .cast::<ICoreWebView2Environment10>()
        .map_err(|error| format!("WebView2 private-profile support is required: {error}"))?;
    // SAFETY: local apartment-owned environment and controller options.
    unsafe {
        let private = environment10.CreateCoreWebView2ControllerOptions()?;
        private.SetIsInPrivateModeEnabled(true)?;
        environment10.CreateCoreWebView2ControllerWithOptions(
            window.inner.0,
            &private,
            &handler,
        )?;
    }
    let controller = await_created(&controller_slot, shared)?;
    // SAFETY: controller remains on its owning thread and is never shown.
    let view = unsafe {
        controller.inner.0.SetIsVisible(false)?;
        controller.inner.0.SetBounds(RECT {
            left: 0,
            top: 0,
            right: 1,
            bottom: 1,
        })?;
        controller.inner.0.CoreWebView2()?
    };
    let failed = Rc::new(RefCell::new(None::<String>));
    let ready = Rc::new(Cell::new(false));
    configure(
        &view,
        &environment,
        resources,
        on_message,
        shared,
        &failed,
        &ready,
    )
    .map_err(|error| format!("configure controlled WebView2: {error}"))?;
    // SAFETY: every request, navigation and message handler is now installed.
    unsafe { view.Navigate(&HSTRING::from(ENTRY)) }
        .map_err(|error| format!("load the controlled runtime: {error}"))?;
    let navigation_deadline = Instant::now() + STARTUP_TIMEOUT;
    loop {
        if let Some(reason) = failed.borrow_mut().take() {
            return Err(reason.into());
        }
        if shared.stopping.load(Ordering::Acquire) {
            return Ok(());
        }
        if !ready.get() && Instant::now() >= navigation_deadline {
            return Err("controlled runtime document did not finish loading"
                .to_owned()
                .into());
        }
        if ready.get() {
            for _ in 0..32 {
                if shared.stopping.load(Ordering::Acquire) {
                    return Ok(());
                }
                let message = match incoming.try_recv() {
                    Ok(message) => message,
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => return Ok(()),
                };
                // Source is checked again at the final delivery boundary.
                // SAFETY: synchronous calls on the owning apartment.
                unsafe {
                    if read_string(|out| view.Source(out))? != ENTRY {
                        return Err("runtime document changed before native delivery"
                            .to_owned()
                            .into());
                    }
                    view.PostWebMessageAsString(&HSTRING::from(message))?;
                }
            }
        }
        if let Err(error) = pump(shared) {
            return Err(failed.borrow_mut().take().unwrap_or(error).into());
        }
    }
}

type NativeEvent<A> = Box<dyn FnMut(Option<ICoreWebView2>, Option<A>) -> windows::core::Result<()>>;

fn guarded<A: 'static>(
    shared: &Arc<Shared>,
    failed: &Rc<RefCell<Option<String>>>,
    mut action: impl FnMut(Option<ICoreWebView2>, Option<A>) -> windows::core::Result<()> + 'static,
) -> NativeEvent<A> {
    let shared = Arc::clone(shared);
    let failed = Rc::clone(failed);
    Box::new(move |sender, args| {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            action(sender.clone(), args)
        }))
        .unwrap_or_else(|_| Err(E_FAIL.into()));
        if let Err(error) = &result {
            shared.notify_stopping("controlled WebView2 callback failed");
            if failed.borrow().is_none() {
                *failed.borrow_mut() =
                    Some(format!("controlled WebView2 callback failed: {error}"));
            }
            if let Some(sender) = sender {
                // SAFETY: the event and its sender belong to this STA thread.
                // Stop immediately, before returning from a failed deny handler.
                let _ = unsafe { sender.Stop() };
            }
        }
        result
    })
}

fn configure(
    view: &ICoreWebView2,
    environment: &ICoreWebView2Environment,
    resources: Arc<RuntimeResources>,
    on_message: Arc<dyn Fn(String) + Send + Sync>,
    shared: &Arc<Shared>,
    failed: &Rc<RefCell<Option<String>>>,
    ready: &Rc<Cell<bool>>,
) -> windows::core::Result<()> {
    // SAFETY: this entire setup is synchronous on the dedicated apartment.
    // Registration must succeed completely before the first navigation.
    unsafe {
        let settings = view.Settings()?;
        settings.SetAreHostObjectsAllowed(false)?;
        settings.SetIsWebMessageEnabled(true)?;
        settings.SetIsScriptEnabled(true)?;
        settings.SetAreDevToolsEnabled(false)?;
        settings.SetAreDefaultContextMenusEnabled(false)?;
        settings.SetAreDefaultScriptDialogsEnabled(false)?;
        settings.SetIsStatusBarEnabled(false)?;
        settings.SetIsZoomControlEnabled(false)?;
        settings.SetIsBuiltInErrorPageEnabled(false)?;
        settings
            .cast::<ICoreWebView2Settings3>()?
            .SetAreBrowserAcceleratorKeysEnabled(false)?;
        let autofill = settings.cast::<ICoreWebView2Settings4>()?;
        autofill.SetIsPasswordAutosaveEnabled(false)?;
        autofill.SetIsGeneralAutofillEnabled(false)?;
        view.cast::<ICoreWebView2_8>()?.SetIsMuted(true)?;

        let mut token = 0;
        let initial = Rc::new(Cell::new(false));
        let failure = Rc::clone(failed);
        let stop = Arc::clone(shared);
        view.add_NavigationStarting(
            &NavigationStartingEventHandler::create(guarded::<
                ICoreWebView2NavigationStartingEventArgs,
            >(
                shared,
                failed,
                move |_, args| {
                    let args = args.ok_or_else(|| windows::core::Error::from(E_POINTER))?;
                    // Deny first: a failure to read/validate a URI must never leave
                    // the browser's permissive default in force.
                    args.SetCancel(true)?;
                    let uri = read_string(|out| args.Uri(out))?;
                    let allowed = !stop.stopping.load(Ordering::Acquire)
                        && !initial.replace(true)
                        && uri == ENTRY;
                    if !allowed {
                        *failure.borrow_mut() =
                            Some("runtime attempted a forbidden navigation".to_owned());
                        return Err(E_ACCESSDENIED.into());
                    }
                    args.SetCancel(false)?;
                    Ok(())
                },
            )),
            &mut token,
        )?;
        view.add_FrameNavigationStarting(
            &NavigationStartingEventHandler::create(guarded::<
                ICoreWebView2NavigationStartingEventArgs,
            >(
                shared,
                failed,
                move |_, args| {
                    if let Some(args) = args {
                        args.SetCancel(true)?;
                    }
                    Ok(())
                },
            )),
            &mut token,
        )?;
        let loaded = Rc::clone(ready);
        let failure = Rc::clone(failed);
        view.add_NavigationCompleted(
            &NavigationCompletedEventHandler::create(guarded::<
                ICoreWebView2NavigationCompletedEventArgs,
            >(
                shared,
                failed,
                move |sender, args| {
                    let (Some(sender), Some(args)) = (sender, args) else {
                        return Err(E_POINTER.into());
                    };
                    let mut success = BOOL(0);
                    args.IsSuccess(&mut success)?;
                    if success.as_bool() && read_string(|out| sender.Source(out))? == ENTRY {
                        loaded.set(true);
                    } else {
                        *failure.borrow_mut() =
                            Some("controlled runtime navigation failed".to_owned());
                        return Err(E_FAIL.into());
                    }
                    Ok(())
                },
            )),
            &mut token,
        )?;
        view.add_NewWindowRequested(
            &NewWindowRequestedEventHandler::create(guarded::<
                ICoreWebView2NewWindowRequestedEventArgs,
            >(
                shared,
                failed,
                move |_, args| {
                    if let Some(args) = args {
                        args.SetHandled(true)?;
                    }
                    Ok(())
                },
            )),
            &mut token,
        )?;
        view.add_PermissionRequested(
            &PermissionRequestedEventHandler::create(guarded::<
                ICoreWebView2PermissionRequestedEventArgs,
            >(
                shared,
                failed,
                move |_, args| {
                    if let Some(args) = args {
                        args.SetState(COREWEBVIEW2_PERMISSION_STATE_DENY)?;
                    }
                    Ok(())
                },
            )),
            &mut token,
        )?;
        view.cast::<ICoreWebView2_4>()?.add_DownloadStarting(
            &DownloadStartingEventHandler::create(
                guarded::<ICoreWebView2DownloadStartingEventArgs>(
                    shared,
                    failed,
                    move |_, args| {
                        if let Some(args) = args {
                            args.SetCancel(true)?;
                            args.SetHandled(true)?;
                        }
                        Ok(())
                    },
                ),
            ),
            &mut token,
        )?;
        view.cast::<ICoreWebView2_18>()?
            .add_LaunchingExternalUriScheme(
                &LaunchingExternalUriSchemeEventHandler::create(guarded::<
                    ICoreWebView2LaunchingExternalUriSchemeEventArgs,
                >(
                    shared,
                    failed,
                    move |_, args| {
                        if let Some(args) = args {
                            args.SetCancel(true)?;
                        }
                        Ok(())
                    },
                )),
                &mut token,
            )?;
        let failure = Rc::clone(failed);
        view.add_ProcessFailed(
            &ProcessFailedEventHandler::create(guarded::<ICoreWebView2ProcessFailedEventArgs>(
                shared,
                failed,
                move |_, _| {
                    *failure.borrow_mut() =
                        Some("WebView2 process failed; automatic restart is disabled".to_owned());
                    Err(E_FAIL.into())
                },
            )),
            &mut token,
        )?;
        let failure = Rc::clone(failed);
        view.add_WindowCloseRequested(
            &WindowCloseRequestedEventHandler::create(guarded::<windows::core::IUnknown>(
                shared,
                failed,
                move |_, _| {
                    *failure.borrow_mut() = Some("controlled runtime requested closure".to_owned());
                    Err(E_FAIL.into())
                },
            )),
            &mut token,
        )?;

        let environment = environment.clone();
        view.add_WebResourceRequested(
            &WebResourceRequestedEventHandler::create(guarded::<ICoreWebView2WebResourceRequestedEventArgs>(shared, failed, move |_, args| {
                let args = args.ok_or_else(|| windows::core::Error::from(E_POINTER))?;
                // Set a denied response before reading any page-controlled URI.
                let denied = environment.CreateWebResourceResponse(
                    None, 403, w!("Forbidden"), w!("Cache-Control: no-store\r\nContent-Length: 0")
                )?;
                args.SetResponse(&denied)?;
                let mut source = COREWEBVIEW2_WEB_RESOURCE_REQUEST_SOURCE_KINDS_ALL;
                args.cast::<ICoreWebView2WebResourceRequestedEventArgs2>()?
                    .RequestedSourceKind(&mut source)?;
                if source != COREWEBVIEW2_WEB_RESOURCE_REQUEST_SOURCE_KINDS_DOCUMENT {
                    return Ok(());
                }
                let request = args.Request()?;
                let uri = read_string(|out| request.Uri(out))?;
                let method = read_string(|out| request.Method(out))?;
                let Some(path) = allowed_resource(&uri, &method) else { return Ok(()); };
                let mut context = COREWEBVIEW2_WEB_RESOURCE_CONTEXT_ALL;
                args.ResourceContext(&mut context)?;
                let expected = if path == "/index.html" {
                    COREWEBVIEW2_WEB_RESOURCE_CONTEXT_DOCUMENT
                } else { COREWEBVIEW2_WEB_RESOURCE_CONTEXT_SCRIPT };
                if context != expected { return Ok(()); }
                let Some((mime, bytes)) = resources.resource(path) else { return Ok(()); };
                if bytes.len() > 16 * 1024 * 1024 { return Ok(()); }
                let body = SHCreateMemStream(Some(&bytes)).ok_or_else(|| windows::core::Error::from(E_POINTER))?;
                let headers = HSTRING::from(format!(
                    "Content-Type: {mime}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\nReferrer-Policy: no-referrer\r\nContent-Security-Policy: {CSP}",
                    bytes.len()
                ));
                let response = environment.CreateWebResourceResponse(&body, 200, w!("OK"), &headers)?;
                args.SetResponse(&response)?;
                Ok(())
            })),
            &mut token,
        )?;
        // The newer filter includes every document, dedicated worker, service
        // worker and shared-worker request. An older runtime is refused.
        view.cast::<ICoreWebView2_22>()?
            .AddWebResourceRequestedFilterWithRequestSourceKinds(
                w!("*"),
                COREWEBVIEW2_WEB_RESOURCE_CONTEXT_ALL,
                COREWEBVIEW2_WEB_RESOURCE_REQUEST_SOURCE_KINDS_ALL,
            )?;

        let stop = Arc::clone(shared);
        let failure = Rc::clone(failed);
        view.add_WebMessageReceived(
            &WebMessageReceivedEventHandler::create(guarded::<
                ICoreWebView2WebMessageReceivedEventArgs,
            >(
                shared,
                failed,
                move |_, args| {
                    if stop.stopping.load(Ordering::Acquire) {
                        return Ok(());
                    }
                    let args = args.ok_or_else(|| windows::core::Error::from(E_POINTER))?;
                    if read_string(|out| args.Source(out))? != ENTRY {
                        *failure.borrow_mut() =
                            Some("runtime message source was refused".to_owned());
                        return Err(E_ACCESSDENIED.into());
                    }
                    let message = read_string(|out| args.TryGetWebMessageAsString(out))?;
                    if message.len() > MAX_FRAME_BYTES {
                        *failure.borrow_mut() =
                            Some("runtime message exceeded its size limit".to_owned());
                        return Err(E_INVALIDARG.into());
                    }
                    if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        on_message(message)
                    }))
                    .is_err()
                    {
                        *failure.borrow_mut() =
                            Some("native runtime message handler failed".to_owned());
                        return Err(E_FAIL.into());
                    }
                    Ok(())
                },
            )),
            &mut token,
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_close_order(exit: u8) {
        use std::sync::Mutex;
        struct ObservedControllerDrop {
            shared: Arc<Shared>,
            events: Arc<Mutex<Vec<&'static str>>>,
        }
        impl Drop for ObservedControllerDrop {
            fn drop(&mut self) {
                assert!(self.shared.stopping.load(Ordering::Acquire));
                let mut events = self.events.lock().unwrap();
                assert_eq!(&*events, &["stopping"]);
                events.push("controller destroyed");
            }
        }
        let events = Arc::new(Mutex::new(Vec::new()));
        let stopping_events = Arc::clone(&events);
        let shared = Arc::new(Shared::new(Arc::new(move |_| {
            stopping_events.lock().unwrap().push("stopping");
        })));
        let result =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> Result<(), ()> {
                // This is the same guard that owns the actual COM controller,
                // including controller values still held by a creation callback.
                let _controller = RevokeBeforeDrop::new(
                    ObservedControllerDrop {
                        shared: Arc::clone(&shared),
                        events: Arc::clone(&events),
                    },
                    Arc::clone(&shared),
                );
                match exit {
                    0 => Ok(()),
                    1 => Err(()),
                    _ => panic!("injected runtime unwind"),
                }
            }));
        match exit {
            0 => assert!(matches!(result, Ok(Ok(())))),
            1 => assert!(matches!(result, Ok(Err(())))),
            _ => assert!(result.is_err()),
        }
        // The thread's terminal fallback must not duplicate native revocation.
        shared.notify_stopping("thread finished");
        events.lock().unwrap().push("closed");
        assert_eq!(
            &*events.lock().unwrap(),
            &["stopping", "controller destroyed", "closed"]
        );
    }

    #[test]
    fn normal_close_revokes_before_controller_destruction() {
        assert_close_order(0);
    }

    #[test]
    fn failed_run_revokes_before_controller_destruction() {
        assert_close_order(1);
    }

    #[test]
    fn unwind_revokes_before_controller_destruction() {
        assert_close_order(2);
    }

    #[test]
    fn packaged_origin_is_an_exact_allowlist() {
        for path in [
            "/index.html",
            "/runtime.js",
            "/bootstrap.js",
            "/transport.js",
        ] {
            assert_eq!(
                allowed_resource(&format!("{ORIGIN}{path}"), "GET"),
                Some(path)
            );
        }
        for uri in [
            "http://bk-cat.invalid/index.html",
            "https://bk-cat.invalid.evil/index.html",
            "https://bk-cat.invalid@evil/index.html",
            "https://bk-cat.invalid:443/index.html",
            "https://bk-cat.invalid/index.html?extra=1",
            "https://bk-cat.invalid/runtime.js#fragment",
            "https://bk-cat.invalid/../runtime.js",
            "https://bk-cat.invalid/%72untime.js",
            "file:///C:/private.txt",
            "https://example.com/",
            "about:blank",
        ] {
            assert!(allowed_resource(uri, "GET").is_none(), "{uri}");
        }
        assert!(allowed_resource(ENTRY, "POST").is_none());
    }

    #[test]
    fn command_handle_is_send_sync_and_rejects_after_close() {
        fn check<T: Send + Sync>() {}
        check::<WindowsRuntime>();
        let (outgoing, _incoming) = mpsc::sync_channel(MAX_PENDING_FRAMES);
        let host = WindowsRuntime {
            shared: Arc::new(Shared::new(Arc::new(|_| {}))),
            outgoing,
        };
        host.close();
        assert!(host.post_message("{}").is_err());
    }

    #[test]
    fn failed_event_revokes_before_another_callback_can_run() {
        let shared = Arc::new(Shared::new(Arc::new(|_| {})));
        let failed = Rc::new(RefCell::new(None));
        let mut event =
            guarded::<windows::core::IUnknown>(&shared, &failed, |_, _| Err(E_ACCESSDENIED.into()));
        assert!(event(None, None).is_err());
        assert!(shared.stopping.load(Ordering::Acquire));
        assert!(failed.borrow().is_some());
    }

    #[test]
    fn native_host_window_is_hidden_disabled_and_nonactivating() {
        use windows::Win32::UI::WindowsAndMessaging::{
            GWL_EXSTYLE, GWL_STYLE, GetWindowLongW, IsWindowVisible, WS_VISIBLE,
        };
        let window = HiddenWindow::create().unwrap();
        // SAFETY: the test owns this hidden window on the current thread.
        unsafe {
            assert!(!IsWindowVisible(window.0).as_bool());
            let style = GetWindowLongW(window.0, GWL_STYLE) as u32;
            let extended = GetWindowLongW(window.0, GWL_EXSTYLE) as u32;
            assert_ne!(style & WS_DISABLED.0, 0);
            assert_eq!(style & WS_VISIBLE.0, 0);
            assert_ne!(extended & WS_EX_NOACTIVATE.0, 0);
        }
    }

    #[test]
    fn delivery_queue_is_bounded_and_never_waits() {
        let (outgoing, _incoming) = mpsc::sync_channel(1);
        let host = WindowsRuntime {
            shared: Arc::new(Shared::new(Arc::new(|_| {}))),
            outgoing,
        };
        host.post_message("{}").unwrap();
        assert!(host.post_message("{}").is_err());
        assert!(host.post_message(&"x".repeat(MAX_FRAME_BYTES + 1)).is_err());
    }
}
