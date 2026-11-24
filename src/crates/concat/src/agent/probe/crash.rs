// SPDX-License-Identifier: AGPL-3.0-or-later
//! Whole-window process-crash acceptance, available only in the opt-in probe.
//!
//! The parent driver owns a loopback-only model fixture and terminates this
//! entire process after a flushed checkpoint. The real window, bundled Pi,
//! tool approval, guarded editor, journal and ordered save lane do the work.
//! No checkpoint, configuration switch or test port exists in product builds.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use concat_agent::configuration::{ChannelDraft, ModelProtocol};
use concat_agent::credentials::{CredentialScope, CredentialStatus, Secret, platform_store};
use concat_agent::operations::{RecoveryReview, inspect_operations, inspect_recovery};
use concat_agent::sqlite::inspect_interrupted_work;
use concat_api::AppDirs;
use concat_host::projects;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use slint::ComponentHandle;

use super::{ensure, on_ui, ui_view, wait_for};

const MARKER: &str = "whole-process-case.json";
const FIXTURE_MODEL: &str = "bkcut-controlled-crash-fixture";
const KIND: &str = "bkcut-whole-window-process-crash-v1";
// Stable native terminal contract; translated display wording is irrelevant.
const UNPROVEN_EDIT_NOTICE: &str = "runtime.editUnverified";

/// Three points in the actual journaled native edit, not simulated rows.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Point {
    Intent,
    Applied,
    Receipt,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum Stage {
    Intent,
    Applied,
    Receipt,
    Saved,
    SaveFirst,
}

impl Stage {
    fn parse(value: &str) -> Result<Self, String> {
        match value {
            "intent" => Ok(Self::Intent),
            "applied" => Ok(Self::Applied),
            "receipt" => Ok(Self::Receipt),
            "saved" => Ok(Self::Saved),
            "save-first" => Ok(Self::SaveFirst),
            _ => Err("unknown whole-process checkpoint".into()),
        }
    }

    fn point(self) -> Point {
        match self {
            Self::Intent => Point::Intent,
            Self::Applied | Self::SaveFirst => Point::Applied,
            Self::Receipt | Self::Saved => Point::Receipt,
        }
    }

    fn saved(self) -> bool {
        matches!(self, Self::Saved | Self::SaveFirst)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Case {
    schema: u32,
    kind: String,
    run_id: String,
    stage: Stage,
    decision: String,
    root: PathBuf,
    project: String,
    endpoint: String,
    channel: String,
    credential_origin: String,
    credential_digest: String,
}

#[derive(Clone)]
struct Configuration {
    case: Case,
    dirs: AppDirs,
    phase: String,
    render_environment: Value,
}

static CONFIGURATION: OnceLock<Configuration> = OnceLock::new();
static HIT: AtomicBool = AtomicBool::new(false);

fn argument<'a>(args: &'a [String], key: &str) -> Result<&'a str, String> {
    let mut values = args.windows(2).filter(|pair| pair[0] == key);
    let value = values.next().ok_or_else(|| format!("missing {key}"))?[1].as_str();
    ensure(values.next().is_none(), "duplicate acceptance argument")?;
    Ok(value)
}

/// Called by the already feature-gated example, never by the product parser.
pub(super) fn run(args: &[String]) -> Result<(), String> {
    ensure(
        cfg!(target_os = "windows"),
        "whole-process driver currently supports Windows",
    )?;
    ensure(
        args.len() == 12,
        "expected --mode crash --phase interrupt|recover|cleanup --stage NAME --decision continue|discard --fixture-port PORT --run-id ID",
    )?;
    for pair in args.chunks_exact(2) {
        ensure(
            [
                "--mode",
                "--phase",
                "--stage",
                "--decision",
                "--fixture-port",
                "--run-id",
            ]
            .contains(&pair[0].as_str()),
            "unknown acceptance argument",
        )?;
    }
    let phase = argument(args, "--phase")?;
    ensure(
        matches!(phase, "interrupt" | "recover" | "cleanup"),
        "unknown acceptance phase",
    )?;
    let stage = Stage::parse(argument(args, "--stage")?)?;
    let decision = argument(args, "--decision")?;
    ensure(
        matches!(decision, "continue" | "discard"),
        "unknown acceptance decision",
    )?;
    let run_id = argument(args, "--run-id")?;
    ensure(
        run_id.len() == 32 && run_id.bytes().all(|b| b.is_ascii_hexdigit()),
        "run identity must be 128-bit hexadecimal",
    )?;
    let port: u16 = argument(args, "--fixture-port")?
        .parse()
        .map_err(|_| "invalid loopback fixture port")?;
    ensure(port != 0, "fixture port must already be bound")?;
    let endpoint = format!("http://127.0.0.1:{port}/{run_id}/v1");
    let executable = std::env::current_exe().map_err(|e| e.to_string())?;
    let root = executable
        .parent()
        .ok_or("acceptance executable has no parent")?
        .to_path_buf();
    ensure(
        root.is_absolute(),
        "acceptance executable path is not absolute",
    )?;
    ordinary_directory(&root)?;
    let portable = root.join("portable");
    let case = if phase == "interrupt" {
        ensure(
            !root.join(MARKER).exists(),
            "case identity already exists; refusing to overwrite a previous run",
        )?;
        ensure(
            !portable.exists()
                || fs::read_dir(&portable)
                    .map_err(|e| e.to_string())?
                    .next()
                    .is_none(),
            "acceptance requires a new empty portable directory",
        )?;
        fs::create_dir_all(&portable).map_err(|e| e.to_string())?;
        ordinary_directory(&portable)?;
        let dirs = AppDirs::under(&portable);
        let info = projects::create(
            portable
                .join("projects")
                .to_str()
                .ok_or("non-Unicode acceptance path")?,
            "BK CUT isolated whole-process acceptance",
            640,
            360,
            24,
            1,
            [0, 0, 0, 255],
        )?;
        let store = crate::panes::agent_settings::configuration_store(&dirs);
        let initial = store.load().map_err(|e| e.to_string())?;
        ensure(
            initial.channels().is_empty(),
            "acceptance configuration was not isolated",
        )?;
        let saved = store
            .save_channel(
                &initial,
                None,
                ChannelDraft {
                    label: "BK CUT local deterministic acceptance".into(),
                    protocol: ModelProtocol::OpenAiCompletions,
                    endpoint: endpoint.clone(),
                    model: FIXTURE_MODEL.into(),
                    request_timeout_ms: 120_000,
                    max_output_tokens: 2048,
                },
            )
            .map_err(|e| e.to_string())?;
        let id = saved.channels()[0].id.clone();
        let saved = store
            .set_context_override(&saved, &id, FIXTURE_MODEL, Some(32_768))
            .map_err(|e| e.to_string())?;
        let saved = store.select(&saved, Some(&id)).map_err(|e| e.to_string())?;
        let channel = saved.active().ok_or("fixture model was not selected")?;
        let origin = channel.service_origin().map_err(|e| e.to_string())?;
        let scope =
            CredentialScope::new(channel.id.as_str(), &origin).map_err(|e| e.to_string())?;
        let vault = platform_store().map_err(|e| e.to_string())?;
        ensure(
            vault.status(&scope).map_err(|e| e.to_string())? == CredentialStatus::Missing,
            "fresh acceptance credential identity already exists",
        )?;
        let secret =
            Secret::new(crate::embedded::random_capability()?).map_err(|e| e.to_string())?;
        let case = Case {
            schema: 1,
            kind: KIND.into(),
            run_id: run_id.into(),
            stage,
            decision: decision.into(),
            root: root.clone(),
            project: info.path,
            endpoint,
            channel: channel.id.as_str().into(),
            credential_origin: origin,
            credential_digest: digest(secret.expose_secret().as_bytes()),
        };
        // The non-secret cleanup identity is flushed before writing the vault.
        // A failed launch can therefore remove only this exact random entry.
        write_json(
            &root.join(MARKER),
            &serde_json::to_value(&case).map_err(|e| e.to_string())?,
            true,
        )?;
        vault.write(&scope, &secret).map_err(|e| e.to_string())?;
        case
    } else {
        ordinary_directory(&portable)?;
        let case: Case = serde_json::from_slice(&ordinary_file(&root.join(MARKER))?)
            .map_err(|e| e.to_string())?;
        ensure(
            case.schema == 1
                && case.kind == KIND
                && case.root == root
                && case.run_id == run_id
                && case.stage == stage
                && case.decision == decision
                && case.endpoint == endpoint,
            "acceptance restart identity does not match the original isolated case",
        )?;
        let path = Path::new(&case.project)
            .canonicalize()
            .map_err(|e| e.to_string())?;
        let projects_root = portable
            .join("projects")
            .canonicalize()
            .map_err(|e| e.to_string())?;
        ensure(
            path.starts_with(projects_root) && projects::is_project(&path),
            "acceptance project escaped its isolated directory",
        )?;
        case
    };
    if phase == "cleanup" {
        cleanup_credential(&case)?;
        return write_json(
            &root.join("credential-cleanup.json"),
            &json!({"removedOnlyGeneratedTestCredential":true,"runId":case.run_id}),
            false,
        );
    }
    let dirs = AppDirs::under(&portable);
    let located = AppDirs::locate()?;
    ensure(
        located.config == dirs.config && located.data == dirs.data,
        "portable application isolation was not selected",
    )?;
    ensure(
        ["CONCAT_GPU", "SLINT_BACKEND", "SLINT_RENDERER"]
            .iter()
            .all(|name| std::env::var_os(name).is_none()),
        "whole-window acceptance uses the application's default renderer",
    )?;
    let path = PathBuf::from(&case.project);
    CONFIGURATION
        .set(Configuration {
            case,
            dirs,
            phase: phase.into(),
            render_environment: super::render_environment(),
        })
        .map_err(|_| "whole-process acceptance already configured")?;
    crate::open_logging(None);
    crate::run_with_project(Some(path)).map_err(|e| e.to_string())
}

fn cleanup_credential(case: &Case) -> Result<(), String> {
    let scope =
        CredentialScope::new(&case.channel, &case.credential_origin).map_err(|e| e.to_string())?;
    let vault = platform_store().map_err(|e| e.to_string())?;
    if let Some(secret) = vault.read(&scope).map_err(|e| e.to_string())? {
        ensure(
            digest(secret.expose_secret().as_bytes()) == case.credential_digest,
            "test credential changed; refusing to delete a different value",
        )?;
        vault.delete(&scope).map_err(|e| e.to_string())?;
    }
    ensure(
        vault.status(&scope).map_err(|e| e.to_string())? == CredentialStatus::Missing,
        "test credential removal was not verified",
    )
}

/// Deterministic save scheduling is part of this explicitly requested fault.
/// Explicit Studio::save still uses the production save worker and receipts.
pub(crate) fn hold_autosave() -> bool {
    CONFIGURATION.get().is_some()
}

pub(super) fn start_if_requested() -> bool {
    let Some(configuration) = CONFIGURATION.get().cloned() else {
        return false;
    };
    let output = configuration.case.root.clone();
    let error_output = output.clone();
    let result = std::thread::Builder::new().name("bkcut-whole-process-acceptance".into()).spawn(move || {
        let result = drive(&configuration);
        if let Err(error) = result {
            let _ = write_json(&output.join(format!("{}-failure.json", configuration.phase)),
                &json!({"passed":false,"error":error,"pid":std::process::id(),"runId":configuration.case.run_id}), false);
            std::process::exit(1);
        }
        if configuration.phase == "interrupt" {
            let _ = write_json(&output.join("interrupt-failure.json"), &json!({"passed":false,"error":"checkpoint did not keep the whole process alive for external termination"}), false);
            std::process::exit(1);
        }
        if let Err(error) = cleanup_credential(&configuration.case) {
            let _ = write_json(&output.join("recover-failure.json"), &json!({"passed":false,"error":error}), false);
            std::process::exit(1);
        }
        let _ = slint::invoke_from_event_loop(|| { let _ = slint::quit_event_loop(); });
    });
    if let Err(error) = result {
        let _ = write_json(
            &error_output.join("launch-failure.json"),
            &json!({"passed":false,"error":error.to_string()}),
            false,
        );
        std::process::exit(1);
    }
    true
}

fn drive(configuration: &Configuration) -> Result<(), String> {
    let path = configuration.case.project.clone();
    wait_for("isolated project in actual editor", || {
        let path = path.clone();
        on_ui(move |shell, app| {
            let studio = shell.studio.borrow();
            let found = studio
                .session
                .as_ref()
                .is_some_and(|session| session.path() == path);
            app.global::<crate::ui::AgentUi>().set_sidebar_open(true);
            Ok((found && shell.agent.borrow().is_some()).then_some(()))
        })
    })?;
    on_ui(|shell, _| {
        if shell.embedded.borrow().is_none() {
            crate::embedded::connect_from_ui()?;
        }
        Ok(())
    })?;
    let runtime = wait_for("actual bundled runtime", || {
        on_ui(|shell, _| Ok(shell.embedded.borrow().clone()))
    })?;
    let target = if configuration.phase == "interrupt" {
        "ready"
    } else {
        "recovery"
    };
    let startup = wait_for("actual runtime recovery boundary", || {
        let view = runtime.view();
        if !view.error.is_empty() {
            return Err(view.error);
        }
        Ok((view.phase == target && view.snapshot.is_some()).then_some(view))
    })?;
    if configuration.phase == "interrupt" {
        ensure(
            !startup.task_running,
            "startup started a model task without an explicit send",
        )?;
        let view = ui_view()?;
        ensure(
            super::clips(&view)?.is_empty(),
            "isolated timeline must start empty",
        )?;
        write_plan(configuration, &view)?;
        snapshot(configuration, "before-explicit-send.png")?;
        on_ui(|_, app| {
            app.global::<crate::ui::AgentUi>().invoke_edit_requested(
                "整应用进程异常终止的隔离验收：按本机受控模型夹具提出一条文字剪辑，等待原生批准。"
                    .into(),
            );
            Ok(())
        })?;
        let permission = wait_for("real native edit approval", || {
            let permission = runtime.tool_permission_view()?;
            let view = runtime.view();
            if !view.error.is_empty() {
                return Err(view.error);
            }
            Ok(permission.pending_id.as_ref().map(|_| permission.clone()))
        })?;
        ensure(
            permission.mode == 0,
            "new project did not default to request approval",
        )?;
        snapshot(configuration, "before-native-approval.png")?;
        let request = permission
            .pending_id
            .ok_or("native approval identity absent")?;
        on_ui(move |_, app| {
            app.global::<crate::ui::AgentUi>()
                .invoke_tool_decision_requested(request.into(), true);
            Ok(())
        })?;
        let began = Instant::now();
        loop {
            // The hook runs in the original native request worker. This separate
            // driver keeps observing real failure rather than manufacturing a
            // receipt or invoking a second edit when that worker is stopped.
            let view = runtime.view();
            if !view.error.is_empty() {
                return Err(view.error);
            }
            ensure(
                began.elapsed() < Duration::from_secs(150),
                "external parent never terminated the checkpoint process",
            )?;
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    recover(
        configuration,
        runtime,
        startup.snapshot.ok_or("recovery snapshot absent")?,
    )
}

fn write_plan(configuration: &Configuration, view: &Value) -> Result<(), String> {
    let track = view
        .pointer("/project/timelines/0/tracks/0/id")
        .and_then(Value::as_str)
        .ok_or("current native track absent")?;
    write_json(
        &configuration.case.root.join("fixture-plan.json"),
        &json!({
            "path":configuration.case.project,"expectedSession":view["sessionId"],"expectedRevision":view["revision"],
            "command":{"op":"addTextClip","trackId":track,"start":0.0,"duration":2.0,
                "style":{"content":"BK CUT whole-process checkpoint"}}
        }),
        false,
    )
}

fn bridge() -> Result<std::sync::Arc<super::super::AgentBridge>, String> {
    on_ui(|shell, _| {
        shell
            .agent
            .borrow()
            .clone()
            .ok_or("current native owner absent".into())
    })
}

fn review() -> Result<RecoveryReview, String> {
    let configuration = CONFIGURATION
        .get()
        .ok_or("whole-process acceptance not configured")?;
    let bridge = bridge()?;
    inspect_recovery(&bridge.owner, &bridge.token, &configuration.case.project)
        .map_err(|e| e.to_string())
}

fn disk_document(configuration: &Configuration) -> Result<(Vec<u8>, Value), String> {
    let bytes = ordinary_file(&projects::manifest_path(Path::new(
        &configuration.case.project,
    )))?;
    let document = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
    Ok((bytes, document))
}

fn document_clips(document: &Value) -> Result<usize, String> {
    let project = document.get("project").unwrap_or(document);
    if projects::is_settings_only(project) {
        return Ok(0);
    }
    let timelines = project
        .get("timelines")
        .and_then(Value::as_array)
        .ok_or("disk project has no native timelines")?;
    Ok(timelines
        .iter()
        .map(|timeline| timeline["clips"].as_array().map_or(0, Vec::len))
        .sum())
}

/// Blocks only an explicitly configured isolated probe at the real mutation
/// boundary. The parent must terminate the complete OS process. Returning an
/// error is never counted as a crash or as a successful acceptance result.
pub(crate) fn checkpoint(
    point: Point,
    operation_id: u64,
    reply: Option<&Value>,
) -> Result<(), String> {
    let Some(configuration) = CONFIGURATION.get() else {
        return Ok(());
    };
    if configuration.phase != "interrupt" || configuration.case.stage.point() != point {
        return Ok(());
    }
    ensure(
        !HIT.swap(true, Ordering::AcqRel),
        "checkpoint was entered twice",
    )?;
    if configuration.case.stage.saved() {
        on_ui(|shell, _| {
            shell.studio.borrow_mut().save(false);
            Ok(())
        })?;
        wait_for("actual ordered save and durable operation evidence", || {
            let inspected = review()?;
            let operation = inspected
                .operations
                .iter()
                .find(|row| row.operation_id == operation_id)
                .ok_or("native intent vanished")?;
            let ready = if configuration.case.stage == Stage::Saved {
                operation.saved.is_some() && operation.current_file_matches_save
            } else {
                !operation.saved_document_candidates.is_empty() && operation.receipt.is_none()
            };
            Ok(ready.then_some(()))
        })?;
    }
    let view = ui_view()?;
    let (bytes, document) = disk_document(configuration)?;
    let inspected = review()?;
    ensure(
        inspected.operations.len() == 1,
        "unexpected previous operation in isolated journal",
    )?;
    let operation = &inspected.operations[0];
    ensure(
        operation.operation_id == operation_id && operation.current_project && !operation.reviewed,
        "checkpoint operation identity differs",
    )?;
    let memory_count = super::clips(&view)?.len();
    let disk_count = document_clips(&document)?;
    ensure(
        memory_count == usize::from(configuration.case.stage != Stage::Intent),
        "checkpoint memory does not match actual native application stage",
    )?;
    ensure(
        disk_count == usize::from(configuration.case.stage.saved()),
        "checkpoint disk contents do not match the isolated save stage",
    )?;
    ensure(
        operation.receipt.is_some()
            == matches!(configuration.case.stage, Stage::Receipt | Stage::Saved),
        "receipt persistence does not match the selected checkpoint",
    )?;
    ensure(
        operation.saved.is_some() == (configuration.case.stage == Stage::Saved),
        "operation save proof does not match the selected checkpoint",
    )?;
    ensure(
        !operation.saved_document_candidates.is_empty() == configuration.case.stage.saved(),
        "captured save evidence does not match the selected checkpoint",
    )?;
    ensure(
        inspected.inspection.notified == 0,
        "completion was delivered before the checkpoint",
    )?;
    let bridge = bridge()?;
    let work = inspect_interrupted_work(&bridge.owner, &bridge.token).map_err(|e| e.to_string())?;
    ensure(
        !work.tasks.is_empty(),
        "original Pi has no durable interrupted task at checkpoint",
    )?;
    snapshot(configuration, "checkpoint-window.png")?;
    let event = json!({"schema":1,"kind":KIND,"runId":configuration.case.run_id,"stage":configuration.case.stage,
        "pid":std::process::id(),"ownerGeneration":bridge.status()?.generation,
        "project":configuration.case.project,"memoryView":view,"diskDocument":document,
        "diskSha256":digest(&bytes),"nativeReply":reply,"operationId":operation_id,
        "operations":inspected.operations,"operationFacts":inspected.facts(),
        "piWork":{"tasks":work.tasks,"submissions":work.submissions,"drainingSubmission":work.draining_submission},
        "fixtureModel":true,"fixtureOrigin":configuration.case.endpoint,
        "wholeApplicationStillRunning":true,"requiresExternalForceTermination":true,
        "nativeUiCallbacks":true,"osMouseInput":false,"autosaveHeldForFaultScheduling":true,
        "rendering":{"requested":"application-default","environment":configuration.render_environment,
            "adapter":crate::gpu::adapter_description()}});
    write_json(
        &configuration.case.root.join("checkpoint.json"),
        &event,
        true,
    )?;
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(120) {
        std::thread::park_timeout(Duration::from_secs(1));
    }
    Err("parent did not force-terminate the entire application at the persistent checkpoint".into())
}

fn recover(
    configuration: &Configuration,
    runtime: std::sync::Arc<crate::embedded::EmbeddedRuntime>,
    snapshot_value: Value,
) -> Result<(), String> {
    let checkpoint: Value = serde_json::from_slice(&ordinary_file(
        &configuration.case.root.join("checkpoint.json"),
    )?)
    .map_err(|e| e.to_string())?;
    let before_view = ui_view()?;
    let (before_bytes, before_document) = disk_document(configuration)?;
    let original = review()?;
    let bridge = bridge()?;
    ensure(
        checkpoint["runId"] == configuration.case.run_id && checkpoint["pid"] != std::process::id(),
        "restart is not a different full application process",
    )?;
    ensure(
        bridge.status()?.generation
            > checkpoint["ownerGeneration"]
                .as_u64()
                .ok_or("old owner generation absent")?,
        "restart reused an old storage owner generation",
    )?;
    ensure(
        digest(&before_bytes) == checkpoint["diskSha256"],
        "restart changed the persisted project before a decision",
    )?;
    ensure(
        super::clips(&before_view)?.len() == usize::from(configuration.case.stage.saved()),
        "reopened timeline does not match persisted contents",
    )?;
    ensure(
        snapshot_value["hasUnfinishedPi"] == true
            && snapshot_value["canContinueInterrupted"] == true,
        "actual original task cannot be reviewed and explicitly continued",
    )?;
    ensure(
        original.operations.len() == 1 && original.unreviewed_current == 1,
        "restart lost the exact unreviewed operation",
    )?;
    ensure(
        serde_json::to_value(&original.operations).map_err(|e| e.to_string())?
            == checkpoint["operations"],
        "restart changed original operation state, receipt, save candidates or fact fingerprint",
    )?;
    ensure(
        original.operations[0].saved.is_some() == (configuration.case.stage == Stage::Saved),
        "restart turned a saved document into an unproven operation receipt",
    )?;
    let server_before = stats(configuration)?;
    ensure(
        server_before["requests"] == 1 && server_before["recoveryRequests"] == 0,
        "application startup contacted the fixture before a recovery decision",
    )?;
    std::thread::sleep(Duration::from_millis(750));
    ensure(
        !runtime.view().task_running
            && stats(configuration)?["requests"] == server_before["requests"],
        "restart scheduled model work before explicit recovery",
    )?;
    ensure(
        ui_view()?["revision"] == before_view["revision"],
        "restart replayed a native edit without an explicit decision",
    )?;
    snapshot(configuration, "recovery-before-choice.png")?;
    write_json(
        &configuration.case.root.join("recovery-before-choice.json"),
        &json!({
        "pid":std::process::id(),"view":before_view,"diskDocument":before_document,
        "diskSha256":digest(&before_bytes),"snapshot":snapshot_value,"operations":original.operations,
        "server":server_before,"noAutomaticRequestOrEdit":true}),
        true,
    )?;
    write_plan(configuration, &before_view)?;
    // Locking the target before Continue proves the old decision does not
    // authorize a future plan through the current native track guard.
    let lock_probe = configuration.case.decision == "continue" && !configuration.case.stage.saved();
    let track = before_view
        .pointer("/project/timelines/0/tracks/0/id")
        .and_then(Value::as_str)
        .ok_or("native target track absent")?
        .to_owned();
    if lock_probe {
        super::lock_track(track.clone(), true)?;
    }
    let locked_before = lock_probe.then(ui_view).transpose()?;
    let decision = configuration.case.decision.clone();
    on_ui(move |_, app| {
        let ui = app.global::<crate::ui::AgentUi>();
        if decision == "continue" {
            ui.invoke_continue_requested();
        } else {
            ui.invoke_discard_requested();
        }
        Ok(())
    })?;
    let mut approved = std::collections::BTreeSet::new();
    let mut locked_refusal_observed = false;
    let mut resumed_request_observed = false;
    let mut unproven_notice = None;
    let began = Instant::now();
    loop {
        let view = runtime.view();
        if !view.error.is_empty() {
            ensure(
                configuration.case.stage == Stage::SaveFirst
                    && configuration.case.decision == "continue"
                    && view.error == UNPROVEN_EDIT_NOTICE
                    && !view.task_running,
                &format!("unexpected resumed-task failure: {}", view.error),
            )?;
            unproven_notice = Some(view.error.clone());
            snapshot(configuration, "unproven-original-operation-notice.png")?;
            write_json(
                &configuration
                    .case
                    .root
                    .join("unproven-original-operation.json"),
                &json!({
                    "notice":view.error,"originalOperationClaimedSuccessful":false,
                    "expectedNativeGuardRefusal":true,"projectRetained":ui_view()?
                }),
                true,
            )?;
            // The ordinary completion guard retires an unproven edit task.
            // Reconnect through the real UI entry only after that old native
            // owner is released, then inspect; never read using a stale lease.
            wait_for("unproven task native retirement", || {
                Ok((runtime.view().closed
                    && bridge.status()?.phase == concat_agent::Phase::Released)
                    .then_some(()))
            })?;
            on_ui(|_, _| crate::embedded::connect_from_ui())?;
            let inspected_runtime =
                wait_for("fresh read-only owner after explicit reconnect", || {
                    on_ui(|shell, _| Ok(shell.embedded.borrow().clone()))
                })?;
            wait_for("fresh owner settles without scheduling", || {
                let current = inspected_runtime.view();
                if !current.error.is_empty() {
                    return Err(current.error);
                }
                Ok((current.phase == "ready" && !current.task_running).then_some(()))
            })?;
            let current_owner = self::bridge()?;
            let work = inspect_interrupted_work(&current_owner.owner, &current_owner.token)
                .map_err(|e| e.to_string())?;
            ensure(
                work.tasks.is_empty() && work.submissions.is_empty(),
                "unproven original task still has a runnable queue",
            )?;
            resumed_request_observed =
                stats(configuration)?["requests"].as_u64() > server_before["requests"].as_u64();
            break;
        }
        let Some(journal) = recovery_inspection(
            &runtime,
            &bridge,
            inspect_operations(&bridge.owner, &bridge.token).map_err(|error| {
                let retired = matches!(
                    error,
                    concat_agent::operations::JournalError::Owner(concat_agent::Error::Revoked)
                );
                (error.to_string(), retired)
            }),
        )?
        else {
            continue;
        };
        if lock_probe && !locked_refusal_observed && journal.not_applied > 0 {
            let current_review = review()?;
            let refused = current_review
                .operations
                .iter()
                .find(|operation| {
                    operation.invocation.call_id == "recovery-new-plan-locked"
                        && operation.invocation.conversation_id
                            == original.operations[0].invocation.conversation_id
                })
                .ok_or("zero-change result does not belong to the resumed locked plan")?;
            let expected_error = concat_api::ApiError::new(
                concat_api::ErrorCode::Refused,
                concat_project::guard::GuardError::LockedTracks {
                    track_ids: vec![track.clone()],
                }
                .to_string(),
            );
            let expected_receipt = json!({"nativeOutcome":"not-applied", "error":expected_error});
            ensure(
                refused.current_project
                    && refused.session
                        == before_view["sessionId"]
                            .as_u64()
                            .ok_or("current native session missing")?
                    && refused.expected_revision
                        == before_view["revision"]
                            .as_u64()
                            .ok_or("current native revision missing")?
                    && refused.state == concat_agent::operations::RecoveryState::NotApplied
                    && refused.receipt.as_ref() == Some(&expected_receipt),
                "resumed refusal was not the exact current native locked-track rejection",
            )?;
            let rejected_view = ui_view()?;
            ensure(
                Some(&rejected_view) == locked_before.as_ref(),
                "locked resumed plan changed native contents, revision, undo or redo state",
            )?;
            locked_refusal_observed = true;
            super::lock_track(track.clone(), false)?;
            write_plan(configuration, &ui_view()?)?;
            write_json(
                &configuration.case.root.join("lock-rechecked.json"),
                &json!({"refusedWithoutChange":true,
                "unlockedForFreshPlan":true,"counts":journal,"exactRefusal":refused,
                "beforeView":locked_before,"afterRefusalView":rejected_view}),
                true,
            )?;
        }
        if let Some(id) = runtime.tool_permission_view()?.pending_id {
            if approved.insert(id.clone()) {
                snapshot(
                    configuration,
                    &format!("recovery-approval-{}.png", approved.len()),
                )?;
                on_ui(move |_, app| {
                    app.global::<crate::ui::AgentUi>()
                        .invoke_tool_decision_requested(id.into(), true);
                    Ok(())
                })?;
            }
        }
        let requests = stats(configuration)?["requests"]
            .as_u64()
            .ok_or("fixture request counter absent")?;
        let old_requests = server_before["requests"]
            .as_u64()
            .ok_or("prior fixture request counter absent")?;
        resumed_request_observed |= requests > old_requests;
        let Some(work) = recovery_inspection(
            &runtime,
            &bridge,
            inspect_interrupted_work(&bridge.owner, &bridge.token).map_err(|error| {
                let retired = matches!(
                    error,
                    concat_agent::sqlite::SqliteError::Ownership(concat_agent::Error::Revoked)
                );
                (error.to_string(), retired)
            }),
        )?
        else {
            continue;
        };
        if view.phase == "ready"
            && !view.task_running
            && work.tasks.is_empty()
            && work.submissions.is_empty()
        {
            break;
        }
        ensure(
            began.elapsed() < Duration::from_secs(100),
            "explicit recovery did not settle through the original runtime",
        )?;
        std::thread::sleep(Duration::from_millis(30));
    }
    let after = ui_view()?;
    let after_review = review()?;
    let original_operation_after = after_review
        .operations
        .iter()
        .find(|operation| operation.operation_id == original.operations[0].operation_id)
        .ok_or("explicit recovery lost the original operation")?;
    ensure(
        original_operation_after.reviewed,
        "explicit recovery review was not durably recorded",
    )?;
    let mut original_after =
        serde_json::to_value(original_operation_after).map_err(|e| e.to_string())?;
    let mut original_before = checkpoint["operations"][0].clone();
    original_after
        .as_object_mut()
        .ok_or("invalid original operation evidence")?
        .remove("reviewed");
    original_before
        .as_object_mut()
        .ok_or("invalid original checkpoint evidence")?
        .remove("reviewed");
    ensure(
        original_after == original_before,
        "explicit recovery rewrote original operation facts instead of only recording review",
    )?;
    if configuration.case.decision == "discard" {
        ensure(
            !resumed_request_observed
                && stats(configuration)?["requests"] == server_before["requests"],
            "discard contacted the controlled model",
        )?;
        ensure(
            after == before_view && disk_document(configuration)?.0 == before_bytes,
            "discard changed the actual project",
        )?;
        ensure(
            after_review.operations.len() == 1,
            "discard dispatched another native edit",
        )?;
    } else {
        ensure(
            resumed_request_observed,
            "Continue did not execute the original resumed task",
        )?;
        ensure(
            super::clips(&after)?.len() == 1,
            "Continue duplicated or lost the required actual clip",
        )?;
        if lock_probe {
            ensure(
                locked_refusal_observed && approved.len() == 2,
                "resumed edit did not recheck locks and separately approve a fresh plan",
            )?;
            ensure(
                after_review.operations.len() == 3,
                "expected original attempt, zero-change locked refusal and one new plan",
            )?;
        } else {
            ensure(
                after_review.operations.len() == 1,
                "saved content was applied a second time",
            )?;
        }
        if lock_probe {
            // Explicitly save the newly reviewed edit; wait for the actual
            // matched journal proof as well as the independently read bytes.
            on_ui(|shell, _| {
                shell.studio.borrow_mut().save(false);
                Ok(())
            })?;
            wait_for("recovered current project save", || {
                let (_, document) = disk_document(configuration)?;
                let saved = review()?.operations.last().is_some_and(|operation| {
                    operation.saved.is_some() && operation.current_file_matches_save
                });
                Ok((document_clips(&document)? == 1 && saved).then_some(()))
            })?;
        }
        if configuration.case.stage == Stage::SaveFirst {
            ensure(
                unproven_notice.is_some()
                    && after_review.operations[0].receipt.is_none()
                    && !after_review.operations[0]
                        .saved_document_candidates
                        .is_empty(),
                "early-save evidence was falsely promoted to a successful original edit",
            )?;
        }
    }
    snapshot(configuration, "recovery-after-choice.png")?;
    let (after_bytes, after_document) = disk_document(configuration)?;
    write_json(
        &configuration.case.root.join("recovery-report.json"),
        &json!({
            "passed":true,"kind":KIND,"runId":configuration.case.run_id,"stage":configuration.case.stage,
            "decision":configuration.case.decision,"oldPid":checkpoint["pid"],"newPid":std::process::id(),
            "oldOwnerGeneration":checkpoint["ownerGeneration"],"newOwnerGeneration":bridge.status()?.generation,
            "beforeSnapshot":snapshot_value,"afterView":after,"afterDiskDocument":after_document,
            "afterDiskSha256":digest(&after_bytes),"operationFacts":review()?.facts(),
            "serverBeforeDecision":server_before,"serverAfterDecision":stats(configuration)?,
            "lockedFreshPlanRefused":locked_refusal_observed,"separateNativeApprovals":approved.len(),
            "unprovenOriginalOperationNotice":unproven_notice,
            "realNativeWindow":true,"realBundledPi":true,"realGuardedCommitAndSave":true,
            "controlledLoopbackModel":true,"realProviderInference":false,"androidDevice":false,
            "nativeUiCallbacks":true,"osMouseInput":false,"osForceTerminationRecordedByParent":true
        }),
        true,
    )?;
    Ok(())
}

// A completion failure retires the owner and publishes its diagnosis in two
// separate operations. An inspection that raced that retirement must return to
// the exact failure-code gate above; it must not turn the expected fifth-state
// refusal into a generic stale-owner failure. This never retries a native write
// or model call, and never reads through an expired owner.
fn recovery_inspection<T>(
    runtime: &crate::embedded::EmbeddedRuntime,
    bridge: &super::super::AgentBridge,
    result: Result<T, (String, bool)>,
) -> Result<Option<T>, String> {
    match result {
        Ok(value) => Ok(Some(value)),
        Err((error, retired)) => {
            // Only the typed owner-revoked error is eligible. SQL corruption,
            // I/O, mismatched identities and other failures remain immediate.
            if !retired {
                return Err(error);
            }
            let began = Instant::now();
            loop {
                if !matches!(
                    bridge.status()?.phase,
                    concat_agent::Phase::Draining | concat_agent::Phase::Released
                ) || began.elapsed() >= Duration::from_secs(5)
                {
                    return Err(error);
                }
                if !runtime.view().error.is_empty() {
                    return Ok(None);
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }
}

fn stats(configuration: &Configuration) -> Result<Value, String> {
    serde_json::from_slice(&ordinary_file(
        &configuration.case.root.join("fixture-stats.json"),
    )?)
    .map_err(|e| e.to_string())
}

fn snapshot(configuration: &Configuration, name: &str) -> Result<(), String> {
    super::window_snapshot(
        &super::Configuration {
            mode: "whole-process-crash".into(),
            dirs: configuration.dirs.clone(),
            project: configuration.case.project.clone(),
            output: configuration.case.root.clone(),
            render_environment: configuration.render_environment.clone(),
        },
        name,
    )
}

fn ordinary_file(path: &Path) -> Result<Vec<u8>, String> {
    let metadata = fs::symlink_metadata(path).map_err(|e| e.to_string())?;
    ensure(
        metadata.is_file() && !metadata.file_type().is_symlink(),
        "acceptance evidence is not an ordinary file",
    )?;
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        ensure(
            metadata.file_attributes() & 0x400 == 0,
            "acceptance evidence is a reparse point",
        )?;
    }
    fs::read(path).map_err(|e| e.to_string())
}

fn ordinary_directory(path: &Path) -> Result<(), String> {
    let metadata = fs::symlink_metadata(path).map_err(|e| e.to_string())?;
    ensure(
        metadata.is_dir() && !metadata.file_type().is_symlink(),
        "acceptance directory is not an ordinary directory",
    )?;
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        ensure(
            metadata.file_attributes() & 0x400 == 0,
            "acceptance directory is a reparse point",
        )?;
    }
    Ok(())
}

fn write_json(path: &Path, value: &Value, fresh: bool) -> Result<(), String> {
    let bytes = serde_json::to_vec_pretty(value).map_err(|e| e.to_string())?;
    let temporary = path.with_extension(format!("pending-{}", std::process::id()));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .map_err(|e| e.to_string())?;
    file.write_all(&bytes)
        .and_then(|_| file.sync_all())
        .map_err(|e| e.to_string())?;
    drop(file);
    if fresh {
        // Publish only the already-flushed bytes. create_new/hard_link cannot
        // replace a previous run, and the parent never sees a partial barrier.
        fs::hard_link(&temporary, path).map_err(|e| e.to_string())?;
        fs::remove_file(temporary).map_err(|e| e.to_string())
    } else {
        // These mutable fixture/evidence files belong only to this new case.
        fs::rename(&temporary, path).map_err(|e| e.to_string())
    }
}

fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
