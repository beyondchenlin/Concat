// SPDX-License-Identifier: AGPL-3.0-or-later

//! A project-owned, versioned writing workspace. Its database is separate
//! from Pi's conversations and never contains another editable timeline.

use std::cell::Cell;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use concat_agent::scripts::{
    LegacyScriptInfo, ScriptDraft, ScriptError, ScriptSnapshot, ScriptStore, ScriptVersion,
};
use concat_host::{project_identity, projects};
use slint::{ComponentHandle, ModelRc, VecModel};

use crate::host::{Shell, spawn_in_project};
use crate::studio::Studio;
use crate::ui::{App, ScriptUi, ScriptVersionData};

/// The native editor's draft and version view for one open project.
#[derive(Default)]
pub struct ScriptPane {
    project_id: Option<String>,
    project_path: Option<PathBuf>,
    root: Option<PathBuf>,
    snapshot: Option<ScriptSnapshot>,
    versions: Vec<ScriptVersion>,
    legacy_documents: Vec<LegacyScriptInfo>,
    legacy: Option<ScriptSnapshot>,
    text: String,
    baseline: String,
    viewed: Option<u64>,
    ready: bool,
    busy: bool,
    pending_save: bool,
    local_io: Arc<Mutex<LocalIoGate>>,
    error: String,
    published_rows: Cell<Option<HistoryRevision>>,
}

/// Closing this gate also refuses work dispatched before termination that has
/// not started yet. The mutex is held only for local file/database operations;
/// callbacks are posted after releasing it and never awaited under it.
#[derive(Default)]
struct LocalIoGate {
    terminating: bool,
}

fn with_local_io<T>(
    gate: &Mutex<LocalIoGate>,
    action: impl FnOnce() -> Result<T, String>,
) -> Result<T, String> {
    let guard = gate
        .lock()
        .map_err(|_| "文案本地操作已中断，无法继续保存。".to_owned())?;
    if guard.terminating {
        return Err("工程正在结束，文案操作已停止。".into());
    }
    let result = action();
    drop(guard);
    result
}

#[derive(Clone, Copy, PartialEq)]
struct HistoryRevision {
    saved: Option<u64>,
    viewed: Option<u64>,
    count: usize,
}

struct Loaded {
    project_id: String,
    project_path: PathBuf,
    root: PathBuf,
    snapshot: Option<ScriptSnapshot>,
    versions: Vec<ScriptVersion>,
    legacy_documents: Vec<LegacyScriptInfo>,
    legacy: Option<ScriptSnapshot>,
}

struct Opened {
    saved: Option<Result<projects::Saved, String>>,
    journal_error: Option<String>,
    loaded: Result<Loaded, String>,
}

/// Capture before dispatch so a legacy identity save keeps the native order
/// and revision. Capturing alone neither writes nor acknowledges a save.
struct NativeSnapshot {
    path: String,
    project_id: String,
    document: serde_json::Value,
    ticket: projects::Ticket,
    authority: projects::SaveAuthority,
    session: u64,
    revision: u64,
}

impl NativeSnapshot {
    fn capture(session: &mut concat_host::Session) -> Result<Self, String> {
        let (path, document) = session.prepare_save(None)?;
        Ok(Self {
            path,
            project_id: session.project_identity().to_owned(),
            document,
            ticket: projects::ticket(),
            authority: session.save_authority(),
            session: session.session_id(),
            revision: session.revision(),
        })
    }
}

fn at_project<T>(
    root: &std::path::Path,
    project_id: &str,
    project_path: &std::path::Path,
    action: impl FnOnce(&ScriptStore) -> Result<T, String>,
) -> Result<T, String> {
    std::fs::create_dir_all(root).map_err(|error| format!("无法打开文案存储位置：{error}"))?;
    concat_agent::script_locations::with_project_location(
        root,
        project_id,
        project_path,
        project_identity::read_persisted,
        || {
            let store = ScriptStore::open(root).map_err(storage_error)?;
            action(&store)
        },
    )
}

impl ScriptPane {
    #[cfg(test)]
    pub(crate) fn disk_draft_for_test(
        root: PathBuf,
        path: String,
        project_id: String,
        text: &str,
    ) -> Self {
        let mut pane = Self::default();
        pane.replace_loaded(load(root, path, project_id).unwrap());
        pane.set_text(text.to_owned());
        pane
    }

    #[cfg(test)]
    pub(crate) fn draft_text_for_test(&self) -> &str {
        &self.text
    }

    fn dirty(&self) -> bool {
        self.text != self.baseline
    }

    /// Project switching and a rejectable close cannot silently drop typed text
    /// or a pending write. The existing explicit quit-without-saving path
    /// remains the only way to abandon a refused close.
    pub fn before_close(&self) -> Result<(), String> {
        if self.busy || self.pending_save {
            return Err("文案操作尚未完成，请稍后再切换或关闭工程。".into());
        }
        if self.dirty() {
            return Err("文案有未保存修改，请先保存新版本或放弃本次修改。".into());
        }
        Ok(())
    }

    /// Preserve the current complete draft when native termination cannot be
    /// refused. This waits only for local operations already inside the gate,
    /// refuses queued operations, and never waits for an event-loop callback.
    /// It does not protect against forced process termination or power loss.
    pub fn finish_on_termination(&mut self) -> Result<(), String> {
        // A save may already have reached disk without its callback. In
        // particular, typing back to the old baseline makes dirty() false
        // while the submitted, different text still has to be superseded.
        let must_preserve = self.dirty() || self.pending_save;
        let gate = Arc::clone(&self.local_io);
        let mut guard = gate
            .lock()
            .map_err(|_| "文案本地操作已中断，无法完成结束保存。".to_owned())?;
        guard.terminating = true;
        if !must_preserve {
            return Ok(());
        }
        let (Some(project_id), Some(root), Some(project_path)) =
            (&self.project_id, &self.root, &self.project_path)
        else {
            return Err("当前文案缺少工程存储位置，结束保存没有完成。".into());
        };
        // Read the real latest snapshot after every admitted local write;
        // the UI snapshot can still be waiting for a save/select callback.
        let saved = at_project(root, project_id, project_path, |store| {
            Ok(match store.load(project_id).map_err(storage_error)? {
                Some(snapshot) if snapshot.latest.content == self.text => snapshot,
                Some(snapshot) => {
                    let mut draft = ScriptDraft::from_snapshot(snapshot);
                    draft.set_text(self.text.clone()).map_err(storage_error)?;
                    store.save(&mut draft).map_err(storage_error)?
                }
                None => store
                    .create(project_id, &self.text)
                    .map_err(storage_error)?,
            })
        })?;
        self.accept_saved(saved);
        // Keep the gate held until the durable result has been accepted.
        drop(guard);
        Ok(())
    }

    fn set_text(&mut self, text: String) {
        if self.ready {
            self.text = text;
            self.error.clear();
        }
    }

    fn replace_loaded(&mut self, loaded: Loaded) {
        self.project_id = Some(loaded.project_id);
        self.project_path = Some(loaded.project_path);
        self.root = Some(loaded.root);
        self.text = loaded
            .snapshot
            .as_ref()
            .map_or_else(String::new, |s| s.latest.content.clone());
        self.baseline = self.text.clone();
        self.viewed = loaded.snapshot.as_ref().map(|s| s.latest.version);
        self.snapshot = loaded.snapshot;
        self.versions = loaded.versions;
        self.legacy_documents = loaded.legacy_documents;
        self.legacy = loaded.legacy;
        self.ready = true;
        self.busy = false;
        self.pending_save = false;
        self.error.clear();
        self.published_rows.set(None);
    }

    fn accept_reloaded(&mut self, loaded: Loaded, draft: Option<String>) {
        self.replace_loaded(loaded);
        if let Some(text) = draft {
            self.text = text;
        }
    }

    /// The editor may have received more typing while the save was on disk.
    /// Publish the saved baseline without replacing that newer draft.
    fn accept_saved(&mut self, snapshot: ScriptSnapshot) {
        self.baseline = snapshot.latest.content.clone();
        self.viewed = Some(snapshot.latest.version);
        if !self
            .versions
            .iter()
            .any(|version| version.version == snapshot.latest.version)
        {
            self.versions.push(snapshot.latest.clone());
        }
        self.snapshot = Some(snapshot);
        self.busy = false;
        self.pending_save = false;
        self.error.clear();
    }

    fn view(&mut self, version: u64) {
        if self.busy || self.dirty() {
            self.error = "请先完成当前操作，并保存或放弃文案修改。".into();
            return;
        }
        let Some(saved) = self.versions.iter().find(|saved| saved.version == version) else {
            self.error = "该文案版本不在当前工程中，请重新读取。".into();
            return;
        };
        self.text = saved.content.clone();
        self.baseline = self.text.clone();
        self.viewed = Some(version);
        self.error.clear();
    }

    fn discard(&mut self) {
        if !self.busy {
            self.text = self.baseline.clone();
            self.error.clear();
        }
    }

    /// Publish the document view without rebuilding its history on each frame.
    pub fn publish(&self, app: &App) {
        let ui = app.global::<ScriptUi>();
        if ui.get_text().as_str() != self.text {
            ui.set_text(self.text.as_str().into());
        }
        let status = if self.busy {
            "正在读取或保存文案".to_owned()
        } else if !self.ready {
            "打开文案后可创建和保存版本".to_owned()
        } else if self.dirty() {
            "文案有未保存修改".to_owned()
        } else if let Some(version) = self.viewed {
            format!("正在查看第{version}版")
        } else {
            "新文案，尚未保存版本".to_owned()
        };
        ui.set_status(status.into());
        ui.set_error(self.error.as_str().into());
        ui.set_ready(self.ready);
        ui.set_busy(self.busy);
        ui.set_dirty(self.dirty());
        ui.set_can_save(self.ready && !self.busy && (self.snapshot.is_none() || self.dirty()));
        ui.set_can_select(self.ready && !self.busy && !self.dirty() && self.viewed.is_some());
        ui.set_has_legacy(!self.legacy_documents.is_empty());
        ui.set_can_copy_legacy(
            self.ready
                && !self.busy
                && !self.dirty()
                && self.snapshot.is_none()
                && self.legacy.is_some(),
        );
        ui.set_legacy_notice(if self.snapshot.is_none() {
            "旧版文案只按旧位置保存，归属尚未确认。请先查看正文，确认后复制；全部历史保留，剪辑版本需重新选择。"
        } else {
            "当前工程已有独立文案。旧位置文案继续保留供核对，不会覆盖当前文案。"
        }.into());
        let legacy_text = self
            .legacy
            .as_ref()
            .map_or("", |legacy| legacy.latest.content.as_str());
        if ui.get_legacy_text().as_str() != legacy_text {
            ui.set_legacy_text(legacy_text.into());
        }
        ui.set_legacy_label(
            self.legacy
                .as_ref()
                .map_or_else(
                    || "请选择旧位置文案查看正文".to_owned(),
                    |legacy| {
                        format!(
                            "{} · 第{}版（仅查看）",
                            legacy.project_id, legacy.latest.version
                        )
                    },
                )
                .into(),
        );
        let selected = self.snapshot.as_ref().and_then(|s| s.selected.as_ref());
        ui.set_selected_label(
            selected
                .map_or_else(
                    || "尚未选择剪辑版本".to_owned(),
                    |binding| format!("剪辑使用第{}版", binding.version),
                )
                .into(),
        );
        let rows_key = HistoryRevision {
            saved: self.snapshot.as_ref().map(|s| s.revision),
            viewed: self.viewed,
            count: self.versions.len(),
        };
        if self.published_rows.get() != Some(rows_key) {
            let rows = self
                .versions
                .iter()
                .rev()
                .map(|version| ScriptVersionData {
                    identity: version.version.to_string().into(),
                    label: format!(
                        "第{}版 · {}字",
                        version.version,
                        version.content.chars().count()
                    )
                    .into(),
                    selected: selected.is_some_and(|binding| binding.version == version.version),
                    viewed: self.viewed == Some(version.version),
                })
                .collect::<Vec<_>>();
            ui.set_versions(ModelRc::new(VecModel::from(rows)));
            ui.set_legacy_documents(ModelRc::new(VecModel::from(
                self.legacy_documents
                    .iter()
                    .map(|legacy| ScriptVersionData {
                        identity: legacy.project_id.as_str().into(),
                        label: format!("{} · {}个版本", legacy.project_id, legacy.latest_version)
                            .into(),
                        selected: false,
                        viewed: false,
                    })
                    .collect::<Vec<_>>(),
            )));
            self.published_rows.set(Some(rows_key));
        }
    }
}

fn storage_error(error: ScriptError) -> String {
    log::warn!("native script workspace: {error}");
    match error {
        ScriptError::Io(_) => "文案文件读取或写入失败，请保留草稿并检查存储位置。",
        ScriptError::Database(_) => "文案存储暂时无法完成操作，请保留草稿后重试。",
        ScriptError::InvalidStorage => "文案存储格式不兼容或内容已损坏，原文件保持不变。",
        ScriptError::InvalidInput => "工程标识或文案内容超出存储限制。",
        ScriptError::NotFound => "当前工程没有已保存文案，请保留草稿并重新读取。",
        ScriptError::AlreadyExists | ScriptError::StaleSnapshot => {
            "文案已被其他操作更新，请保留草稿并重新读取最新版本。"
        }
        ScriptError::WrongVersion => "所选文案版本不属于当前工程，请重新读取。",
        ScriptError::StaleCandidate => "生成期间文案已修改，生成结果保留为候选。",
        ScriptError::NoSelectedVersion => "请先明确选择用于剪辑的文案版本。",
        ScriptError::ReadOnly => "文案存储为只读，请保留草稿并检查写入权限。",
        ScriptError::RevisionExhausted => "文案版本已达到存储上限，请保留当前内容。",
        ScriptError::Poisoned => "文案存储连接已中断，请保留草稿并重新读取。",
    }
    .into()
}

fn load(root: PathBuf, path: String, project_id: String) -> Result<Loaded, String> {
    let project_path = std::fs::canonicalize(path)
        .map_err(|_| "无法核对当前工程位置，文案没有打开。".to_owned())?;
    let legacy_key = project_path
        .to_str()
        .ok_or_else(|| "当前工程路径无法用于文案存储。".to_owned())?;
    at_project(&root, &project_id, &project_path, |store| {
        let snapshot = store.load(&project_id).map_err(storage_error)?;
        let versions = if snapshot.is_some() {
            store.versions(&project_id).map_err(storage_error)?
        } else {
            Vec::new()
        };
        let legacy_documents = store.legacy_documents().map_err(storage_error)?;
        // An exact old path is only an inspection suggestion, never ownership.
        let legacy = store.load(legacy_key).map_err(storage_error)?;
        Ok(Loaded {
            project_id: project_id.clone(),
            project_path: project_path.clone(),
            root: root.clone(),
            snapshot,
            versions,
            legacy_documents,
            legacy,
        })
    })
}

/// Inspect the actual native identity on the worker. An already durable
/// identity needs no native write, save receipt, or editor-save completion.
fn open_with_identity(
    root: PathBuf,
    native: NativeSnapshot,
    record_written: impl FnOnce(&NativeSnapshot) -> Option<String>,
) -> Result<Opened, String> {
    // Resolve aliases for inspection, while keeping the Session's original
    // path in the ordered save and journal evidence. The native save layer
    // remains responsible for write ordering and final identity admission.
    let actual_path = std::fs::canonicalize(&native.path)
        .map_err(|_| "无法核对当前工程位置，文案没有打开。".to_owned())?;
    let existing = project_identity::read_persisted(&actual_path)?;
    if !projects::is_project(&actual_path) {
        return Err("原工程文档已不存在，请先核对原生工程，文案没有打开。".into());
    }
    if let Some(existing) = existing {
        if existing != native.project_id {
            return Err("工程位置已属于另一份工程，文案没有打开。".into());
        }
        return Ok(Opened {
            saved: None,
            journal_error: None,
            loaded: load(root, native.path, native.project_id),
        });
    }

    // Only a legacy manifest without native identity needs its full snapshot
    // persisted before independent script data can be bound to that identity.
    let saved = projects::save_ordered_with_authority(
        &native.path,
        &native.document,
        native.ticket,
        &native.authority,
    );
    let journal_error = if matches!(saved, Ok(projects::Saved::Written)) {
        record_written(&native)
    } else {
        None
    };
    let loaded = match &saved {
        Ok(_) => load(root, native.path, native.project_id),
        Err(error) => Err(format!("工程身份尚未安全保存，文案没有打开：{error}")),
    };
    Ok(Opened {
        saved: Some(saved),
        journal_error,
        loaded,
    })
}

impl Studio {
    fn open_script_workspace(&mut self, reload: bool) {
        if self.script.busy || (!reload && self.script.ready) {
            return;
        }
        self.flush_commit();
        let Some(session) = self.session.as_mut() else {
            self.script.error = "请先打开工程。".into();
            return;
        };
        let native = match NativeSnapshot::capture(session) {
            Ok(native) => native,
            Err(error) => {
                self.script.error = format!("无法保存工程身份，文案没有打开：{error}");
                return;
            }
        };
        // Keep an existing autosave scheduled: this open can be entirely read
        // only. If a legacy snapshot must be written, its ticket and revision
        // protect newer edits and their independently scheduled save.
        let saved_session = native.session;
        let saved_revision = native.revision;
        let agent = self.host.agent.clone();
        let root = self.host.dirs.data.join("bk-cat").join("script-workspaces");
        let draft = self.script.dirty().then(|| self.script.text.clone());
        let was_ready = self.script.ready;
        let gate = Arc::clone(&self.script.local_io);
        self.script.busy = true;
        self.script.ready = false;
        self.script.error.clear();
        spawn_in_project(
            move || {
                with_local_io(&gate, || {
                    open_with_identity(root, native, |native| {
                        agent.as_ref().and_then(|bridge| {
                            bridge
                                .record_saved(
                                    &native.path,
                                    native.session,
                                    native.revision,
                                    &native.document,
                                )
                                .err()
                        })
                    })
                })
            },
            move |studio, _, _, result| {
                let loaded = match result {
                    Ok(opened) => {
                        if let Some(saved) = opened.saved {
                            studio.accept_project_save(
                                saved_session,
                                saved_revision,
                                saved,
                                opened.journal_error,
                                false,
                            );
                        }
                        opened.loaded
                    }
                    Err(error) => Err(error),
                };
                match loaded {
                    Ok(loaded) => studio.script.accept_reloaded(loaded, draft),
                    Err(error) => {
                        studio.script.busy = false;
                        studio.script.ready = was_ready;
                        studio.script.error = error;
                    }
                }
            },
        );
    }

    fn save_script(&mut self) {
        if !self.script.ready || self.script.busy {
            return;
        }
        let (Some(project_id), Some(root), Some(project_path)) = (
            self.script.project_id.clone(),
            self.script.root.clone(),
            self.script.project_path.clone(),
        ) else {
            self.script.error = "请重新打开文案工作区。".into();
            return;
        };
        let snapshot = self.script.snapshot.clone();
        let text = self.script.text.clone();
        let gate = Arc::clone(&self.script.local_io);
        self.script.busy = true;
        self.script.pending_save = true;
        self.script.error.clear();
        spawn_in_project(
            move || {
                with_local_io(&gate, || {
                    at_project(&root, &project_id, &project_path, |store| match snapshot {
                        Some(snapshot) => {
                            let mut draft = ScriptDraft::from_snapshot(snapshot);
                            draft.set_text(text).map_err(storage_error)?;
                            store.save(&mut draft).map_err(storage_error)
                        }
                        None => store.create(&project_id, &text).map_err(storage_error),
                    })
                })
            },
            |studio, _, _, result| match result {
                Ok(loaded) => studio.script.accept_saved(loaded),
                Err(error) => {
                    studio.script.busy = false;
                    studio.script.pending_save = false;
                    studio.script.error = format!("未确认文案保存完成：{error}");
                }
            },
        );
    }

    fn select_script_version(&mut self) {
        if !self.script.ready || self.script.busy || self.script.dirty() {
            self.script.error = "请先保存或放弃文案修改，再选择剪辑版本。".into();
            return;
        }
        let (Some(snapshot), Some(root), Some(version), Some(project_path)) = (
            self.script.snapshot.clone(),
            self.script.root.clone(),
            self.script.viewed,
            self.script.project_path.clone(),
        ) else {
            return;
        };
        let gate = Arc::clone(&self.script.local_io);
        self.script.busy = true;
        self.script.error.clear();
        spawn_in_project(
            move || {
                with_local_io(&gate, || {
                    at_project(&root, &snapshot.project_id, &project_path, |store| {
                        store.select(&snapshot, version).map_err(storage_error)
                    })
                })
            },
            |studio, _, _, result| {
                studio.script.busy = false;
                match result {
                    Ok(snapshot) => {
                        // The selected immutable version changes; newer typing
                        // and the version currently being viewed stay untouched.
                        studio.script.snapshot = Some(snapshot);
                        studio.script.error.clear();
                    }
                    Err(error) => studio.script.error = error,
                }
            },
        );
    }

    fn inspect_legacy_script(&mut self, legacy_id: String) {
        if self.script.busy
            || !self.script.ready
            || !self
                .script
                .legacy_documents
                .iter()
                .any(|item| item.project_id == legacy_id)
        {
            return;
        }
        let (Some(project_id), Some(root), Some(project_path)) = (
            self.script.project_id.clone(),
            self.script.root.clone(),
            self.script.project_path.clone(),
        ) else {
            return;
        };
        let gate = Arc::clone(&self.script.local_io);
        self.script.busy = true;
        self.script.error.clear();
        spawn_in_project(
            move || {
                with_local_io(&gate, || {
                    at_project(&root, &project_id, &project_path, |store| {
                        store
                            .load(&legacy_id)
                            .map_err(storage_error)?
                            .ok_or_else(|| "旧位置文案已经不可读取，原稿未改动。".into())
                    })
                })
            },
            |studio, _, _, result| {
                studio.script.busy = false;
                match result {
                    Ok(legacy) => studio.script.legacy = Some(legacy),
                    Err(error) => studio.script.error = error,
                }
            },
        );
    }

    fn copy_legacy_script(&mut self) {
        if !self.script.ready
            || self.script.busy
            || self.script.dirty()
            || self.script.snapshot.is_some()
        {
            return;
        }
        let (Some(project_id), Some(root), Some(project_path), Some(source)) = (
            self.script.project_id.clone(),
            self.script.root.clone(),
            self.script.project_path.clone(),
            self.script.legacy.clone(),
        ) else {
            return;
        };
        let gate = Arc::clone(&self.script.local_io);
        self.script.busy = true;
        // This explicit copy owns the editor until its callback; no typed draft
        // may disappear when the copied, independently owned history is shown.
        self.script.ready = false;
        self.script.error.clear();
        spawn_in_project(
            move || {
                with_local_io(&gate, || {
                    at_project(&root, &project_id, &project_path, |store| {
                        store
                            .copy_legacy_document(&source, &project_id)
                            .map_err(storage_error)?;
                        Ok(())
                    })?;
                    let path = project_path
                        .to_str()
                        .ok_or("工程位置无法读取。")?
                        .to_owned();
                    load(root, path, project_id)
                })
            },
            |studio, _, _, result| match result {
                Ok(loaded) => studio.script.replace_loaded(loaded),
                Err(error) => {
                    studio.script.busy = false;
                    studio.script.ready = true;
                    studio.script.error =
                        format!("旧稿未确认复制完成，原记录保留；请重新读取核对：{error}");
                }
            },
        );
    }
}

fn with_studio(action: impl FnOnce(&mut Studio)) {
    Shell::with(|shell, app| {
        let mut studio = shell.studio.borrow_mut();
        action(&mut studio);
        studio.script.publish(&app);
    });
}

/// Install real native callbacks. No model, network or agent approval is
/// needed to write, read or choose the user's own document versions.
pub fn install(app: &App) {
    let ui = app.global::<ScriptUi>();
    ui.on_open_requested(|| with_studio(|studio| studio.open_script_workspace(false)));
    ui.on_reload_requested(|| with_studio(|studio| studio.open_script_workspace(true)));
    ui.on_edited(|text| with_studio(|studio| studio.script.set_text(text.to_string())));
    ui.on_save_requested(|| with_studio(Studio::save_script));
    ui.on_select_requested(|| with_studio(Studio::select_script_version));
    ui.on_discard_requested(|| with_studio(|studio| studio.script.discard()));
    ui.on_legacy_view_requested(|identity| {
        with_studio(|studio| studio.inspect_legacy_script(identity.to_string()))
    });
    ui.on_copy_legacy_requested(|| with_studio(Studio::copy_legacy_script));
    ui.on_new_requested(|| {
        with_studio(|studio| {
            if studio.script.ready && !studio.script.busy && !studio.script.dirty() {
                studio.script.text.clear();
                studio.script.error.clear();
            }
        })
    });
    ui.on_view_requested(|version| {
        with_studio(|studio| match version.parse::<u64>() {
            Ok(version) => studio.script.view(version),
            Err(_) => studio.script.error = "文案版本无效。".into(),
        })
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    const PROJECT_ID: &str = "bkcat-project:11111111111111111111111111111111";

    fn snapshot(version: u64, content: &str) -> ScriptSnapshot {
        ScriptSnapshot {
            project_id: PROJECT_ID.into(),
            document_id: "document-a".into(),
            revision: version,
            latest: ScriptVersion {
                version,
                content: content.into(),
                sha256: "fixture".into(),
                candidate_id: None,
            },
            selected: None,
        }
    }

    fn loaded(version: u64, content: &str) -> Loaded {
        let snapshot = snapshot(version, content);
        Loaded {
            project_id: snapshot.project_id.clone(),
            root: PathBuf::from("unused-test-root"),
            project_path: PathBuf::from("unused-native-project"),
            legacy_documents: Vec::new(),
            legacy: None,
            versions: vec![snapshot.latest.clone()],
            snapshot: Some(snapshot),
        }
    }

    #[test]
    fn late_save_preserves_text_typed_after_submission() {
        let mut pane = ScriptPane::default();
        pane.replace_loaded(loaded(1, "第一版\n\n保留换行"));
        pane.set_text("提交保存的第二版".into());
        pane.busy = true;
        pane.set_text("保存期间继续输入的第三稿".into());
        pane.accept_saved(snapshot(2, "提交保存的第二版"));
        assert_eq!(pane.text, "保存期间继续输入的第三稿");
        assert_eq!(pane.baseline, "提交保存的第二版");
        assert!(pane.dirty());
        assert!(pane.before_close().is_err());
        pane.discard();
        assert_eq!(pane.text, "提交保存的第二版");
        assert!(pane.before_close().is_ok());
    }

    #[test]
    fn explicit_reload_preserves_draft_and_adopts_new_saved_base() {
        let mut pane = ScriptPane::default();
        pane.replace_loaded(loaded(1, "第一版"));
        pane.set_text("尚未保存的完整手稿\n\n不丢段落".into());
        let draft = pane.text.clone();
        pane.accept_reloaded(loaded(2, "另一处已保存的第二版"), Some(draft));
        assert_eq!(pane.text, "尚未保存的完整手稿\n\n不丢段落");
        assert_eq!(pane.baseline, "另一处已保存的第二版");
        assert_eq!(pane.snapshot.as_ref().unwrap().latest.version, 2);
        assert!(pane.dirty());
        assert!(pane.before_close().is_err());
    }

    #[test]
    fn browsing_history_cannot_discard_an_unsaved_draft() {
        let mut pane = ScriptPane::default();
        pane.replace_loaded(loaded(2, "最新版本"));
        pane.versions.push(snapshot(1, "历史版本").latest);
        pane.set_text("未保存修改".into());
        pane.view(1);
        assert_eq!(pane.text, "未保存修改");
        assert_eq!(pane.viewed, Some(2));
        pane.discard();
        pane.view(1);
        assert_eq!(pane.text, "历史版本");
        assert!(!pane.dirty());
        assert_eq!(pane.snapshot.as_ref().unwrap().latest.version, 2);
    }

    #[test]
    fn pending_document_write_blocks_normal_project_close() {
        let pane = ScriptPane {
            busy: true,
            ..Default::default()
        };
        assert!(pane.before_close().is_err());
        assert!(ScriptPane::default().before_close().is_ok());
    }

    struct DiskFixture(PathBuf);

    impl DiskFixture {
        fn new() -> Self {
            use std::sync::atomic::{AtomicU64, Ordering};
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let stamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let root = std::env::temp_dir().join(format!(
                "bk-cat-script-termination-{}-{stamp}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir(&root).unwrap();
            std::fs::create_dir(root.join("project")).unwrap();
            std::fs::write(
                root.join("project").join("concat.json"),
                serde_json::to_vec(&serde_json::json!({"bkCatProjectId": PROJECT_ID})).unwrap(),
            )
            .unwrap();
            Self(root)
        }

        fn pane(&self) -> ScriptPane {
            let store = ScriptStore::open(&self.0).unwrap();
            let snapshot = store.load(PROJECT_ID).unwrap();
            let versions = if snapshot.is_some() {
                store.versions(PROJECT_ID).unwrap()
            } else {
                Vec::new()
            };
            let mut pane = ScriptPane::default();
            pane.replace_loaded(Loaded {
                project_id: PROJECT_ID.into(),
                root: self.0.clone(),
                project_path: self.0.join("project"),
                legacy_documents: Vec::new(),
                legacy: None,
                snapshot,
                versions,
            });
            pane
        }

        fn contents(&self) -> Vec<String> {
            ScriptStore::open_read_only(&self.0)
                .unwrap()
                .versions(PROJECT_ID)
                .unwrap()
                .into_iter()
                .map(|version| version.content)
                .collect()
        }
    }

    impl Drop for DiskFixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn append(root: &std::path::Path, snapshot: ScriptSnapshot, text: &str) -> ScriptSnapshot {
        let store = ScriptStore::open(root).unwrap();
        let mut draft = ScriptDraft::from_snapshot(snapshot);
        draft.set_text(text.to_owned()).unwrap();
        store.save(&mut draft).unwrap()
    }

    #[test]
    fn termination_persists_a_first_manual_draft_byte_for_byte() {
        let fixture = DiskFixture::new();
        let mut pane = fixture.pane();
        let text = "第一段中文\r\n\r\n第二段保留换行与 空格。\n";
        pane.set_text(text.into());
        assert!(pane.before_close().is_err());
        pane.finish_on_termination().unwrap();
        assert_eq!(fixture.contents(), [text]);
        assert!(!pane.dirty());
        assert!(pane.snapshot.as_ref().unwrap().selected.is_none());
        assert!(pane.local_io.lock().unwrap().terminating);
    }

    #[test]
    fn termination_appends_once_and_keeps_the_explicitly_selected_older_version() {
        let fixture = DiskFixture::new();
        {
            let store = ScriptStore::open(&fixture.0).unwrap();
            let first = store.create(PROJECT_ID, "用于剪辑的第一版").unwrap();
            store.select(&first, 1).unwrap();
        }
        let mut pane = fixture.pane();
        let selected = pane.snapshot.as_ref().unwrap().selected.clone();
        pane.set_text("尚未点击保存的第二稿\n\n完整正文".into());
        pane.finish_on_termination().unwrap();
        pane.finish_on_termination().unwrap();
        let store = ScriptStore::open_read_only(&fixture.0).unwrap();
        let saved = store.load(PROJECT_ID).unwrap().unwrap();
        assert_eq!(saved.latest.version, 2);
        assert_eq!(saved.latest.content, pane.text);
        assert_eq!(saved.selected, selected);
        assert_eq!(
            store
                .resolve_binding(&saved.selected.unwrap())
                .unwrap()
                .version,
            1
        );
        assert_eq!(store.versions(PROJECT_ID).unwrap().len(), 2);
    }

    #[test]
    fn termination_preserves_typing_back_to_the_old_base_after_commit_without_callback() {
        let fixture = DiskFixture::new();
        let original = ScriptStore::open(&fixture.0)
            .unwrap()
            .create(PROJECT_ID, "旧基线甲")
            .unwrap();
        let mut pane = fixture.pane();
        pane.set_text("已提交的乙稿".into());
        pane.busy = true;
        pane.pending_save = true;
        let gate = Arc::clone(&pane.local_io);
        with_local_io(&gate, || Ok(append(&fixture.0, original, "已提交的乙稿"))).unwrap();
        // The disk commit is done, but the event loop has not accepted it.
        pane.set_text("旧基线甲".into());
        assert!(!pane.dirty());
        assert!(pane.pending_save);
        pane.finish_on_termination().unwrap();
        assert_eq!(fixture.contents(), ["旧基线甲", "已提交的乙稿", "旧基线甲"]);
        assert!(!pane.pending_save);
    }

    #[test]
    fn termination_waits_for_started_local_save_without_waiting_for_its_callback() {
        use std::sync::mpsc::channel;
        use std::time::Duration;

        let fixture = DiskFixture::new();
        let original = ScriptStore::open(&fixture.0)
            .unwrap()
            .create(PROJECT_ID, "第一版")
            .unwrap();
        let mut pane = fixture.pane();
        pane.set_text("正在保存的第二版".into());
        pane.busy = true;
        pane.pending_save = true;
        pane.set_text("保存期间继续手写的第三稿".into());
        let gate = Arc::clone(&pane.local_io);
        let root = fixture.0.clone();
        let (entered_tx, entered_rx) = channel();
        let (release_tx, release_rx) = channel();
        let worker = std::thread::spawn(move || {
            with_local_io(&gate, || {
                entered_tx.send(()).unwrap();
                release_rx.recv().unwrap();
                Ok(append(&root, original, "正在保存的第二版"))
            })
        });
        entered_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        assert!(pane.local_io.try_lock().is_err());
        let (finishing_tx, finishing_rx) = channel();
        let finishing = std::thread::spawn(move || {
            finishing_tx.send(()).unwrap();
            let result = pane.finish_on_termination();
            (pane, result)
        });
        finishing_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        release_tx.send(()).unwrap();
        worker.join().unwrap().unwrap();
        let (pane, result) = finishing.join().unwrap();
        result.unwrap();
        assert_eq!(
            fixture.contents(),
            ["第一版", "正在保存的第二版", "保存期间继续手写的第三稿"]
        );
        assert!(!pane.dirty());
        assert!(!pane.pending_save);
    }

    #[test]
    fn termination_refuses_a_dispatched_old_save_that_has_not_entered_the_gate() {
        use std::sync::mpsc::channel;
        use std::time::Duration;

        let fixture = DiskFixture::new();
        let original = ScriptStore::open(&fixture.0)
            .unwrap()
            .create(PROJECT_ID, "仍要保留的甲稿")
            .unwrap();
        let mut pane = fixture.pane();
        pane.busy = true;
        pane.pending_save = true;
        // The save of B is dispatched, but the current text has returned to A.
        let gate = Arc::clone(&pane.local_io);
        let root = fixture.0.clone();
        let (queued_tx, queued_rx) = channel();
        let (release_tx, release_rx) = channel();
        let worker = std::thread::spawn(move || {
            queued_tx.send(()).unwrap();
            release_rx.recv().unwrap();
            with_local_io(&gate, || Ok(append(&root, original, "已经过时的乙稿")))
        });
        queued_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        assert!(!pane.dirty());
        // Finalization reaches the gate first. Its idempotent content check
        // skips an append, but it must still permanently refuse the old task.
        pane.finish_on_termination().unwrap();
        release_tx.send(()).unwrap();
        assert!(worker.join().unwrap().is_err());
        pane.finish_on_termination().unwrap();
        assert_eq!(fixture.contents(), ["仍要保留的甲稿"]);
    }

    #[test]
    fn termination_does_not_append_an_unedited_history_view() {
        let fixture = DiskFixture::new();
        let first = ScriptStore::open(&fixture.0)
            .unwrap()
            .create(PROJECT_ID, "第一版")
            .unwrap();
        append(&fixture.0, first, "最新的第二版");
        let mut pane = fixture.pane();
        pane.view(1);
        assert!(!pane.dirty());
        assert!(!pane.pending_save);
        pane.finish_on_termination().unwrap();
        assert_eq!(pane.text, "第一版");
        assert_eq!(fixture.contents(), ["第一版", "最新的第二版"]);
    }

    #[test]
    fn termination_reports_a_local_storage_failure_and_keeps_the_full_draft() {
        let fixture = DiskFixture::new();
        let mut pane = fixture.pane();
        let blocked = fixture.0.join("not-a-directory");
        std::fs::write(&blocked, b"existing unrelated file").unwrap();
        pane.root = Some(blocked.clone());
        pane.set_text("发生存储错误仍须完整保留的手稿\n\n第二段".into());
        let text = pane.text.clone();
        assert!(pane.finish_on_termination().is_err());
        assert_eq!(pane.text, text);
        assert!(pane.dirty());
        assert!(pane.local_io.lock().unwrap().terminating);
        assert_eq!(std::fs::read(&blocked).unwrap(), b"existing unrelated file");
    }

    #[test]
    fn legacy_location_text_is_visible_but_is_not_silently_adopted_by_load() {
        let fixture = DiskFixture::new();
        let path = fixture.0.join("project").canonicalize().unwrap();
        let path_text = path.to_str().unwrap().to_owned();
        let store = ScriptStore::open(&fixture.0).unwrap();
        let legacy = store
            .create(&path_text, "待核对归属的旧稿\r\n\r\n中文")
            .unwrap();
        let legacy = store.select(&legacy, 1).unwrap();
        let loaded = load(fixture.0.clone(), path_text.clone(), PROJECT_ID.into()).unwrap();
        assert!(loaded.snapshot.is_none());
        assert!(loaded.versions.is_empty());
        assert_eq!(loaded.legacy.as_ref(), Some(&legacy));
        assert_eq!(loaded.legacy_documents.len(), 1);
        let mut pane = ScriptPane::default();
        pane.replace_loaded(loaded);
        assert!(
            pane.text.is_empty(),
            "inspection must not become the current draft"
        );
        assert!(!pane.dirty());
        assert!(store.load(PROJECT_ID).unwrap().is_none());
        store
            .copy_legacy_document(pane.legacy.as_ref().unwrap(), PROJECT_ID)
            .unwrap();
        pane.replace_loaded(load(fixture.0.clone(), path_text, PROJECT_ID.into()).unwrap());
        assert_eq!(pane.text, legacy.latest.content);
        assert!(pane.snapshot.as_ref().unwrap().selected.is_none());
        assert_eq!(store.load(&legacy.project_id).unwrap(), Some(legacy));
    }

    #[test]
    fn real_workspace_load_follows_a_move_and_refuses_a_live_identity_copy() {
        let fixture = DiskFixture::new();
        let original = fixture.0.join("project");
        ScriptStore::open(&fixture.0)
            .unwrap()
            .create(PROJECT_ID, "属于持久身份的文案")
            .unwrap();
        load(
            fixture.0.clone(),
            original.to_str().unwrap().into(),
            PROJECT_ID.into(),
        )
        .unwrap();
        let moved = fixture.0.join("moved");
        std::fs::rename(&original, &moved).unwrap();
        let moved_view = load(
            fixture.0.clone(),
            moved.to_str().unwrap().into(),
            PROJECT_ID.into(),
        )
        .unwrap();
        assert_eq!(
            moved_view.snapshot.unwrap().latest.content,
            "属于持久身份的文案"
        );
        std::fs::create_dir(&original).unwrap();
        std::fs::copy(moved.join("concat.json"), original.join("concat.json")).unwrap();
        assert!(
            load(
                fixture.0.clone(),
                original.to_str().unwrap().into(),
                PROJECT_ID.into()
            )
            .is_err()
        );
        let replacement = "bkcat-project:22222222222222222222222222222222";
        std::fs::write(
            original.join("concat.json"),
            serde_json::to_vec(&serde_json::json!({"bkCatProjectId":replacement})).unwrap(),
        )
        .unwrap();
        let replacement_view = load(
            fixture.0.clone(),
            original.to_str().unwrap().into(),
            replacement.into(),
        )
        .unwrap();
        assert!(replacement_view.snapshot.is_none());
        assert!(replacement_view.legacy.is_none());
        assert!(replacement_view.versions.is_empty());
    }

    #[test]
    fn termination_refuses_an_identity_replaced_after_open_without_losing_the_draft() {
        let fixture = DiskFixture::new();
        let mut pane = fixture.pane();
        pane.set_text("必须保留的未保存手稿".into());
        std::fs::write(fixture.0.join("project").join("concat.json"),
            serde_json::to_vec(&serde_json::json!({"bkCatProjectId":"bkcat-project:22222222222222222222222222222222"})).unwrap()).unwrap();
        assert!(pane.finish_on_termination().is_err());
        assert_eq!(pane.text, "必须保留的未保存手稿");
        assert!(pane.dirty());
        assert!(
            ScriptStore::open(&fixture.0)
                .unwrap()
                .load(PROJECT_ID)
                .unwrap()
                .is_none()
        );
    }

    fn native_project(
        fixture: &DiskFixture,
        legacy: bool,
    ) -> (concat_host::ProjectInfo, concat_host::Session) {
        let info = projects::create(
            fixture.0.to_str().unwrap(),
            "Native project",
            64,
            64,
            30,
            1,
            [0, 0, 0, 255],
        )
        .unwrap();
        if legacy {
            let mut document = projects::read_document(&info.path).unwrap().unwrap();
            document
                .as_object_mut()
                .unwrap()
                .remove(project_identity::FIELD);
            std::fs::write(
                projects::manifest_path(std::path::Path::new(&info.path)),
                serde_json::to_vec_pretty(&document).unwrap(),
            )
            .unwrap();
        }
        let session = concat_host::Session::open_info(&info).unwrap();
        (info, session)
    }

    #[test]
    fn opening_a_durable_identity_neither_saves_edits_nor_consumes_a_save_receipt() {
        let fixture = DiskFixture::new();
        let (info, mut session) = native_project(&fixture, false);
        let root = fixture.0.join("scripts");
        let store = ScriptStore::open(&root).unwrap();
        let script = store
            .create(session.project_identity(), "已经保存的文案")
            .unwrap();
        let script = store.select(&script, 1).unwrap();
        let manifest = projects::manifest_path(std::path::Path::new(&info.path));
        let before = std::fs::read(&manifest).unwrap();
        session
            .apply(concat_project::Command::AddTrack { bottom: false })
            .unwrap();
        let earlier_ticket = projects::ticket();
        let native = NativeSnapshot::capture(&mut session).unwrap();
        let pending_document = native.document.clone();
        let receipts = Cell::new(0);
        let opened = open_with_identity(root, native, |_| {
            receipts.set(receipts.get() + 1);
            None
        })
        .unwrap();

        assert!(
            opened.saved.is_none(),
            "reading is not a native save result"
        );
        assert!(opened.journal_error.is_none());
        assert_eq!(receipts.get(), 0);
        assert_eq!(std::fs::read(&manifest).unwrap(), before);
        assert_eq!(opened.loaded.unwrap().snapshot, Some(script));
        assert_eq!(session.document(), pending_document);
        // A write with the earlier ticket must still be admitted. Equal file
        // bytes alone would not detect an unwanted save of an equal snapshot.
        assert_eq!(
            projects::save_ordered(&info.path, &pending_document, earlier_ticket).unwrap(),
            projects::Saved::Written
        );
    }

    #[test]
    fn a_legacy_identity_is_persisted_before_binding_and_reports_the_real_snapshot() {
        let fixture = DiskFixture::new();
        let (info, mut session) = native_project(&fixture, true);
        let root = fixture.0.join("scripts");
        let native = NativeSnapshot::capture(&mut session).unwrap();
        let expected = native.document.clone();
        let saved_session = native.session;
        let saved_revision = native.revision;
        let identity = session.project_identity().to_owned();
        assert!(
            project_identity::read_persisted(std::path::Path::new(&info.path))
                .unwrap()
                .is_none()
        );
        // A newer edit stays outside the captured native save. The UI uses
        // this same session/revision check before clearing its unsaved flag.
        session
            .apply(concat_project::Command::AddTrack { bottom: false })
            .unwrap();
        let receipts = Cell::new(0);
        let opened = open_with_identity(root.clone(), native, |saved| {
            receipts.set(receipts.get() + 1);
            assert_eq!(saved.path, info.path);
            assert_eq!(saved.session, saved_session);
            assert_eq!(saved.revision, saved_revision);
            assert_eq!(saved.document, expected);
            assert_eq!(
                projects::read_document(&saved.path).unwrap(),
                Some(expected.clone())
            );
            assert_eq!(
                project_identity::read_persisted(std::path::Path::new(&saved.path)).unwrap(),
                Some(identity.clone())
            );
            assert!(
                !root.join("project-locations").exists(),
                "native persistence precedes independent identity registration"
            );
            None
        })
        .unwrap();
        assert_eq!(opened.saved, Some(Ok(projects::Saved::Written)));
        assert_eq!(receipts.get(), 1);
        assert!(opened.journal_error.is_none());
        let loaded = opened.loaded.unwrap();
        assert_eq!(loaded.project_id, identity);
        assert!(loaded.snapshot.is_none());
        assert!(root.join("project-locations").is_dir());
        assert!(
            session
                .check_revision(saved_session, saved_revision)
                .is_err()
        );
        assert_ne!(session.document(), expected);

        let pane = ScriptPane::disk_draft_for_test(
            root,
            info.path,
            identity,
            "下一步需要保留的手稿\n第二段",
        );
        assert_eq!(pane.draft_text_for_test(), "下一步需要保留的手稿\n第二段");
        assert!(pane.before_close().is_err());
    }

    #[test]
    fn a_superseded_legacy_snapshot_cannot_bind_an_identity_absent_from_disk() {
        let fixture = DiskFixture::new();
        let (info, mut session) = native_project(&fixture, true);
        let legacy = projects::read_document(&info.path).unwrap().unwrap();
        let native = NativeSnapshot::capture(&mut session).unwrap();
        let root = fixture.0.join("scripts");
        assert_eq!(
            projects::save_ordered(&info.path, &legacy, projects::ticket()).unwrap(),
            projects::Saved::Written
        );
        let receipts = Cell::new(0);
        let opened = open_with_identity(root.clone(), native, |_| {
            receipts.set(receipts.get() + 1);
            None
        })
        .unwrap();
        assert_eq!(opened.saved, Some(Ok(projects::Saved::Superseded)));
        assert!(opened.loaded.is_err());
        assert_eq!(receipts.get(), 0);
        assert_eq!(projects::read_document(&info.path).unwrap(), Some(legacy));
        assert!(!root.join("project-locations").exists());
    }

    #[test]
    fn foreign_or_damaged_native_identity_is_rejected_without_writes_or_receipts() {
        let fixture = DiskFixture::new();
        let (info, mut session) = native_project(&fixture, false);
        let root = fixture.0.join("scripts");
        let manifest = projects::manifest_path(std::path::Path::new(&info.path));
        for identity in [
            serde_json::json!("bkcat-project:22222222222222222222222222222222"),
            serde_json::Value::Null,
        ] {
            let native = NativeSnapshot::capture(&mut session).unwrap();
            let mut replaced = session.document();
            replaced[project_identity::FIELD] = identity;
            let bytes = serde_json::to_vec_pretty(&replaced).unwrap();
            std::fs::write(&manifest, &bytes).unwrap();
            let receipts = Cell::new(0);
            assert!(
                open_with_identity(root.clone(), native, |_| {
                    receipts.set(receipts.get() + 1);
                    None
                })
                .is_err()
            );
            assert_eq!(receipts.get(), 0);
            assert_eq!(std::fs::read(&manifest).unwrap(), bytes);
            assert!(!root.exists());
        }
    }

    #[test]
    fn an_existing_identity_cannot_be_reinitialized_after_disk_metadata_is_removed() {
        let fixture = DiskFixture::new();
        let (info, mut session) = native_project(&fixture, false);
        let root = fixture.0.join("scripts");
        let native = NativeSnapshot::capture(&mut session).unwrap();
        let mut downgraded = session.document();
        downgraded
            .as_object_mut()
            .unwrap()
            .remove(project_identity::FIELD);
        let bytes = serde_json::to_vec_pretty(&downgraded).unwrap();
        let manifest = projects::manifest_path(std::path::Path::new(&info.path));
        std::fs::write(&manifest, &bytes).unwrap();
        let receipts = Cell::new(0);
        let opened = open_with_identity(root.clone(), native, |_| {
            receipts.set(receipts.get() + 1);
            None
        })
        .unwrap();
        assert!(matches!(opened.saved, Some(Err(_))));
        assert!(opened.loaded.is_err());
        assert_eq!(receipts.get(), 0);
        assert_eq!(std::fs::read(&manifest).unwrap(), bytes);
        assert!(!root.exists());
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn native_alias_inspection_preserves_the_original_save_and_receipt_path() {
        let fixture = DiskFixture::new();
        let (mut info, original) = native_project(&fixture, true);
        drop(original);
        let actual_path = std::path::Path::new(&info.path).canonicalize().unwrap();
        let alias = fixture.0.join("native-alias");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&actual_path, &alias).unwrap();
        #[cfg(windows)]
        {
            let result = std::process::Command::new("cmd")
                .args(["/C", "mklink", "/J"])
                .arg(&alias)
                .arg(&actual_path)
                .output()
                .unwrap();
            assert!(result.status.success(), "{:?}", result.status);
        }
        info.path = alias.to_str().unwrap().to_owned();
        let mut session = concat_host::Session::open_info(&info).unwrap();
        let native = NativeSnapshot::capture(&mut session).unwrap();
        let identity = native.project_id.clone();
        let expected = native.document.clone();
        let receipts = Cell::new(0);
        let opened = open_with_identity(fixture.0.join("scripts"), native, |saved| {
            receipts.set(receipts.get() + 1);
            assert_eq!(saved.path, info.path);
            assert_eq!(
                projects::read_document(&saved.path).unwrap(),
                Some(expected.clone())
            );
            None
        })
        .unwrap();
        assert_eq!(opened.saved, Some(Ok(projects::Saved::Written)));
        assert_eq!(receipts.get(), 1);
        assert_eq!(opened.loaded.unwrap().project_path, actual_path);
        assert_eq!(
            project_identity::read_persisted(&actual_path).unwrap(),
            Some(identity)
        );
        // Opening the already durable identity through the same alias now
        // becomes a read and produces no second native receipt.
        let opened = open_with_identity(
            fixture.0.join("scripts"),
            NativeSnapshot::capture(&mut session).unwrap(),
            |_| panic!("the already durable alias must not save again"),
        )
        .unwrap();
        assert!(opened.saved.is_none());
        assert!(opened.loaded.is_ok());
        #[cfg(windows)]
        std::fs::remove_dir(alias).unwrap();
        #[cfg(unix)]
        std::fs::remove_file(alias).unwrap();
    }
}
