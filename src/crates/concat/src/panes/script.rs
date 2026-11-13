// SPDX-License-Identifier: AGPL-3.0-or-later

//! A project-owned, versioned writing workspace. Its database is separate
//! from Pi's conversations and never contains another editable timeline.

use std::cell::Cell;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use concat_agent::scripts::{ScriptDraft, ScriptError, ScriptSnapshot, ScriptStore, ScriptVersion};
use slint::{ComponentHandle, ModelRc, VecModel};

use crate::host::{Shell, spawn_in_project};
use crate::studio::Studio;
use crate::ui::{App, ScriptUi, ScriptVersionData};

/// The native editor's draft and version view for one open project.
#[derive(Default)]
pub struct ScriptPane {
    project_id: Option<String>,
    root: Option<PathBuf>,
    snapshot: Option<ScriptSnapshot>,
    versions: Vec<ScriptVersion>,
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
    root: PathBuf,
    snapshot: Option<ScriptSnapshot>,
    versions: Vec<ScriptVersion>,
}

impl ScriptPane {
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
        let (Some(project_id), Some(root)) = (&self.project_id, &self.root) else {
            return Err("当前文案缺少工程存储位置，结束保存没有完成。".into());
        };
        let store = ScriptStore::open(root).map_err(storage_error)?;
        // Read the real latest snapshot after every admitted local write;
        // the UI snapshot can still be waiting for a save/select callback.
        let saved = match store.load(project_id).map_err(storage_error)? {
            Some(snapshot) if snapshot.latest.content == self.text => snapshot,
            Some(snapshot) => {
                let mut draft = ScriptDraft::from_snapshot(snapshot);
                draft.set_text(self.text.clone()).map_err(storage_error)?;
                store.save(&mut draft).map_err(storage_error)?
            }
            None => store
                .create(project_id, &self.text)
                .map_err(storage_error)?,
        };
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
        self.root = Some(loaded.root);
        self.text = loaded
            .snapshot
            .as_ref()
            .map_or_else(String::new, |s| s.latest.content.clone());
        self.baseline = self.text.clone();
        self.viewed = loaded.snapshot.as_ref().map(|s| s.latest.version);
        self.snapshot = loaded.snapshot;
        self.versions = loaded.versions;
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

fn load(root: PathBuf, path: String) -> Result<Loaded, String> {
    let canonical = std::fs::canonicalize(path)
        .map_err(|_| "无法核对当前工程位置，文案没有打开。".to_owned())?;
    let project_id = canonical
        .to_str()
        .ok_or_else(|| "当前工程路径无法用于文案存储。".to_owned())?
        .to_owned();
    let store = ScriptStore::open(&root).map_err(storage_error)?;
    let snapshot = store.load(&project_id).map_err(storage_error)?;
    let versions = if snapshot.is_some() {
        store.versions(&project_id).map_err(storage_error)?
    } else {
        Vec::new()
    };
    Ok(Loaded {
        project_id,
        root,
        snapshot,
        versions,
    })
}

impl Studio {
    fn open_script_workspace(&mut self, reload: bool) {
        if self.script.busy || (!reload && self.script.ready) {
            return;
        }
        let Some(session) = &self.session else {
            self.script.error = "请先打开工程。".into();
            return;
        };
        let path = session.path().to_owned();
        let root = self.host.dirs.data.join("bk-cat").join("script-workspaces");
        let draft = self.script.dirty().then(|| self.script.text.clone());
        let was_ready = self.script.ready;
        let gate = Arc::clone(&self.script.local_io);
        self.script.busy = true;
        // A reload temporarily disables editing, so its callback cannot
        // overwrite a draft typed while the old snapshot was being read.
        self.script.ready = false;
        self.script.error.clear();
        spawn_in_project(
            move || with_local_io(&gate, || load(root, path)),
            move |studio, _, _, result| match result {
                Ok(loaded) => studio.script.accept_reloaded(loaded, draft),
                Err(error) => {
                    studio.script.busy = false;
                    studio.script.ready = was_ready;
                    studio.script.error = error;
                }
            },
        );
    }

    fn save_script(&mut self) {
        if !self.script.ready || self.script.busy {
            return;
        }
        let (Some(project_id), Some(root)) =
            (self.script.project_id.clone(), self.script.root.clone())
        else {
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
                    let store = ScriptStore::open(&root).map_err(storage_error)?;
                    match snapshot {
                        Some(snapshot) => {
                            let mut draft = ScriptDraft::from_snapshot(snapshot);
                            draft.set_text(text).map_err(storage_error)?;
                            store.save(&mut draft).map_err(storage_error)
                        }
                        None => store.create(&project_id, &text).map_err(storage_error),
                    }
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
        let (Some(snapshot), Some(root), Some(version)) = (
            self.script.snapshot.clone(),
            self.script.root.clone(),
            self.script.viewed,
        ) else {
            return;
        };
        let gate = Arc::clone(&self.script.local_io);
        self.script.busy = true;
        self.script.error.clear();
        spawn_in_project(
            move || {
                with_local_io(&gate, || {
                    let store = ScriptStore::open(&root).map_err(storage_error)?;
                    store.select(&snapshot, version).map_err(storage_error)
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

    fn snapshot(version: u64, content: &str) -> ScriptSnapshot {
        ScriptSnapshot {
            project_id: "project-a".into(),
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
            Self(root)
        }

        fn pane(&self) -> ScriptPane {
            let store = ScriptStore::open(&self.0).unwrap();
            let snapshot = store.load("project-a").unwrap();
            let versions = if snapshot.is_some() {
                store.versions("project-a").unwrap()
            } else {
                Vec::new()
            };
            let mut pane = ScriptPane::default();
            pane.replace_loaded(Loaded {
                project_id: "project-a".into(),
                root: self.0.clone(),
                snapshot,
                versions,
            });
            pane
        }

        fn contents(&self) -> Vec<String> {
            ScriptStore::open_read_only(&self.0)
                .unwrap()
                .versions("project-a")
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
            let first = store.create("project-a", "用于剪辑的第一版").unwrap();
            store.select(&first, 1).unwrap();
        }
        let mut pane = fixture.pane();
        let selected = pane.snapshot.as_ref().unwrap().selected.clone();
        pane.set_text("尚未点击保存的第二稿\n\n完整正文".into());
        pane.finish_on_termination().unwrap();
        pane.finish_on_termination().unwrap();
        let store = ScriptStore::open_read_only(&fixture.0).unwrap();
        let saved = store.load("project-a").unwrap().unwrap();
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
        assert_eq!(store.versions("project-a").unwrap().len(), 2);
    }

    #[test]
    fn termination_preserves_typing_back_to_the_old_base_after_commit_without_callback() {
        let fixture = DiskFixture::new();
        let original = ScriptStore::open(&fixture.0)
            .unwrap()
            .create("project-a", "旧基线甲")
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
            .create("project-a", "第一版")
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
            .create("project-a", "仍要保留的甲稿")
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
            .create("project-a", "第一版")
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
}
