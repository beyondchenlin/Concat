// SPDX-License-Identifier: AGPL-3.0-or-later

//! Product-only JNI credential storage. No method is exposed to the page,
//! model tools or native edit protocol. Returned secret buffers are wiped.

use std::sync::Arc;

use concat_agent::credentials::{
    CredentialError, CredentialScope, CredentialStore, MAX_SECRET_BYTES, Secret,
    install_platform_store, platform_store,
};
use jni::objects::{Global, JByteArray, JClass, JObject};
use jni::{Env, JavaVM, jni_sig, jni_str};
use slint::android::AndroidApp;
use zeroize::Zeroizing;

/// Application-global references only. Never retain the Activity/AndroidApp:
/// a credential store survives same-process activity reconstruction.
struct AndroidCredentialStore {
    context: Global<JObject<'static>>,
    class: Global<JClass<'static>>,
}

impl AndroidCredentialStore {
    fn call<T>(
        &self,
        action: impl FnOnce(
            &mut Env<'_>,
            &JObject<'_>,
            &Global<JClass<'static>>,
        ) -> jni::errors::Result<T>,
    ) -> Result<T, CredentialError> {
        JavaVM::singleton()
            .and_then(|vm| {
                vm.attach_current_thread(|env| {
                    let result = action(env, self.context.as_ref(), &self.class);
                    // Never describe a provider exception or leave it pending
                    // for a later call or JVM thread detach.
                    if env.exception_check() {
                        env.exception_clear();
                    }
                    result
                })
            })
            .map_err(|_| CredentialError::Unavailable)
    }
}

fn status(code: i32) -> Result<(), CredentialError> {
    match code {
        0 => Ok(()),
        3 => Err(CredentialError::Corrupt),
        4 => Err(CredentialError::InvalidSecret),
        _ => Err(CredentialError::Unavailable),
    }
}

impl CredentialStore for AndroidCredentialStore {
    fn read(&self, scope: &CredentialScope) -> Result<Option<Secret>, CredentialError> {
        let bytes = self.call(|env, context, class| {
            let key = env.new_string(scope.key_id())?;
            let answer = env
                .call_static_method(
                    class,
                    jni_str!("read"),
                    jni_sig!("(Landroid/content/Context;Ljava/lang/String;)[B"),
                    &[context.into(), (&key).into()],
                )?
                .l()?;
            let array = env.cast_local::<JByteArray>(answer)?;
            let length = array.len(env)?;
            if length == 0 || length > MAX_SECRET_BYTES + 1 {
                return Err(jni::errors::Error::NullPtr("invalid credential response"));
            }
            let copied = env.convert_byte_array(&array).map(Zeroizing::new);
            if env.exception_check() {
                env.exception_clear();
            }
            array.set_region(env, 0, &vec![0; length])?;
            copied
        })?;
        match bytes.first().copied() {
            Some(1) if bytes.len() == 1 => Ok(None),
            Some(0) => {
                let value =
                    std::str::from_utf8(&bytes[1..]).map_err(|_| CredentialError::Corrupt)?;
                Secret::new(value.to_owned())
                    .map(Some)
                    .map_err(|_| CredentialError::Corrupt)
            }
            Some(3) => Err(CredentialError::Corrupt),
            Some(4) => Err(CredentialError::InvalidScope),
            _ => Err(CredentialError::Unavailable),
        }
    }

    fn write(&self, scope: &CredentialScope, secret: &Secret) -> Result<(), CredentialError> {
        let result = self.call(|env, context, class| {
            let key = env.new_string(scope.key_id())?;
            let bytes = secret.expose_secret().as_bytes();
            let array = env.byte_array_from_slice(bytes)?;
            let written = env
                .call_static_method(
                    class,
                    jni_str!("write"),
                    jni_sig!("(Landroid/content/Context;Ljava/lang/String;[B)I"),
                    &[context.into(), (&key).into(), (&array).into()],
                )
                .and_then(|answer| answer.i());
            if env.exception_check() {
                env.exception_clear();
            }
            // Java clears its input in finally; also cover linkage failures.
            array.set_region(env, 0, &vec![0; bytes.len()])?;
            written
        })?;
        status(result)
    }

    fn delete(&self, scope: &CredentialScope) -> Result<(), CredentialError> {
        let result = self.call(|env, context, class| {
            let key = env.new_string(scope.key_id())?;
            env.call_static_method(
                class,
                jni_str!("delete"),
                jni_sig!("(Landroid/content/Context;Ljava/lang/String;)I"),
                &[context.into(), (&key).into()],
            )?
            .i()
        })?;
        status(result)
    }
}

/// Capture the application context once after JavaVM initialization. Further
/// activity entries keep the same adapter and need no reference to the old one.
pub fn install(app: &AndroidApp) -> Result<(), CredentialError> {
    if platform_store().is_ok() {
        return Ok(());
    }
    let (context, class) = super::java::with_credentials_class(app, |env, activity, class| {
        let result = (|| {
            let context = env
                .call_method(
                    activity,
                    jni_str!("getApplicationContext"),
                    jni_sig!("()Landroid/content/Context;"),
                    &[],
                )?
                .l()?;
            if context.is_null() {
                return Err(jni::errors::Error::NullPtr(
                    "application context unavailable",
                ));
            }
            Ok((env.new_global_ref(context)?, env.new_global_ref(class)?))
        })();
        if env.exception_check() {
            env.exception_clear();
        }
        result
    })
    .map_err(|_| CredentialError::Unavailable)?;
    install_platform_store(Arc::new(AndroidCredentialStore { context, class })).map(|_| ())
}
