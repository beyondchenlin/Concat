// SPDX-License-Identifier: AGPL-3.0-or-later

//! JNI host for the controlled, script-only Android WebView.
//!
//! Android's UI thread owns every WebView operation. It never synchronously
//! waits for Slint. The native dispatcher retains its ownership until Java
//! acknowledges actual destruction; a failed teardown keeps the old host
//! registered and revoked rather than admitting a replacement.

use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use concat::embedded::{PlatformHost, RuntimeCallbacks, RuntimeResources};
use jni::objects::{JClass, JString};
use jni::{Env, JavaVM, jni_sig, jni_str, sys};
use slint::android::AndroidApp;

struct HostState {
    instance: String,
    app: AndroidApp,
    callbacks: RuntimeCallbacks,
    accepting: AtomicBool,
    stopping_notified: AtomicBool,
    closed: AtomicBool,
}

impl HostState {
    fn stopping(&self, reason: String) {
        self.accepting.store(false, Ordering::Release);
        if !self.stopping_notified.swap(true, Ordering::AcqRel) {
            (self.callbacks.stopping)(reason);
        }
    }

    fn closed(&self, reason: String) {
        self.stopping(reason.clone());
        if self.closed.swap(true, Ordering::AcqRel) {
            return;
        }
        if let Ok(mut hosts) = hosts().lock() {
            hosts.remove(&self.instance);
        }
        (self.callbacks.closed)(reason);
    }
}

// Strong references survive a dropped PlatformHost until Java acknowledges
// destruction. A failed close deliberately retains the revoked native owner.
fn hosts() -> &'static Mutex<HashMap<String, Arc<HostState>>> {
    static HOSTS: OnceLock<Mutex<HashMap<String, Arc<HostState>>>> = OnceLock::new();
    HOSTS.get_or_init(|| Mutex::new(HashMap::new()))
}

struct AndroidHost {
    state: Arc<HostState>,
}

impl AndroidHost {
    fn open(
        app: &AndroidApp,
        resources: Arc<RuntimeResources>,
        callbacks: RuntimeCallbacks,
    ) -> Result<Box<dyn PlatformHost>, String> {
        let asset = |path: &str| -> Result<String, String> {
            let (_, bytes) = resources
                .resource(path)
                .ok_or_else(|| format!("embedded resource is unavailable: {path}"))?;
            String::from_utf8(bytes).map_err(|_| format!("embedded resource is not UTF-8: {path}"))
        };
        let document = asset("/index.html")?;
        let runtime = asset("/runtime.js")?;
        let bootstrap = asset("/bootstrap.js")?;
        let transport = asset("/transport.js")?;
        static NEXT: AtomicU64 = AtomicU64::new(1);
        let sequence = NEXT
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| n.checked_add(1))
            .map_err(|_| "Android runtime identity exhausted".to_owned())?;
        // This is a native routing identity, not the capability. The common
        // dispatcher separately validates its random component capability.
        let instance = format!("android-{}-{sequence}", std::process::id());
        let state = Arc::new(HostState {
            instance: instance.clone(),
            app: app.clone(),
            callbacks,
            accepting: AtomicBool::new(true),
            stopping_notified: AtomicBool::new(false),
            closed: AtomicBool::new(false),
        });
        {
            let mut hosts = hosts()
                .lock()
                .map_err(|_| "Android runtime registry failed".to_owned())?;
            if !hosts.is_empty() {
                return Err(
                    "the preceding Android runtime has not acknowledged destruction".into(),
                );
            }
            hosts.insert(instance.clone(), Arc::clone(&state));
        }

        let created = super::java::with_runtime_class(app, |env, activity, class| {
            let id = env.new_string(&instance)?;
            let document = env.new_string(&document)?;
            let runtime = env.new_string(&runtime)?;
            let bootstrap = env.new_string(&bootstrap)?;
            let transport = env.new_string(&transport)?;
            env.call_static_method(
                class,
                jni_str!("create"),
                jni_sig!("(Landroid/app/Activity;Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;)V"),
                &[
                    activity.into(),
                    (&id).into(),
                    (&document).into(),
                    (&runtime).into(),
                    (&bootstrap).into(),
                    (&transport).into(),
                ],
            )?;
            Ok(())
        });
        if let Err(error) = created {
            state.closed("Android runtime JNI creation failed".into());
            return Err(format!("create Android runtime: {error}"));
        }
        Ok(Box::new(Self { state }))
    }
}

impl PlatformHost for AndroidHost {
    fn post_message(&self, message: &str) -> Result<(), String> {
        if !self.state.accepting.load(Ordering::Acquire) {
            return Err("Android runtime is no longer active".into());
        }
        if message.len() > 4 * 1024 * 1024 {
            return Err("Android runtime message exceeds its bound".into());
        }
        super::java::with_runtime_class(&self.state.app, |env, _, class| {
            let id = env.new_string(&self.state.instance)?;
            let message = env.new_string(message)?;
            env.call_static_method(
                class,
                jni_str!("send"),
                jni_sig!("(Ljava/lang/String;Ljava/lang/String;)V"),
                &[(&id).into(), (&message).into()],
            )?;
            Ok(())
        })
        .map_err(|error| format!("send Android runtime message: {error}"))
    }

    fn close(&self) {
        // Reject any JNI callback before the Android UI thread processes close.
        // Java then emits stopping -> destroys the view -> emits closed.
        self.state.accepting.store(false, Ordering::Release);
        if self.state.closed.load(Ordering::Acquire) {
            return;
        }
        let result = super::java::with_runtime_class(&self.state.app, |env, _, class| {
            let id = env.new_string(&self.state.instance)?;
            env.call_static_method(
                class,
                jni_str!("close"),
                jni_sig!("(Ljava/lang/String;)V"),
                &[(&id).into()],
            )?;
            Ok(())
        });
        if result.is_err() {
            // No closed acknowledgement: the common dispatcher retains the
            // revoked storage owner. Never release on a timeout or JNI error.
            self.state
                .stopping("Android runtime could not acknowledge close".into());
            log::error!("Android runtime teardown failed; ownership retained");
        }
    }
}

impl Drop for AndroidHost {
    fn drop(&mut self) {
        self.close();
    }
}

/// Installs the platform factory before concat::run creates the native shell.
pub fn install(app: &AndroidApp) -> Result<(), String> {
    // Register the observer before any factory request can enter the Android
    // UI queue. It cannot serialize or recreate a runtime component.
    super::java::with_runtime_class(app, |env, activity, class| {
        env.call_static_method(
            class,
            jni_str!("install"),
            jni_sig!("(Landroid/app/Activity;)V"),
            &[activity.into()],
        )?;
        Ok(())
    })
    .map_err(|error| format!("install Android runtime lifecycle observer: {error}"))?;
    let app = app.clone();
    concat::embedded::install_factory(move |resources, callbacks| {
        AndroidHost::open(&app, resources, callbacks)
    })
}

/// Register only the three fixed Java declarations on the bundled class.
pub(super) fn register(env: &mut Env, class: &JClass) -> jni::errors::Result<()> {
    // SAFETY: signatures exactly match BKCatRuntime.java. All callbacks catch
    // unwinding so a Rust panic cannot cross the JVM's native call boundary.
    let methods = unsafe {
        [
            jni::NativeMethod::from_raw_parts(
                jni_str!("runtimeMessage"),
                jni_str!("(Ljava/lang/String;Ljava/lang/String;)V"),
                runtime_message as *mut c_void,
            ),
            jni::NativeMethod::from_raw_parts(
                jni_str!("runtimeStopping"),
                jni_str!("(Ljava/lang/String;Ljava/lang/String;)V"),
                runtime_stopping as *mut c_void,
            ),
            jni::NativeMethod::from_raw_parts(
                jni_str!("runtimeClosed"),
                jni_str!("(Ljava/lang/String;Ljava/lang/String;)V"),
                runtime_closed as *mut c_void,
            ),
        ]
    };
    // SAFETY: each method's declaration and native function agree above.
    unsafe { env.register_native_methods(class, &methods) }
}

fn receive(instance: sys::jstring, body: sys::jstring, action: fn(&HostState, String)) {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let read = JavaVM::singleton().and_then(|vm| {
            vm.attach_current_thread(|env| {
                // SAFETY: both references are live JNI arguments of this frame.
                let instance = unsafe { JString::from_raw(env, instance) };
                let body = unsafe { JString::from_raw(env, body) };
                Ok::<_, jni::errors::Error>((
                    instance.mutf8_chars(env)?.to_string(),
                    body.mutf8_chars(env)?.to_string(),
                ))
            })
        });
        let Ok((instance, body)) = read else {
            log::error!("Android runtime callback had invalid JNI strings");
            return;
        };
        let state = hosts()
            .lock()
            .ok()
            .and_then(|all| all.get(&instance).cloned());
        if let Some(state) = state {
            action(&state, body);
        }
    }));
    if result.is_err() {
        log::error!("Android runtime callback failed; no handoff acknowledged");
    }
}

unsafe extern "system" fn runtime_message(
    _env: *mut sys::JNIEnv,
    _class: sys::jclass,
    instance: sys::jstring,
    message: sys::jstring,
) {
    receive(instance, message, |state, message| {
        if state.accepting.load(Ordering::Acquire) && message.len() <= 4 * 1024 * 1024 {
            (state.callbacks.message)(message);
        }
    });
}

unsafe extern "system" fn runtime_stopping(
    _env: *mut sys::JNIEnv,
    _class: sys::jclass,
    instance: sys::jstring,
    reason: sys::jstring,
) {
    receive(instance, reason, HostState::stopping);
}

unsafe extern "system" fn runtime_closed(
    _env: *mut sys::JNIEnv,
    _class: sys::jclass,
    instance: sys::jstring,
    reason: sys::jstring,
) {
    receive(instance, reason, HostState::closed);
}
