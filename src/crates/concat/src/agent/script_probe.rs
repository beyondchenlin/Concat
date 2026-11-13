// SPDX-License-Identifier: AGPL-3.0-or-later
//! Feature-gated checks through the actual native document callbacks and project
//! lifecycle. Each run creates new portable data and two empty projects. No
//! provider, model request, webpage runtime, user media or external service is
//! used. Timer callbacks poll real asynchronous results without sleeping on or
//! blocking the native event loop. This proves project reopen, not app restart.

use std::cell::RefCell;
use std::fs;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::{OnceLock, mpsc};
use std::time::{Duration, Instant};

use concat_agent::scripts::ScriptStore;
use concat_api::AppDirs;
use concat_host::{ProjectInfo, projects};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use slint::{ComponentHandle, Model};

use crate::host::Shell;
use crate::ui::{AgentUi, AgentView, App, ScriptUi};

const FIRST: &str = "第一版：中文标点，保留原文！\r\n\r\n第二段……两个  空格。\n";
const SECOND: &str = "第二版：已提交保存的手稿。\n这一版不改变已选定的第一版。\n";
const LATER_DRAFT: &str = "第三版：保存期间继续输入。\n\n保留草稿并读取后，再保存为新版本。\n";

#[derive(Clone)]
struct Configuration {
    dirs: AppDirs,
    output: PathBuf,
    first: ProjectInfo,
    second: ProjectInfo,
}

static CONFIGURATION: OnceLock<Configuration> = OnceLock::new();

/// Run the isolated real-window document scenario through the existing example.
pub fn run() -> Result<(), String> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    ensure(
        args == ["--mode", "script"],
        "usage: bk-cat-native-probe --mode script",
    )?;
    let executable = std::env::current_exe().map_err(|error| error.to_string())?;
    let output = executable
        .parent()
        .ok_or("probe executable has no parent")?
        .to_path_buf();
    let portable = output.join("portable");
    // Refuse even an existing empty directory. There is no adoption or cleanup
    // of a directory that did not belong to this exact verification attempt.
    fs::create_dir(&portable)
        .map_err(|error| format!("probe requires a new portable directory: {error}"))?;
    let dirs = AppDirs::under(&portable);
    let located = AppDirs::locate()?;
    ensure(
        located.config == dirs.config && located.data == dirs.data,
        "portable isolation was not selected",
    )?;
    let project_root = portable.join("projects");
    let project_root = project_root.to_str().ok_or("non-Unicode probe path")?;
    let first = projects::create(
        project_root,
        "BK CAT script integration A",
        640,
        360,
        24,
        1,
        [0, 0, 0, 255],
    )?;
    let second = projects::create(
        project_root,
        "BK CAT script integration B",
        640,
        360,
        24,
        1,
        [0, 0, 0, 255],
    )?;
    CONFIGURATION
        .set(Configuration {
            dirs,
            output,
            first: first.clone(),
            second,
        })
        .map_err(|_| "document probe already configured")?;
    crate::draw_in_software();
    crate::open_logging(None);
    crate::run_with_project(Some(PathBuf::from(first.path))).map_err(|error| error.to_string())
}

/// Start before automatic embedded-runtime startup. Returns false for every
/// ordinary application run that has not explicitly configured this probe.
pub(crate) fn start_if_requested() -> bool {
    let Some(configuration) = CONFIGURATION.get().cloned() else {
        return false;
    };
    schedule(Rc::new(RefCell::new(Probe {
        configuration,
        stage: 0,
        started: Instant::now(),
        stage_started: Instant::now(),
        steps: Vec::new(),
        disk: None,
        disk_evidence: None,
    })));
    true
}

struct Probe {
    configuration: Configuration,
    stage: u8,
    started: Instant,
    stage_started: Instant,
    steps: Vec<Value>,
    disk: Option<mpsc::Receiver<Result<Value, String>>>,
    disk_evidence: Option<Value>,
}

fn schedule(probe: Rc<RefCell<Probe>>) {
    slint::Timer::single_shot(Duration::from_millis(30), move || {
        let result = probe.borrow_mut().tick();
        match result {
            Ok(false) => schedule(probe),
            Ok(true) => probe.borrow().finish(Ok(())),
            Err(error) => probe.borrow().finish(Err(error)),
        }
    });
}

impl Probe {
    fn advance(&mut self) {
        self.stage += 1;
        self.stage_started = Instant::now();
    }

    fn step(&mut self, name: &str, evidence: Value) {
        println!("SCRIPT_PROBE_STEP {name} {evidence}");
        self.steps
            .push(json!({"name":name,"passed":true,"evidence":evidence}));
    }

    fn tick(&mut self) -> Result<bool, String> {
        ensure(
            self.started.elapsed() < Duration::from_secs(180),
            "document probe exceeded total deadline",
        )?;
        ensure(
            self.stage_started.elapsed() < Duration::from_secs(45),
            &format!("document stage {} timed out", self.stage),
        )?;
        let mut result = Err("native window was not installed".to_owned());
        Shell::with(|shell, app| {
            result = self.tick_on_window(shell, &app);
        });
        result
    }

    fn tick_on_window(&mut self, shell: &Shell, app: &App) -> Result<bool, String> {
        let ui = app.global::<ScriptUi>();
        match self.stage {
            0 => {
                self.ensure_current(shell, &self.configuration.first.path)?;
                ensure(
                    shell.embedded.borrow().is_none(),
                    "document probe must not start the embedded model runtime",
                )?;
                app.global::<AgentUi>().set_view(AgentView::Script);
                ui.invoke_open_requested();
                self.advance();
            }
            1 => {
                if !ready(app)? {
                    return Ok(false);
                }
                ensure(
                    ui.get_text().is_empty() && ui.get_versions().row_count() == 0,
                    "fresh project inherited a document",
                )?;
                self.step(
                    "fresh_project_opens_real_document_workspace",
                    json!({"rows":0,"modelRequests":false}),
                );
                edit(app, FIRST);
                ensure(
                    ui.get_dirty() && ui.get_can_save(),
                    "actual edited callback did not retain the first draft",
                )?;
                ui.invoke_save_requested();
                ensure(
                    ui.get_busy(),
                    "actual save callback did not enter pending state",
                )?;
                let close_error = shell
                    .studio
                    .borrow_mut()
                    .close_project()
                    .err()
                    .ok_or("pending document save did not block normal close")?;
                let switch_error = shell
                    .studio
                    .borrow_mut()
                    .open_project(self.configuration.second.clone())
                    .err()
                    .ok_or("pending document save did not block project switch")?;
                self.ensure_current(shell, &self.configuration.first.path)?;
                self.step(
                    "pending_save_blocks_native_close_and_switch",
                    json!({"closeError":close_error,"switchError":switch_error}),
                );
                self.advance();
            }
            2 => {
                if !ready(app)? {
                    return Ok(false);
                }
                ensure(
                    !ui.get_dirty() && ui.get_text().as_str() == FIRST,
                    "first save did not preserve exact text",
                )?;
                ensure(
                    ui.get_versions().row_count() == 1,
                    "first save did not publish one immutable version",
                )?;
                ui.invoke_select_requested();
                self.advance();
            }
            3 => {
                if !ready(app)? {
                    return Ok(false);
                }
                ensure(
                    selected_version(app) == Some("1".into()),
                    "explicit first-version selection did not persist in the native view",
                )?;
                self.step(
                    "first_saved_version_is_explicitly_selected",
                    json!({"version":1,"sha256":digest(FIRST)}),
                );
                edit(app, SECOND);
                ui.invoke_save_requested();
                ensure(ui.get_busy(), "second save did not enter pending state")?;
                // This executes in the same UI event turn after submission,
                // before the background save callback can publish its result.
                edit(app, LATER_DRAFT);
                self.advance();
            }
            4 => {
                if !ready(app)? {
                    return Ok(false);
                }
                ensure(
                    ui.get_versions().row_count() == 2,
                    "second save did not publish version two",
                )?;
                ensure(
                    ui.get_text().as_str() == LATER_DRAFT && ui.get_dirty(),
                    "late save callback overwrote newer typing",
                )?;
                ensure(
                    selected_version(app) == Some("1".into()),
                    "saving version two changed the selected cutting input",
                )?;
                let close_error = shell
                    .studio
                    .borrow_mut()
                    .close_project()
                    .err()
                    .ok_or("dirty document did not block normal close")?;
                let switch_error = shell
                    .studio
                    .borrow_mut()
                    .open_project(self.configuration.second.clone())
                    .err()
                    .ok_or("dirty document did not block project switch")?;
                self.ensure_current(shell, &self.configuration.first.path)?;
                ensure(
                    ui.get_text().as_str() == LATER_DRAFT,
                    "refused project transition lost the draft",
                )?;
                self.step(
                    "second_save_preserves_newer_draft_and_first_version_binding",
                    json!({
                        "savedVersion":2,"selectedVersion":1,"unsavedSha256":digest(LATER_DRAFT),
                        "closeError":close_error,"switchError":switch_error
                    }),
                );
                // The same real reload callback now supports preserving a dirty
                // draft while refreshing its saved concurrency snapshot.
                ui.invoke_reload_requested();
                ensure(ui.get_busy(), "preserve-draft reload did not start")?;
                self.advance();
            }
            5 => {
                if !ready(app)? {
                    return Ok(false);
                }
                ensure(
                    ui.get_text().as_str() == LATER_DRAFT && ui.get_dirty(),
                    "reload did not preserve the unsaved draft",
                )?;
                ensure(
                    ui.get_versions().row_count() == 2 && selected_version(app) == Some("1".into()),
                    "reload lost saved history or selection",
                )?;
                ui.invoke_save_requested();
                self.advance();
            }
            6 => {
                if !ready(app)? {
                    return Ok(false);
                }
                ensure(
                    !ui.get_dirty() && ui.get_text().as_str() == LATER_DRAFT,
                    "save after preserve-draft reload did not succeed",
                )?;
                ensure(
                    ui.get_versions().row_count() == 3 && selected_version(app) == Some("1".into()),
                    "third version or stable selection missing",
                )?;
                self.step(
                    "preserved_draft_reload_allows_a_new_saved_version",
                    json!({"latestVersion":3,"selectedVersion":1,"sha256":digest(LATER_DRAFT)}),
                );
                {
                    let mut studio = shell.studio.borrow_mut();
                    studio.close_project()?;
                    ensure(
                        studio.session.is_none(),
                        "clean project did not actually close",
                    )?;
                    studio.open_project(self.configuration.first.clone())?;
                    studio.publish(app, &shell.models);
                }
                app.global::<AgentUi>().set_view(AgentView::Script);
                ui.invoke_open_requested();
                self.advance();
            }
            7 => {
                if !ready(app)? {
                    return Ok(false);
                }
                self.ensure_current(shell, &self.configuration.first.path)?;
                ensure(
                    ui.get_versions().row_count() == 3 && ui.get_text().as_str() == LATER_DRAFT,
                    "reopened project did not restore latest text and all versions",
                )?;
                ensure(
                    selected_version(app) == Some("1".into()),
                    "reopened project changed explicit selection",
                )?;
                for (version, expected) in [("1", FIRST), ("2", SECOND), ("3", LATER_DRAFT)] {
                    ui.invoke_view_requested(version.into());
                    ensure(
                        ui.get_text().as_str() == expected && !ui.get_dirty(),
                        "history callback changed or lost saved text",
                    )?;
                }
                self.step("closed_project_reopens_exact_history_through_native_callbacks", json!({
                    "versions":[1,2,3],"selectedVersion":1,"exactUtf8":true,"applicationRestart":false
                }));
                self.advance();
            }
            8 => {
                // Snapshot only after the reopened view had a complete event
                // turn for layout. The file is written on a background thread.
                let image = app
                    .window()
                    .take_snapshot()
                    .map_err(|error| error.to_string())?;
                ensure(
                    image.width() > 0 && image.height() > 0,
                    "native document snapshot was empty",
                )?;
                let pixels = (image.as_bytes().to_vec(), image.width(), image.height());
                let configuration = self.configuration.clone();
                let (sender, receiver) = mpsc::channel();
                std::thread::Builder::new()
                    .name("bk-cat-document-probe-evidence".into())
                    .spawn(move || {
                        let result = disk_evidence(&configuration, pixels);
                        let _ = sender.send(result);
                    })
                    .map_err(|error| error.to_string())?;
                self.disk = Some(receiver);
                self.advance();
            }
            9 => {
                let receiver = self
                    .disk
                    .as_ref()
                    .ok_or("disk evidence worker was not started")?;
                match receiver.try_recv() {
                    Ok(Ok(evidence)) => {
                        self.step(
                            "real_database_reopen_checks_all_versions_and_bound_input",
                            evidence.clone(),
                        );
                        self.disk_evidence = Some(evidence);
                    }
                    Ok(Err(error)) => return Err(error),
                    Err(mpsc::TryRecvError::Empty) => return Ok(false),
                    Err(mpsc::TryRecvError::Disconnected) => {
                        return Err("disk evidence worker exited without a result".into());
                    }
                }
                {
                    let mut studio = shell.studio.borrow_mut();
                    studio.open_project(self.configuration.second.clone())?;
                    studio.publish(app, &shell.models);
                }
                app.global::<AgentUi>().set_view(AgentView::Script);
                ui.invoke_open_requested();
                self.advance();
            }
            10 => {
                if !ready(app)? {
                    return Ok(false);
                }
                self.ensure_current(shell, &self.configuration.second.path)?;
                ensure(
                    ui.get_versions().row_count() == 0 && ui.get_text().is_empty(),
                    "second project inherited the first project's history",
                )?;
                ensure(
                    selected_version(app).is_none(),
                    "second project inherited a cutting version",
                )?;
                self.step(
                    "another_native_project_has_an_independent_empty_workspace",
                    json!({"rows":0,"selectedVersion":null}),
                );
                {
                    let mut studio = shell.studio.borrow_mut();
                    studio.open_project(self.configuration.first.clone())?;
                    studio.publish(app, &shell.models);
                }
                app.global::<AgentUi>().set_view(AgentView::Script);
                ui.invoke_open_requested();
                self.advance();
            }
            11 => {
                if !ready(app)? {
                    return Ok(false);
                }
                ensure(
                    ui.get_versions().row_count() == 3 && ui.get_text().as_str() == LATER_DRAFT,
                    "returning from another project lost the original document",
                )?;
                ensure(
                    selected_version(app) == Some("1".into()),
                    "returning from another project lost original selection",
                )?;
                shell.studio.borrow_mut().close_project()?;
                ensure(
                    shell.studio.borrow().session.is_none(),
                    "final clean close did not release the project",
                )?;
                self.step(
                    "clean_document_permits_normal_project_close",
                    json!({"closed":true}),
                );
                return Ok(true);
            }
            _ => return Err("invalid document verification stage".into()),
        }
        Ok(false)
    }

    fn ensure_current(&self, shell: &Shell, path: &str) -> Result<(), String> {
        ensure(
            shell
                .studio
                .borrow()
                .session
                .as_ref()
                .is_some_and(|session| session.path() == path),
            "native project changed unexpectedly",
        )
    }

    fn finish(&self, result: Result<(), String>) {
        let report = json!({
            "mode":"script","passed":result.is_ok(),"error":result.as_ref().err(),
            "stage":self.stage,"steps":self.steps,
            "projects":[self.configuration.first.path,self.configuration.second.path],
            "portable":self.configuration.dirs.data,
            "diskEvidence":self.disk_evidence,
            "rendering":{"requested":"software","applicationAdapterDescription":crate::gpu::adapter_description()},
            "scope":{
                "nativeWindow":true,"realScriptUiCallbacks":true,"realProjectLifecycle":true,
                "realSqlitePersistence":true,"projectReopen":true,"applicationRestart":false,
                "physicalKeyboardOrMouse":false,"modelRequests":false,"androidDevice":false,
                "completeProductAcceptance":false
            }
        });
        let path = self.configuration.output.join("script-probe-report.json");
        let passed = result.is_ok();
        if let Err(error) = std::thread::Builder::new()
            .name("bk-cat-document-probe-report".into())
            .spawn(move || {
                let saved = serde_json::to_vec_pretty(&report)
                    .map_err(|error| error.to_string())
                    .and_then(|bytes| fs::write(path, bytes).map_err(|error| error.to_string()));
                println!("SCRIPT_PROBE_RESULT {report}");
                if !passed || saved.is_err() {
                    std::process::exit(1);
                }
                if slint::invoke_from_event_loop(|| {
                    let _ = slint::quit_event_loop();
                })
                .is_err()
                {
                    std::process::exit(1);
                }
            })
        {
            eprintln!("document probe report thread failed: {error}");
            std::process::exit(1);
        }
    }
}

fn ready(app: &App) -> Result<bool, String> {
    let ui = app.global::<ScriptUi>();
    if ui.get_busy() {
        return Ok(false);
    }
    ensure(
        ui.get_error().is_empty(),
        &format!("document operation failed: {}", ui.get_error()),
    )?;
    Ok(ui.get_ready())
}

fn edit(app: &App, text: &str) {
    let ui = app.global::<ScriptUi>();
    ui.set_text(text.into());
    ui.invoke_edited(text.into());
}

fn selected_version(app: &App) -> Option<String> {
    let versions = app.global::<ScriptUi>().get_versions();
    (0..versions.row_count())
        .filter_map(|row| versions.row_data(row))
        .find(|row| row.selected)
        .map(|row| row.identity.to_string())
}

fn disk_evidence(
    configuration: &Configuration,
    pixels: (Vec<u8>, u32, u32),
) -> Result<Value, String> {
    let project_id = fs::canonicalize(&configuration.first.path)
        .map_err(|error| error.to_string())?
        .to_str()
        .ok_or("non-Unicode stable project identity")?
        .to_owned();
    let second_id = fs::canonicalize(&configuration.second.path)
        .map_err(|error| error.to_string())?
        .to_str()
        .ok_or("non-Unicode second project identity")?
        .to_owned();
    let store = ScriptStore::open_read_only(
        &configuration
            .dirs
            .data
            .join("bk-cat")
            .join("script-workspaces"),
    )
    .map_err(|error| error.to_string())?;
    let versions = store
        .versions(&project_id)
        .map_err(|error| error.to_string())?;
    ensure(
        versions.len() == 3,
        "disk history does not contain three versions",
    )?;
    for (index, expected) in [FIRST, SECOND, LATER_DRAFT].into_iter().enumerate() {
        ensure(
            versions[index].version == index as u64 + 1
                && versions[index].content.as_bytes() == expected.as_bytes()
                && versions[index].sha256 == digest(expected),
            "persisted version or exact content digest did not match",
        )?;
    }
    let binding = store
        .bind_selected(&project_id)
        .map_err(|error| error.to_string())?;
    ensure(
        binding.version == 1 && binding.sha256 == digest(FIRST),
        "disk cutting input did not remain the first version",
    )?;
    ensure(
        store
            .resolve_binding(&binding)
            .map_err(|error| error.to_string())?
            == versions[0],
        "bound cutting input did not resolve to the original text",
    )?;
    ensure(
        store
            .load(&second_id)
            .map_err(|error| error.to_string())?
            .is_none(),
        "empty second project unexpectedly owns the first document",
    )?;
    image::save_buffer(
        configuration.output.join("script-workspace.png"),
        &pixels.0,
        pixels.1,
        pixels.2,
        image::ColorType::Rgba8,
    )
    .map_err(|error| error.to_string())?;
    Ok(json!({
        "stableProjectId":project_id,"documentId":binding.document_id,"selectedVersion":binding.version,
        "selectedSha256":binding.sha256,
        "versions":versions.iter().map(|version| json!({"version":version.version,"sha256":version.sha256,"bytes":version.content.len()})).collect::<Vec<_>>(),
        "windowSnapshot":{"file":"script-workspace.png","width":pixels.1,"height":pixels.2,"completeVisualAcceptance":false}
    }))
}

fn ensure(condition: bool, message: &str) -> Result<(), String> {
    if condition {
        Ok(())
    } else {
        Err(message.to_owned())
    }
}

fn digest(content: &str) -> String {
    format!("{:x}", Sha256::digest(content.as_bytes()))
}
