#![cfg(target_os = "android")]

#[cfg(not(target_arch = "aarch64"))]
compile_error!("This regression probe requires Android AArch64.");
#[cfg(panic = "abort")]
compile_error!("The rejected-type regression requires panic=unwind.");

use std::collections::BTreeSet;
use std::error::Error;
use std::ffi::{CString, c_char, c_int};
use std::hint::black_box;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::time::{SystemTime, UNIX_EPOCH};

use android_activity::AndroidApp;
use jni::errors::Error as JniError;
use jni::objects::{JClass, JClassLoader, JObject, JString};
use jni::refs::LoaderContext;
use jni::{Env, JValue, JavaVM, jni_sig, jni_str};

// Compile the exact production helper, rather than a copy of its implementation.
#[path = "../../../../src/patches/jni/src/ffi_bool.rs"]
mod ffi_bool;

jni::bind_java_type! {
    pub GeneratedFixture => app.concat.jniregression.ProbeFixture,
    methods {
        fn echo(value: jboolean) -> jboolean,
        static fn static_echo {
            name = "staticEcho",
            sig = (value: jboolean) -> jboolean,
        },
    },
    fields {
        instance_value {
            name = "instanceValue",
            sig = jboolean,
            get = instance_value,
        },
        static static_value {
            name = "staticValue",
            sig = jboolean,
            get = static_value,
        },
    },
}

jni::bind_java_type! {
    pub GeneratedChild => "app.concat.jniregression.ProbeFixture$Child",
    type_map = {
        GeneratedFixture => app.concat.jniregression.ProbeFixture,
    },
    is_instance_of = {
        base: GeneratedFixture,
    },
}

// This intentionally invalid declaration must be rejected during API loading.
// Its assertion is caught in Rust; no invalid reference is created or used.
jni::bind_java_type! {
    pub RejectedGeneratedFixture => app.concat.jniregression.ProbeFixture,
    type_map = {
        GeneratedChild => "app.concat.jniregression.ProbeFixture$Child",
    },
    is_instance_of = {
        GeneratedChild,
    },
}

// Both symbols execute the same body. One exposes the complete register for
// fixture verification; the other follows JNI's actual unsigned-byte ABI.
#[cfg(target_arch = "aarch64")]
core::arch::global_asm!(
    ".text",
    ".p2align 2",
    ".global bkcat_probe_dirty_byte",
    ".global bkcat_probe_dirty_word",
    ".hidden bkcat_probe_dirty_byte",
    ".hidden bkcat_probe_dirty_word",
    ".type bkcat_probe_dirty_byte, %function",
    ".type bkcat_probe_dirty_word, %function",
    "bkcat_probe_dirty_byte:",
    "bkcat_probe_dirty_word:",
    "and x0, x0, #0xff",
    "mov x9, #0x3500",
    "movk x9, #0x7866, lsl #16",
    "movk x9, #0x7b89, lsl #32",
    "orr x0, x0, x9",
    "ret",
    ".size bkcat_probe_dirty_byte, .-bkcat_probe_dirty_byte",
    ".size bkcat_probe_dirty_word, .-bkcat_probe_dirty_word",
);

unsafe extern "system" {
    fn bkcat_probe_dirty_byte(byte: usize) -> u8;
    fn bkcat_probe_dirty_word(byte: usize) -> usize;
}

type ProbeResult<T> = Result<T, Box<dyn Error>>;
const DEX: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/classes.dex"));
const EXPECTED_EXCEPTION: &str = "BKCAT_JNI_PROBE_EXPECTED";
const EXPECTED_CASES: usize = 41;

#[link(name = "log")]
unsafe extern "C" {
    fn __android_log_write(priority: c_int, tag: *const c_char, text: *const c_char) -> c_int;
}

fn write_log(run: &str, message: &str) {
    let line = format!("JNI_PROBE run={run} {message}").replace('\0', "?");
    let line = CString::new(line).expect("NUL bytes were removed");
    // Android's log buffer is the only output. This is a separate test-app tag.
    unsafe { __android_log_write(4, c"BKCAT-JNI-PROBE".as_ptr(), line.as_ptr()) };
}

struct Reporter {
    run: String,
    passed: BTreeSet<String>,
}

impl Reporter {
    fn pass(&mut self, name: &str) -> ProbeResult<()> {
        if !self.passed.insert(name.to_owned()) {
            return Err(format!("duplicate case: {name}").into());
        }
        write_log(&self.run, &format!("PASS test={name}"));
        Ok(())
    }

    fn boolean(&mut self, name: &str, actual: bool, expected: bool) -> ProbeResult<()> {
        if actual != expected {
            return Err(format!("{name}: expected {expected}, got {actual}").into());
        }
        self.pass(name)
    }
}

fn load_class<'local>(
    env: &mut Env<'local>,
    loader: &JObject<'_>,
    name: &str,
) -> ProbeResult<JClass<'local>> {
    let name = env.new_string(name)?;
    let class = env
        .call_method(
            loader,
            jni_str!("loadClass"),
            jni_sig!("(Ljava/lang/String;)Ljava/lang/Class;"),
            &[(&name).into()],
        )?
        .l()?;
    Ok(JClass::cast_local(env, class)?)
}

fn check_dirty_register(report: &mut Reporter) -> ProbeResult<()> {
    let byte_function = black_box(
        bkcat_probe_dirty_byte as unsafe extern "system" fn(usize) -> u8,
    );
    // Model the jni-sys table type without ever calling it as returning bool.
    let legacy_slot: unsafe extern "system" fn(usize) -> bool =
        unsafe { core::mem::transmute(byte_function) };
    for byte in [0usize, 1, 2, 255] {
        let word = unsafe { bkcat_probe_dirty_word(black_box(byte)) };
        if word != (0x7b89_7866_3500 | byte) {
            return Err(format!("wrong dirty-register fixture: byte={byte} word={word:#018x}").into());
        }
    }
    report.pass("DirtyRegister.word")?;
    for byte in [0usize, 1, 2, 255] {
        let actual = unsafe { ffi_bool::call1(legacy_slot, black_box(byte)) };
        report.boolean(&format!("DirtyRegister.byte_{byte}"), actual, byte != 0)?;
    }
    Ok(())
}

fn check_generated_bindings(
    env: &mut Env<'_>,
    loader: &JClassLoader<'_>,
    fixture: &JObject<'_>,
    fixture_class: &JClass<'_>,
    child: &JObject<'_>,
    report: &mut Reporter,
) -> ProbeResult<()> {
    // Explicitly use the real in-memory loader; the thread's default loader
    // does not contain the embedded fixture classes. Later calls use this cache.
    let context = LoaderContext::Loader(loader);
    GeneratedFixtureAPI::get(env, &context)?;
    GeneratedChildAPI::get(env, &context)?;
    let generated_child = env.new_cast_local_ref::<GeneratedChild>(child)?;
    let generated_base = generated_child.as_base();
    report.boolean(
        "Generated.IsAssignableFrom.true",
        env.is_same_object(child, &*generated_base)?,
        true,
    )?;
    let rejected = catch_unwind(AssertUnwindSafe(|| {
        RejectedGeneratedFixtureAPI::get(env, &context)
    }));
    match rejected {
        Err(payload) => {
            let message = payload.downcast_ref::<String>().map(String::as_str)
                .or_else(|| payload.downcast_ref::<&str>().copied())
                .unwrap_or("");
            if !message.contains("is not a subtype of")
                || !message.contains("RejectedGeneratedFixture")
            {
                return Err(format!("unexpected generated-binding panic: {message}").into());
            }
        }
        Ok(Err(error)) => return Err(format!("unexpected generated-binding error: {error}").into()),
        Ok(Ok(_)) => return Err("generated binding accepted the invalid parent-to-child declaration".into()),
    }
    if env.exception_check() {
        return Err("generated type rejection left a pending Java exception".into());
    }
    report.pass("Generated.IsAssignableFrom.false_rejected")?;

    let generated = env.new_cast_local_ref::<GeneratedFixture>(fixture)?;
    for expected in [false, true] {
        let suffix = if expected { "true" } else { "false" };
        report.boolean(
            &format!("Generated.CallBooleanMethodA.{suffix}"),
            generated.echo(env, expected)?,
            expected,
        )?;
        report.boolean(
            &format!("Generated.CallStaticBooleanMethodA.{suffix}"),
            GeneratedFixture::static_echo(env, expected)?,
            expected,
        )?;
        env.set_field(fixture, jni_str!("instanceValue"), jni_sig!("Z"), JValue::Bool(expected))?;
        report.boolean(
            &format!("Generated.GetBooleanField.{suffix}"),
            generated.instance_value(env)?,
            expected,
        )?;
        env.set_static_field(fixture_class, jni_str!("staticValue"), jni_sig!("Z"), JValue::Bool(expected))?;
        report.boolean(
            &format!("Generated.GetStaticBooleanField.{suffix}"),
            GeneratedFixture::static_value(env)?,
            expected,
        )?;
    }
    Ok(())
}

fn run_suite(env: &mut Env<'_>, app: &AndroidApp, report: &mut Reporter) -> ProbeResult<()> {
    check_dirty_register(report)?;
    report.boolean("ExceptionCheck.false_initial", env.exception_check(), false)?;

    // Borrow the Activity reference; AndroidApp owns it throughout this scope.
    let activity = unsafe { JObject::from_raw(env, app.activity_as_ptr().cast()) };
    let parent = env
        .call_method(
            &activity,
            jni_str!("getClassLoader"),
            jni_sig!("()Ljava/lang/ClassLoader;"),
            &[],
        )?
        .l()?;
    // The embedded bytecode has static lifetime; this class loader only reads it.
    let buffer = unsafe { env.new_direct_byte_buffer(DEX.as_ptr().cast_mut(), DEX.len()) }?;
    let loader = env.new_object(
        jni_str!("dalvik/system/InMemoryDexClassLoader"),
        jni_sig!("(Ljava/nio/ByteBuffer;Ljava/lang/ClassLoader;)V"),
        &[(&buffer).into(), (&parent).into()],
    )?;
    let fixture_class = load_class(env, &loader, "app.concat.jniregression.ProbeFixture")?;
    let child_class = load_class(env, &loader, "app.concat.jniregression.ProbeFixture$Child")?;
    let fixture = env.new_object(&fixture_class, jni_sig!("()V"), &[])?;
    let other = env.new_object(&fixture_class, jni_sig!("()V"), &[])?;
    let child = env.new_object(&child_class, jni_sig!("()V"), &[])?;
    let string_class = env.find_class(jni_str!("java/lang/String"))?;
    let expected_exception_class = env.find_class(jni_str!("java/lang/IllegalStateException"))?;

    // Invoke real Java methods and read real VM fields through the patched API.
    for expected in [false, true] {
        let suffix = if expected { "true" } else { "false" };
        let args = [JValue::Bool(expected)];
        let actual = env
            .call_method(&fixture, jni_str!("echo"), jni_sig!("(Z)Z"), &args)?
            .z()?;
        report.boolean(&format!("CallBooleanMethodA.{suffix}"), actual, expected)?;

        let overridden = env
            .call_method(&child, jni_str!("echo"), jni_sig!("(Z)Z"), &args)?
            .z()?;
        report.boolean(
            &format!("VirtualDispatch.child_override_{suffix}"),
            overridden,
            !expected,
        )?;
        let nonvirtual = env
            .call_nonvirtual_method(
                &child,
                &fixture_class,
                jni_str!("echo"),
                jni_sig!("(Z)Z"),
                &args,
            )?
            .z()?;
        report.boolean(
            &format!("CallNonvirtualBooleanMethodA.{suffix}"),
            nonvirtual,
            expected,
        )?;

        let actual = env
            .call_static_method(
                &fixture_class,
                jni_str!("staticEcho"),
                jni_sig!("(Z)Z"),
                &args,
            )?
            .z()?;
        report.boolean(&format!("CallStaticBooleanMethodA.{suffix}"), actual, expected)?;

        env.set_field(
            &fixture,
            jni_str!("instanceValue"),
            jni_sig!("Z"),
            JValue::Bool(expected),
        )?;
        let actual = env.get_field(&fixture, jni_str!("instanceValue"), jni_sig!("Z"))?.z()?;
        report.boolean(&format!("GetBooleanField.{suffix}"), actual, expected)?;

        env.set_static_field(
            &fixture_class,
            jni_str!("staticValue"),
            jni_sig!("Z"),
            JValue::Bool(expected),
        )?;
        let actual = env
            .get_static_field(&fixture_class, jni_str!("staticValue"), jni_sig!("Z"))?
            .z()?;
        report.boolean(&format!("GetStaticBooleanField.{suffix}"), actual, expected)?;
    }

    let alias = env.new_local_ref(&fixture)?;
    report.boolean("IsSameObject.true", env.is_same_object(&fixture, &alias)?, true)?;
    report.boolean("IsSameObject.false", env.is_same_object(&fixture, &other)?, false)?;
    report.boolean("IsInstanceOf.true", env.is_instance_of(&child, &fixture_class)?, true)?;
    report.boolean("IsInstanceOf.false", env.is_instance_of(&fixture, &string_class)?, false)?;
    // JNI IsAssignableFrom(sub, sup) tests whether sub can be cast to sup.
    report.boolean(
        "IsAssignableFrom.true",
        env.is_assignable_from(&child_class, &fixture_class)?,
        true,
    )?;
    report.boolean(
        "IsAssignableFrom.false",
        env.is_assignable_from(&fixture_class, &child_class)?,
        false,
    )?;

    let loader = JClassLoader::cast_local(env, loader)?;
    check_generated_bindings(env, &loader, &fixture, &fixture_class, &child, report)?;

    let thrown = env.call_static_method(
        &fixture_class,
        jni_str!("throwExpected"),
        jni_sig!("()V"),
        &[],
    );
    // These are all exception-safe JNI operations. Capture the actual pending
    // Throwable before clearing it; never substitute synthetic exception state.
    let pending = env.exception_check();
    let exception = env.exception_occurred();
    env.exception_clear();
    let cleared = env.exception_check();
    let after_clear = env.exception_occurred();

    report.boolean(
        "ExpectedThrow.returned_JavaException",
        matches!(thrown, Err(JniError::JavaException)),
        true,
    )?;
    report.boolean("ExceptionCheck.true_pending", pending, true)?;
    report.boolean("ExceptionObject.present", exception.is_some(), true)?;
    report.boolean("ExceptionCheck.false_cleared", cleared, false)?;
    report.boolean("ExceptionObject.cleared", after_clear.is_none(), true)?;
    let exception = exception.ok_or("expected a real pending Throwable")?;
    report.boolean(
        "ExceptionObject.expected_type",
        env.is_instance_of(&exception, &expected_exception_class)?,
        true,
    )?;
    let message = env
        .call_method(
            &exception,
            jni_str!("getMessage"),
            jni_sig!("()Ljava/lang/String;"),
            &[],
        )?
        .l()?;
    let message = JString::cast_local(env, message)?.to_string();
    if message != EXPECTED_EXCEPTION {
        return Err(format!("wrong expected-exception message: {message}").into());
    }
    report.pass("ExceptionObject.expected_message")?;
    if report.passed.len() != EXPECTED_CASES {
        return Err(format!("case count: expected {EXPECTED_CASES}, got {}", report.passed.len()).into());
    }
    Ok(())
}

#[unsafe(no_mangle)]
fn android_main(app: AndroidApp) {
    let stamp = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos());
    let run = format!("{}-{stamp}", std::process::id());
    let mut report = Reporter { run: run.clone(), passed: BTreeSet::new() };
    write_log(&run, "BEGIN package=app.concat.jniregression families=9 generated=10 dirty_register=5 cases=41");
    let vm = unsafe { JavaVM::from_raw(app.vm_as_ptr().cast()) };
    let result = catch_unwind(AssertUnwindSafe(|| {
        vm.with_local_frame(64, |env| run_suite(env, &app, &mut report))
    }));
    match result {
        Ok(Ok(())) => {
            write_log(&run, "RESULT PASS families=9 generated=10 dirty_register=5 cases=41");
            // Returning lets android-activity finish this temporary Activity.
        }
        Ok(Err(error)) => {
            write_log(&run, &format!("RESULT FAIL passed={} error={error}", report.passed.len()));
            std::process::exit(1);
        }
        Err(_) => {
            write_log(&run, &format!("RESULT FAIL passed={} error=rust_panic", report.passed.len()));
            std::process::exit(1);
        }
    }
}

