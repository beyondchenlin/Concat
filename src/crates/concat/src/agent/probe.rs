// SPDX-License-Identifier: AGPL-3.0-or-later
//! Opt-in executable integration checks against the real native event loop.
//! This module is excluded from ordinary product builds. The native mode uses
//! the real protocol/current-window path, with a deterministic trusted client;
//! the platform mode uses the actual bundled webpage and platform host. Neither
//! mode submits model work. Settings mode exercises explicit local configuration
//! and product-scoped random credentials without any network authority.

use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock, mpsc};
use std::time::{Duration, Instant};

use concat_agent::Phase;
use concat_agent::operations::inspect_operations;
use concat_agent::sqlite::{DATABASE_FILE_NAME, inspect_storage};
use concat_api::AppDirs;
use concat_host::projects;
use concat_project::Command;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use slint::{ComponentHandle, Model};

use super::AgentBridge;
use super::native::{NativeSession, STORAGE_ID};
use crate::host::Shell;
use crate::ui::App;

mod settings;

#[derive(Clone)]
struct Configuration {
    mode: String,
    dirs: AppDirs,
    project: String,
    output: PathBuf,
    render_environment: Value,
}

static CONFIGURATION: OnceLock<Configuration> = OnceLock::new();

/// Entry point for the feature-gated example. Refuses an existing nonempty
/// portable directory, so a verification run cannot adopt user configuration.
pub fn run() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() != 2
        || args[0] != "--mode"
        || !matches!(args[1].as_str(), "native" | "platform" | "settings")
    {
        return Err("usage: bk-cat-native-probe --mode native|platform|settings".into());
    }
    let render_environment = render_environment();
    if args[1] != "native" {
        ensure(
            ["CONCAT_GPU", "SLINT_BACKEND", "SLINT_RENDERER"]
                .iter()
                .all(|name| std::env::var_os(name).is_none()),
            "platform/settings probe requires the application's default rendering without environment overrides",
        )?;
    }
    let executable = std::env::current_exe().map_err(|e| e.to_string())?;
    let output = executable
        .parent()
        .ok_or("probe executable has no parent")?
        .to_path_buf();
    let portable = output.join("portable");
    if portable.exists()
        && fs::read_dir(&portable)
            .map_err(|e| e.to_string())?
            .next()
            .is_some()
    {
        return Err("probe requires a new, empty portable directory beside its executable".into());
    }
    fs::create_dir_all(&portable).map_err(|e| e.to_string())?;
    let dirs = AppDirs::under(&portable);
    let located = AppDirs::locate()?;
    ensure(
        located.config == dirs.config && located.data == dirs.data,
        "portable application isolation was not selected",
    )?;
    let info = projects::create(
        portable
            .join("projects")
            .to_str()
            .ok_or("non-Unicode probe path")?,
        "BK CAT isolated integration",
        640,
        360,
        24,
        1,
        [0, 0, 0, 255],
    )?;
    CONFIGURATION
        .set(Configuration {
            mode: args[1].clone(),
            dirs,
            project: info.path.clone(),
            output,
            render_environment,
        })
        .map_err(|_| "probe already configured")?;
    // Only native mode requests software drawing for actual exported pixels.
    // Platform mode uses the existing application's default renderer selection;
    // initialization errors remain errors and there is no probe fallback.
    if args[1] == "native" {
        crate::draw_in_software();
    }
    crate::open_logging(None);
    crate::run_with_project(Some(PathBuf::from(info.path))).map_err(|e| e.to_string())
}

/// Called from the real window's first event-loop callback, before automatic
/// platform startup. An ordinary feature-enabled application is unchanged
/// unless the isolated example configured this process explicitly.
pub(crate) fn start_if_requested() -> bool {
    let Some(configuration) = CONFIGURATION.get().cloned() else {
        return false;
    };
    if configuration.mode != "native" {
        crate::embedded::start_for_window();
    }
    let failure_configuration = configuration.clone();
    if let Err(error) = std::thread::Builder::new()
        .name("bk-cat-isolated-probe".into())
        .spawn(move || {
            let mut steps = Vec::new();
            let result = if configuration.mode == "native" {
                native_mode(&configuration, &mut steps)
            } else if configuration.mode == "settings" {
                settings::run(&configuration, &mut steps)
            } else {
                platform_mode(&configuration, &mut steps)
            };
            let cleanup = on_ui(|shell, _| shell.studio.borrow_mut().close_project());
            let result = result.and(cleanup);
            let report = json!({
                "mode":configuration.mode,"passed":result.is_ok(),
                "error":result.as_ref().err(),"steps":steps,
                "project":configuration.project,"portable":configuration.dirs.data,
                "rendering":{
                    "requested":if configuration.mode=="native" {"software"} else {"application-default"},
                    "softwarePreferenceSetByProbe":configuration.mode=="native",
                    "environmentAtLaunch":configuration.render_environment,
                    "effectiveEnvironment":render_environment(),
                    "applicationAdapterDescription":crate::gpu::adapter_description(),
                    "adapterScope":"application adapter diagnostic; not a visual acceptance result",
                    "completeVisualAcceptance":false,
                },
                "scope":{"nativeWindow":true,"currentProject":true,
                    "realInternalProtocol":configuration.mode=="native",
                    "actualBundledWebpage":configuration.mode!="native",
                    "nativeSettingsCallbacks":configuration.mode=="settings",
                    "mouseKeyboardAcceptance":false,
                    "modelRequests":false,"androidDevice":false},
            });
            let saved = fs::write(
                configuration.output.join("probe-report.json"),
                serde_json::to_vec_pretty(&report).unwrap(),
            );
            println!("PROBE_RESULT {}", report);
            if result.is_err() || saved.is_err() {
                std::process::exit(1);
            }
            let _ = slint::invoke_from_event_loop(|| {
                let _ = slint::quit_event_loop();
            });
        })
    {
        let _ = fs::write(
            failure_configuration.output.join("probe-report.json"),
            json!({"passed":false,"error":error.to_string()}).to_string(),
        );
        std::process::exit(1);
    }
    true
}

fn ensure(condition: bool, message: &str) -> Result<(), String> {
    if condition {
        Ok(())
    } else {
        Err(message.to_owned())
    }
}

fn render_environment() -> Value {
    let mut values = serde_json::Map::new();
    for name in [
        "CONCAT_GPU",
        "SLINT_BACKEND",
        "SLINT_RENDERER",
        "WGPU_BACKEND",
        "WGPU_DX12_COMPILER",
    ] {
        values.insert(
            name.to_owned(),
            std::env::var_os(name)
                .map(|value| Value::String(value.to_string_lossy().into_owned()))
                .unwrap_or(Value::Null),
        );
    }
    Value::Object(values)
}

fn step(steps: &mut Vec<Value>, name: &str, evidence: Value) {
    println!("PROBE_STEP {name} {evidence}");
    steps.push(json!({"name":name,"passed":true,"evidence":evidence}));
}

fn on_ui<T: Send + 'static>(
    body: impl FnOnce(&Shell, &App) -> Result<T, String> + Send + 'static,
) -> Result<T, String> {
    let (sender, receiver) = mpsc::channel();
    slint::invoke_from_event_loop(move || {
        let mut result = Err("native window was not installed".to_owned());
        Shell::with(|shell, app| {
            result = body(shell, &app);
        });
        let _ = sender.send(result);
    })
    .map_err(|e| e.to_string())?;
    receiver
        .recv_timeout(Duration::from_secs(20))
        .map_err(|_| "native window callback timed out".to_owned())?
}

fn wait_for<T>(
    name: &str,
    mut read: impl FnMut() -> Result<Option<T>, String>,
) -> Result<T, String> {
    let started = Instant::now();
    loop {
        if let Some(result) = read()? {
            return Ok(result);
        }
        if started.elapsed() > Duration::from_secs(45) {
            return Err(format!("timed out waiting for {name}"));
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

fn bound(lease: &Value, method: &str, extra: Value) -> Value {
    let mut payload =
        json!({"method":method,"ownerHandle":lease["handle"],"generation":lease["generation"]});
    payload
        .as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    payload
}

fn protocol(
    native: &NativeSession,
    lease: &Value,
    message: Value,
    call: Option<&str>,
) -> Result<Value, String> {
    let mut payload = bound(lease, "mcp.send", json!({"message":message}));
    if let Some(call) = call {
        // Trusted deterministic integration-client identity, never model input.
        payload["invocation"] = json!({"conversationId":1,"taskId":1,"callId":call});
    }
    native.handle(payload)
}

fn tool(
    native: &NativeSession,
    lease: &Value,
    id: u64,
    name: &str,
    arguments: Value,
    call: Option<&str>,
) -> Result<Value, String> {
    protocol(
        native,
        lease,
        json!({"jsonrpc":"2.0","id":id,"method":"tools/call",
        "params":{"name":name,"arguments":arguments}}),
        call,
    )
}

fn successful(reply: Value) -> Result<Value, String> {
    if reply.get("error").is_some() || reply.pointer("/result/isError") == Some(&json!(true)) {
        return Err(format!("actual native protocol refused: {reply}"));
    }
    reply
        .pointer("/result/structuredContent")
        .cloned()
        .ok_or_else(|| format!("native tool reply has no structured result: {reply}"))
}

fn current_view(
    native: &NativeSession,
    lease: &Value,
    id: u64,
    path: &str,
) -> Result<Value, String> {
    successful(tool(
        native,
        lease,
        id,
        "project_get",
        json!({"path":path}),
        None,
    )?)
}

fn clips(view: &Value) -> Result<&Vec<Value>, String> {
    view.pointer("/project/timelines/0/clips")
        .and_then(Value::as_array)
        .ok_or_else(|| "native view has no timeline clips".into())
}

fn ui_view() -> Result<Value, String> {
    on_ui(|shell, _| {
        let studio = shell.studio.borrow();
        let session = studio
            .session
            .as_ref()
            .ok_or("native window has no project")?;
        let mut view = serde_json::to_value(session.view()).map_err(|e| e.to_string())?;
        view["publishedClipRows"] = json!(shell.models.clips.row_count());
        Ok(view)
    })
}

fn lock_track(track: String, locked: bool) -> Result<(), String> {
    on_ui(move |shell, app| {
        shell
            .studio
            .borrow_mut()
            .lanes
            .lane_view
            .entry(track)
            .or_default()
            .locked = locked;
        shell.studio.borrow().publish(app, &shell.models);
        Ok(())
    })
}

fn save_and_ack(
    native: &NativeSession,
    lease: &Value,
    bridge: &AgentBridge,
    path: &str,
    expected_revision: u64,
) -> Result<Value, String> {
    let expected = on_ui(|shell, _| {
        let mut studio = shell.studio.borrow_mut();
        let document = studio
            .session
            .as_ref()
            .ok_or("project closed before save")?
            .document();
        studio.save(false);
        Ok(document)
    })?;
    let notifications = wait_for("native Written save receipt", || {
        let saved = native.take_saved_notifications()?;
        Ok((!saved.is_empty()).then_some(saved))
    })?;
    ensure(
        notifications.len() == 1,
        "expected one actual saved operation",
    )?;
    let notification = &notifications[0];
    ensure(
        notification["saved"]["revision"] == expected_revision.to_string(),
        "save credited a different native revision",
    )?;
    let bytes = fs::read(PathBuf::from(path).join("concat.json")).map_err(|e| e.to_string())?;
    let document: Value = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
    ensure(
        document == expected,
        "saved document differs from the captured native session",
    )?;
    ensure(
        notification["saved"]["sha256"] == format!("{:x}", Sha256::digest(&bytes)),
        "saved receipt did not hash the actual disk bytes",
    )?;
    let journal = inspect_operations(&bridge.owner, &bridge.token).map_err(|e| e.to_string())?;
    ensure(
        journal.saved_not_notified == 1,
        "dequeue incorrectly acknowledged native completion",
    )?;
    native.handle(bound(
        lease,
        "operation.acknowledge",
        json!({"operationId":notification["operationId"]}),
    ))?;
    Ok(
        json!({"saved":notification["saved"],"operationId":notification["operationId"],
        "acknowledgingClient":"deterministic native probe, not a webpage"}),
    )
}

fn preview(
    configuration: &Configuration,
    time: f32,
    name: &str,
    previous: Option<String>,
) -> Result<Value, String> {
    let (baseline, session, revision) = on_ui(move |shell, app| {
        let mut studio = shell.studio.borrow_mut();
        let current = studio
            .session
            .as_ref()
            .ok_or("project closed before preview")?;
        let identity = (
            studio.monitor.probe_requested,
            current.session_id(),
            current.revision(),
        );
        studio.playhead = time;
        studio.request_preview();
        studio.publish(app, &shell.models);
        Ok(identity)
    })?;
    let (bytes, width, height, hash, frame) =
        wait_for("current exact native preview request", || {
            let previous = previous.clone();
            on_ui(move |shell, _| {
                let studio = shell.studio.borrow();
                let Some(frame) = studio.monitor.probe_frame else {
                    return Ok(None);
                };
                if frame.request <= baseline
                    || frame.session != session
                    || frame.revision != revision
                    || (frame.time - f64::from(time)).abs() > 1e-6
                    || !frame.exact
                {
                    return Ok(None);
                }
                let Some(pixels) = studio.monitor.image.to_rgba8() else {
                    return Ok(None);
                };
                let bytes = pixels.as_bytes();
                let hash = format!("{:x}", Sha256::digest(bytes));
                let visible = bytes
                    .chunks_exact(4)
                    .filter(|p| p[0] > 32 || p[1] > 32 || p[2] > 32)
                    .count();
                if visible < 20 || previous.as_ref() == Some(&hash) {
                    return Ok(None);
                }
                Ok(Some((
                    bytes.to_vec(),
                    pixels.width(),
                    pixels.height(),
                    hash,
                    frame,
                )))
            })
        })?;
    image::save_buffer(
        configuration.output.join(name),
        &bytes,
        width,
        height,
        image::ColorType::Rgba8,
    )
    .map_err(|e| e.to_string())?;
    Ok(
        json!({"sha256":hash,"request":frame.request,"time":frame.time,
        "session":frame.session,"revision":frame.revision,"exact":frame.exact}),
    )
}

fn window_snapshot(configuration: &Configuration, name: &str) -> Result<(), String> {
    let (bytes, width, height) = on_ui(|_, app| {
        let pixels = app.window().take_snapshot().map_err(|e| e.to_string())?;
        Ok((pixels.as_bytes().to_vec(), pixels.width(), pixels.height()))
    })?;
    image::save_buffer(
        configuration.output.join(name),
        &bytes,
        width,
        height,
        image::ColorType::Rgba8,
    )
    .map_err(|e| e.to_string())
}

fn native_mode(configuration: &Configuration, steps: &mut Vec<Value>) -> Result<(), String> {
    let bridge = on_ui(|shell, _| {
        shell
            .agent
            .borrow()
            .clone()
            .ok_or_else(|| "native owner unavailable".into())
    })?;
    let native = Arc::new(NativeSession::new(
        Arc::clone(&bridge),
        "isolated-native-probe".into(),
    )?);
    let result = native_steps(configuration, steps, &native, &bridge);
    let stopped = native.shutdown();
    result.and(stopped)
}

fn native_steps(
    configuration: &Configuration,
    steps: &mut Vec<Value>,
    native: &Arc<NativeSession>,
    bridge: &Arc<AgentBridge>,
) -> Result<(), String> {
    let lease = native.handle(json!({"method":"ownership.acquire","storageId":STORAGE_ID}))?;
    let snapshot = native.snapshot()?;
    ensure(
        snapshot["projectPath"] == configuration.project,
        "recovery inspected another project",
    )?;
    let native_for_ui = Arc::clone(native);
    let snapshot_id = snapshot["snapshotId"]
        .as_str()
        .ok_or("no snapshot identity")?
        .to_owned();
    let approval = on_ui(move |_, _| native_for_ui.approve_from_ui(&snapshot_id, "continue"))?;
    native.handle(bound(
        &lease,
        "ownership.confirm",
        json!({"snapshot":snapshot,"approval":approval}),
    ))?;
    let init = protocol(
        native,
        &lease,
        json!({"jsonrpc":"2.0","id":1,"method":"initialize",
        "params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"isolated-native-probe","version":"1"}}}),
        None,
    )?;
    ensure(
        init["result"]["protocolVersion"] == "2025-11-25",
        "pinned protocol handshake failed",
    )?;
    protocol(
        native,
        &lease,
        json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
        None,
    )?;
    let initial = current_view(native, &lease, 2, &configuration.project)?;
    ensure(
        clips(&initial)?.is_empty(),
        "isolated project did not start empty",
    )?;
    let track = initial
        .pointer("/project/timelines/0/tracks/0/id")
        .and_then(Value::as_str)
        .ok_or("no native track")?
        .to_owned();
    let plan = |view: &Value| {
        json!({"path":configuration.project,"expectedSession":view["sessionId"],"expectedRevision":view["revision"],
        "command":{"op":"batch","commands":[
            {"op":"addTextClip","trackId":track,"start":0.0,"duration":1.5,"style":{"content":"BK CAT - First subtitle"}},
            {"op":"addTextClip","trackId":track,"start":2.0,"duration":1.5,"style":{"content":"BK CAT - Second subtitle"}}
        ]}})
    };
    step(
        steps,
        "native_recovery_and_real_protocol",
        json!({"session":initial["sessionId"],"revision":initial["revision"]}),
    );

    lock_track(track.clone(), true)?;
    let before_lock_plan = current_view(native, &lease, 3, &configuration.project)?;
    let before_arguments = plan(&before_lock_plan);
    let refused = tool(
        native,
        &lease,
        4,
        "edit_apply_guarded",
        before_arguments,
        Some("locked-before-plan"),
    )?;
    ensure(
        refused.pointer("/result/isError") == Some(&json!(true)),
        "track locked before plan was not refused",
    )?;
    ensure(
        current_view(native, &lease, 5, &configuration.project)? == before_lock_plan,
        "locked batch partially changed the project",
    )?;
    lock_track(track.clone(), false)?;
    let after_plan = current_view(native, &lease, 6, &configuration.project)?;
    let after_arguments = plan(&after_plan);
    lock_track(track.clone(), true)?;
    let refused = tool(
        native,
        &lease,
        7,
        "edit_apply_guarded",
        after_arguments,
        Some("locked-after-plan"),
    )?;
    ensure(
        refused.pointer("/result/isError") == Some(&json!(true)),
        "track locked after plan was not refused",
    )?;
    ensure(
        current_view(native, &lease, 8, &configuration.project)? == after_plan,
        "late track lock allowed a partial batch",
    )?;
    lock_track(track.clone(), false)?;
    step(
        steps,
        "both_track_lock_timings_reject_the_whole_batch",
        json!({"clips":0}),
    );

    let unlocked = current_view(native, &lease, 90, &configuration.project)?;
    let arguments = plan(&unlocked);
    let applied = successful(tool(
        native,
        &lease,
        9,
        "edit_apply_guarded",
        arguments.clone(),
        Some("two-subtitles"),
    )?)?;
    let view = current_view(native, &lease, 10, &configuration.project)?;
    ensure(
        clips(&view)?.len() == 2,
        "native batch did not add both subtitles",
    )?;
    let shown = ui_view()?;
    ensure(
        shown["sessionId"] == view["sessionId"] && shown["publishedClipRows"] == 2,
        "current native timeline model did not publish the two clips",
    )?;
    let duplicate = tool(
        native,
        &lease,
        11,
        "edit_apply_guarded",
        arguments,
        Some("two-subtitles"),
    )?;
    ensure(
        duplicate.pointer("/result/isError") == Some(&json!(true)),
        "repeated original invocation was not refused",
    )?;
    ensure(
        clips(&current_view(native, &lease, 12, &configuration.project)?)?.len() == 2,
        "duplicate invocation repeated the batch",
    )?;
    let revision = applied["revision"]
        .as_u64()
        .ok_or("edit reply has no revision")?;
    let saved = save_and_ack(native, &lease, bridge, &configuration.project, revision)?;
    step(
        steps,
        "two_subtitles_one_batch_and_real_written_receipt",
        saved,
    );
    let first = preview(configuration, 0.5, "preview-first.png", None)?;
    let second = preview(
        configuration,
        2.5,
        "preview-second.png",
        first["sha256"].as_str().map(str::to_owned),
    )?;
    window_snapshot(configuration, "native-window.png")?;
    step(
        steps,
        "native_preview_contains_two_distinct_rendered_frames",
        json!({"first":first,"second":second}),
    );

    on_ui(|shell, app| {
        shell.studio.borrow_mut().undo();
        shell.studio.borrow().publish(app, &shell.models);
        Ok(())
    })?;
    let undone = current_view(native, &lease, 13, &configuration.project)?;
    ensure(
        clips(&undone)?.is_empty() && undone["canRedo"] == true,
        "one native undo did not remove the complete batch",
    )?;
    ensure(
        ui_view()?["publishedClipRows"] == 0,
        "undo did not update the current timeline",
    )?;
    on_ui(|shell, app| {
        shell.studio.borrow_mut().redo();
        shell.studio.borrow().publish(app, &shell.models);
        Ok(())
    })?;
    let redone = current_view(native, &lease, 14, &configuration.project)?;
    ensure(
        clips(&redone)?.len() == 2,
        "one native redo did not restore both subtitles",
    )?;
    step(
        steps,
        "single_native_undo_and_redo_share_protocol_history",
        json!({"undoRevision":undone["revision"],"redoRevision":redone["revision"]}),
    );

    let clip_id = clips(&redone)?[0]["id"]
        .as_str()
        .ok_or("no subtitle id")?
        .to_owned();
    let manual_id = clip_id.clone();
    on_ui(move |shell, app| {
        let command: Command = serde_json::from_value(
            json!({"op":"updateClip","clipId":manual_id,"patch":{"name":"Manual subtitle label"}}),
        )
        .map_err(|e| e.to_string())?;
        shell.studio.borrow_mut().apply(command);
        shell.studio.borrow().publish(app, &shell.models);
        Ok(())
    })?;
    let manual = current_view(native, &lease, 15, &configuration.project)?;
    ensure(
        manual["sessionId"] == initial["sessionId"]
            && clips(&manual)?[0]["name"] == "Manual subtitle label",
        "manual editing did not update the same session",
    )?;
    let stale = tool(
        native,
        &lease,
        16,
        "edit_apply_guarded",
        json!({"path":configuration.project,"expectedSession":redone["sessionId"],"expectedRevision":redone["revision"],
        "command":{"op":"updateClip","clipId":clip_id,"patch":{"name":"stale must not land"}}}),
        Some("stale-after-manual"),
    )?;
    ensure(
        stale.pointer("/result/isError") == Some(&json!(true)),
        "old plan survived a manual edit",
    )?;
    let continued = successful(tool(
        native,
        &lease,
        17,
        "edit_apply_guarded",
        json!({"path":configuration.project,"expectedSession":manual["sessionId"],"expectedRevision":manual["revision"],
        "command":{"op":"batch","commands":[{"op":"updateClip","clipId":clip_id,"patch":{"name":"Continued native protocol edit"}}]}}),
        Some("continue-after-manual"),
    )?)?;
    let final_view = current_view(native, &lease, 18, &configuration.project)?;
    ensure(
        clips(&final_view)?.len() == 2
            && clips(&final_view)?[0]["name"] == "Continued native protocol edit",
        "continued protocol edit missed the manually edited project",
    )?;
    let saved = save_and_ack(
        native,
        &lease,
        bridge,
        &configuration.project,
        continued["revision"]
            .as_u64()
            .ok_or("no continued revision")?,
    )?;
    let journal = inspect_operations(&bridge.owner, &bridge.token).map_err(|e| e.to_string())?;
    ensure(
        journal.needs_project_reconciliation == 0
            && journal.applied_not_saved == 0
            && journal.saved_not_notified == 0,
        "normal protected refusals left a stuck operation",
    )?;
    step(
        steps,
        "manual_then_protocol_edit_and_stale_plan_refusal",
        json!({"save":saved,"notified":journal.notified,"notApplied":journal.not_applied}),
    );
    Ok(())
}

fn platform_mode(configuration: &Configuration, steps: &mut Vec<Value>) -> Result<(), String> {
    let (runtime, bridge) = on_ui(|shell, _| {
        Ok((
            shell
                .embedded
                .borrow()
                .clone()
                .ok_or("platform runtime absent")?,
            shell.agent.borrow().clone().ok_or("native owner absent")?,
        ))
    })?;
    let first = wait_for("actual bundled webpage recovery", || {
        let view = runtime.view();
        if !view.error.is_empty() {
            return Err(view.error);
        }
        Ok((view.phase == "recovery" && view.snapshot.is_some()).then_some(view))
    })?;
    ensure(
        first.loaded && bridge.status()?.phase == Phase::RecoveryRequired,
        "page load auto-approved execution",
    )?;
    ensure(
        !bridge
            .owner
            .storage_root()
            .join(DATABASE_FILE_NAME)
            .exists(),
        "page opened writable storage before user approval",
    )?;
    let generation = bridge.status()?.generation;
    let component = runtime.component().to_owned();
    let confirming = Arc::clone(&runtime);
    on_ui(move |_, _| confirming.confirm())?;
    wait_for("actual bundled webpage ready", || {
        let view = runtime.view();
        if !view.error.is_empty() {
            return Err(view.error);
        }
        Ok((view.phase == "ready").then_some(()))
    })?;
    ensure(
        bridge.status()?.phase == Phase::Ready,
        "ready page has no native authority",
    )?;
    let storage = inspect_storage(&bridge.owner, &bridge.token).map_err(|e| e.to_string())?;
    ensure(
        storage.compatible,
        "original Pi did not create its actual native session schema",
    )?;
    let current = ui_view()?;
    ensure(
        clips(&current)?.is_empty(),
        "platform startup changed the isolated project",
    )?;
    match window_snapshot(configuration, "platform-ready-window.png") {
        Ok(()) => step(
            steps,
            "platform_window_snapshot_captured",
            json!({
                "file":"platform-ready-window.png", "completeVisualAcceptance":false,
            }),
        ),
        Err(error) => {
            println!("PROBE_EVIDENCE_LIMITATION window_snapshot {error}");
            steps.push(
                json!({"name":"platform_window_snapshot_captured", "passed":false,
                "evidenceOnly":true,"error":error,"completeVisualAcceptance":false}),
            );
        }
    }
    step(
        steps,
        "real_webpage_native_window_and_original_pi_storage",
        json!({"generation":generation,"schema":storage.schema_version,"counts":storage.counts}),
    );
    let stopping = Arc::clone(&runtime);
    on_ui(move |_, _| {
        stopping.stop();
        Ok(())
    })?;
    wait_for("actual platform destruction and native release", || {
        let view = runtime.view();
        Ok((view.closed && bridge.status()?.phase == Phase::Released).then_some(()))
    })?;
    step(
        steps,
        "actual_platform_closed_and_native_owner_released",
        json!({"generation":generation,
        "actualClosedAcknowledged":runtime.view().closed,"ownerReleased":bridge.status()?.phase==Phase::Released}),
    );
    on_ui(|_, _| crate::embedded::connect_from_ui())?;
    let (replacement, next) = on_ui(|shell, _| {
        Ok((
            shell
                .embedded
                .borrow()
                .clone()
                .ok_or("replacement runtime absent")?,
            shell
                .agent
                .borrow()
                .clone()
                .ok_or("replacement owner absent")?,
        ))
    })?;
    let reopened = wait_for("replacement page recovery", || {
        let view = replacement.view();
        if !view.error.is_empty() {
            return Err(view.error);
        }
        Ok((view.phase == "recovery" && view.snapshot.is_some()).then_some(view))
    })?;
    ensure(
        replacement.component() != component && next.status()?.generation > generation,
        "replacement reused the old component generation",
    )?;
    ensure(
        next.status()?.phase == Phase::RecoveryRequired,
        "replacement automatically revived execution",
    )?;
    let snapshot = reopened
        .snapshot
        .ok_or("replacement has no recovery evidence")?;
    ensure(
        snapshot["schemaVersion"] == 2 && snapshot["hasInterruptedWork"] == false,
        "replacement did not inspect original stored session",
    )?;
    std::thread::sleep(Duration::from_millis(500));
    ensure(
        replacement.view().phase == "recovery" && next.status()?.phase == Phase::RecoveryRequired,
        "idle replacement auto-approved recovery",
    )?;
    let stopping = Arc::clone(&replacement);
    on_ui(move |_, _| {
        stopping.stop();
        Ok(())
    })?;
    wait_for("replacement platform destruction", || {
        Ok((replacement.view().closed && next.status()?.phase == Phase::Released).then_some(()))
    })?;
    step(
        steps,
        "explicit_reconnect_reopens_persistent_session_without_scheduling",
        json!({"generation":next.status()?.generation,"schemaVersion":snapshot["schemaVersion"]}),
    );
    Ok(())
}
