// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 Jareer Abdullah

//! The engine-owned editing session.
//!
//! Open a project folder and the engine holds the edit: every mutation is a
//! `concat_project` [`Command`], applied with undo recorded, and the new
//! state is what the window draws. The window never keeps a model of its
//! own; it renders the [`Project`] this session hands back.
//!
//! Saving reuses `projects::save`'s temp-file-and-rename, so the document on
//! disk is written by exactly one code path.

use concat_project::model::VideoSettings;
use concat_project::{Command, DocumentSettings, Editor, Project};
use serde::Serialize;
use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::projects;

static NEXT_SESSION_ID: AtomicU64 = AtomicU64::new(1);

fn next_session_id() -> Result<u64, String> {
    NEXT_SESSION_ID
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |id| id.checked_add(1))
        .map_err(|_| "native session identity exhausted".to_owned())
}

/// One open project: its folder, its settings and its undo history.
pub struct Session {
    /// Identity of this open, never reused within a native process.
    session_id: u64,
    /// Travels with the saved manifest and never belongs to undo snapshots.
    project_identity: String,
    /// Native-only save permission, shared with every captured snapshot.
    save_authority: projects::SaveAuthority,
    /// Advances for manual edits, agent edits, undo, redo, names and view changes.
    revision: u64,
    /// The project folder, for saving.
    path: String,
    settings: DocumentSettings,
    editor: Editor,
    /// The links to retired effects the open rewrote, old id and new, each
    /// pair once: what the window names as the project opens.
    upgraded: Vec<(String, String)>,
    /// The clips whose retired fields the open took off.
    retired: concat_project::RetiredFields,
}

/// What every mutating call returns: the authoritative state plus history
/// availability, so undo/redo affordances are never guessing.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct EditorView {
    /// Identity of this particular open; a path alone is not an edit target.
    pub session_id: u64,
    /// Revision which an agent plan must retain until native submission.
    pub revision: u64,
    /// The whole project, as the engine holds it.
    pub project: Project,
    /// Whether there is something to undo.
    pub can_undo: bool,
    /// Whether there is something to redo.
    pub can_redo: bool,
    /// The settings as the session holds them - the document's own output
    /// size wins over the manifest's on open.
    pub settings: SettingsView,
    /// The id a creating command minted, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_id: Option<String>,
}

/// The session's settings, as the window shows them.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct SettingsView {
    /// Project name.
    pub name: String,
    /// Output width in pixels.
    pub width: u32,
    /// Output height in pixels.
    pub height: u32,
    /// Frame rate numerator.
    pub rate_num: i64,
    /// Frame rate denominator.
    pub rate_den: i64,
}

impl Session {
    /// Opens a project folder as the editing session.
    ///
    /// A folder with no document yet opens as an empty project rather than
    /// failing, but a document that is there and cannot be read or parsed
    /// is an error, because silently replacing an edit with emptiness is
    /// how projects get lost. `settings` come from the manifest and seed the first timeline
    /// of a project that has no document yet; a document that loads brings
    /// every timeline's own frame with it, and those win, because that is
    /// where an edited frame was saved.
    pub fn open(path: &str, settings: DocumentSettings) -> Result<Session, String> {
        let (document, source) = projects::read_for_session(path)?;
        let project_identity = document
            .as_ref()
            .map(crate::project_identity::from_document)
            .transpose()?
            .flatten();
        // Identity belongs to the native host, never to an undoable Project
        // snapshot or model-supplied extra fields. Put it back only on save.
        let document = document.map(|mut document| {
            if let Some(object) = document.as_object_mut() {
                object.remove(crate::project_identity::FIELD);
            }
            document
        });
        let editor = match document {
            Some(document) => match Editor::from_document(&document) {
                Some(editor) => editor,
                // The settings-only manifest `create` writes: a project
                // closed before its first edit reopens empty, it is not
                // corrupt.
                None if projects::is_settings_only(&document) => {
                    Editor::with_video(settings.video())
                }
                None if concat_project::document_version(&document)
                    > concat_project::DOCUMENT_VERSION =>
                {
                    return Err(format!(
                        "{path} was saved by a newer Concat than this one: update to open it"
                    ));
                }
                None => {
                    return Err(format!("{path} holds a document this build cannot read"));
                }
            },
            // No document yet - a project created moments ago.
            None => Editor::with_video(settings.video()),
        };
        // A link to a retired effect opens as a link to the one that stands
        // in for it, its knobs carried over (`[[replaces]]`).
        let mut editor = editor;
        let catalogue = concat_effects::Catalogue::builtin();
        let mut upgraded: Vec<(String, String)> = Vec::new();
        editor.upgrade_links(|link| {
            let old = link.id.clone();
            let changed = catalogue.upgrade(link);
            let pair = (old, link.id.clone());
            // A link carried over to its own package's newer version opens
            // as it looked, near enough, and says nothing.
            if changed && pair.0 != pair.1 && !upgraded.contains(&pair) {
                upgraded.push(pair);
            }
            changed
        });
        // Fields an earlier version wrote and this one does not read, off
        // the clips, with the clips named for the window to say so.
        let retired = editor.drop_retired_fields();
        let already_identified = project_identity.is_some();
        let project_identity = match project_identity {
            Some(identity) => identity,
            None => crate::project_identity::new()?,
        };
        let save_authority =
            projects::SaveAuthority::opened(project_identity.clone(), source, already_identified);
        Ok(Session {
            session_id: next_session_id()?,
            project_identity,
            save_authority,
            revision: 0,
            path: path.to_owned(),
            settings,
            editor,
            upgraded,
            retired,
        })
    }

    /// Opens the project a [`projects::ProjectInfo`] describes.
    pub fn open_info(info: &projects::ProjectInfo) -> Result<Session, String> {
        Session::open(
            &info.path,
            DocumentSettings {
                name: info.name.clone(),
                width: info.width,
                height: info.height,
                rate_num: info.rate_num,
                rate_den: info.rate_den,
                canvas: info.canvas,
            },
        )
    }

    /// The links to retired effects this open read as links to the
    /// effects that stand in for them, old id and new. Empty when the
    /// document named none; the change is the document's once it is saved.
    pub fn upgraded(&self) -> &[(String, String)] {
        &self.upgraded
    }

    /// The clips this open found with fields an earlier version wrote and
    /// this one no longer reads, now taken off: reversed ones that play
    /// forwards, animated ones that hold still. Empty for most documents.
    pub fn retired(&self) -> &concat_project::RetiredFields {
        &self.retired
    }

    /// A copy of this session as it stands, with no history: what renders
    /// or reads the project off the thread that owns it - an API caller's
    /// export of the project open in the window. Cheap, since timelines are
    /// shared until edited.
    pub fn snapshot(&self) -> Session {
        Session {
            session_id: self.session_id,
            project_identity: self.project_identity.clone(),
            save_authority: self.save_authority.clone(),
            revision: self.revision,
            path: self.path.clone(),
            settings: self.settings.clone(),
            editor: Editor::from_project(self.editor.project().clone()),
            upgraded: Vec::new(),
            retired: Default::default(),
        }
    }

    /// The project folder.
    pub fn path(&self) -> &str {
        &self.path
    }

    /// Persistent native identity. A caller must persist the native document
    /// before associating independent data with a newly opened legacy project.
    pub fn project_identity(&self) -> &str {
        &self.project_identity
    }

    /// Capture with the native document before dispatching a background save.
    /// All clones share the one-time legacy initialization state.
    pub fn save_authority(&self) -> projects::SaveAuthority {
        self.save_authority.clone()
    }

    /// Identity of the current open, including when a path is reopened.
    pub fn session_id(&self) -> u64 {
        self.session_id
    }

    /// Current revision, shared by manual and guarded agent edits.
    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// Reject a plan made for a closed open-session or an older edit.
    pub fn check_revision(&self, session_id: u64, revision: u64) -> Result<(), String> {
        if self.session_id != session_id {
            return Err("the plan belongs to another open project session".to_owned());
        }
        if self.revision != revision {
            return Err("the project changed after the plan was read".to_owned());
        }
        Ok(())
    }

    /// The session's settings: the project's name, and the frame and rate of
    /// the timeline being edited.
    ///
    /// Built rather than stored, because the frame is the active timeline's
    /// and the active timeline changes under a tab click. Everything that
    /// renders - the monitor, the export - asks here and gets the frame of
    /// whatever it is about to draw.
    pub fn settings(&self) -> DocumentSettings {
        let video = self.editor.project().active().video;
        DocumentSettings {
            name: self.settings.name.clone(),
            width: video.width,
            height: video.height,
            rate_num: video.rate_num,
            rate_den: video.rate_den,
            canvas: video.canvas,
        }
    }

    /// The active timeline's frame and rate.
    pub fn video(&self) -> VideoSettings {
        self.editor.project().active().video
    }

    /// Sets the active timeline's frame and rate, as an edit - undoable,
    /// and this timeline's alone. Anything a frame could not be is ignored
    /// by the command, for the reason a zero dimension is in
    /// [`Session::prepare_save`].
    pub fn set_video(&mut self, video: VideoSettings) -> Result<EditorView, String> {
        let timeline_id = self.editor.project().active_timeline_id.clone();
        self.apply(Command::SetTimelineVideo { timeline_id, video })
    }

    /// The edit as it stands.
    pub fn project(&self) -> &Project {
        self.editor.project()
    }

    /// Whether there is something to undo.
    pub fn can_undo(&self) -> bool {
        self.editor.can_undo()
    }

    /// Whether there is something to redo.
    pub fn can_redo(&self) -> bool {
        self.editor.can_redo()
    }

    /// The current state without changing anything.
    pub fn view(&self) -> EditorView {
        self.view_with(None)
    }

    fn view_with(&self, created_id: Option<String>) -> EditorView {
        let settings = self.settings();
        EditorView {
            session_id: self.session_id,
            revision: self.revision,
            project: self.editor.project().clone(),
            can_undo: self.editor.can_undo(),
            can_redo: self.editor.can_redo(),
            settings: SettingsView {
                name: settings.name,
                width: settings.width,
                height: settings.height,
                rate_num: settings.rate_num,
                rate_den: settings.rate_den,
            },
            created_id,
        }
    }

    /// Applies one edit command and returns the new state.
    pub fn apply(&mut self, command: Command) -> Result<EditorView, String> {
        self.apply_within(None, command)
    }

    /// Applies one edit command as a move of `gesture`, so that a knob
    /// dragged through many values is one undo step; see
    /// `concat_project::Editor::apply_within`. `None` is a step of its own.
    pub fn apply_within(
        &mut self,
        gesture: Option<&str>,
        command: Command,
    ) -> Result<EditorView, String> {
        let next = self
            .revision
            .checked_add(1)
            .ok_or_else(|| "native project revision exhausted".to_owned())?;
        let outcome = self
            .editor
            .apply_within(gesture, command)
            .map_err(|error| error.to_string())?;
        if outcome.applied {
            self.revision = next;
        }
        Ok(self.view_with(outcome.created_id))
    }

    /// Submit a strict agent batch on the native edit thread. The caller must
    /// hold a valid host permit and collect the window's current track locks in
    /// that same callback. Rejection preserves the edit and its history.
    pub fn apply_guarded(
        &mut self,
        command: Command,
        locked_tracks: &HashSet<String>,
        expected_session: u64,
        expected_revision: u64,
    ) -> Result<EditorView, String> {
        self.check_revision(expected_session, expected_revision)?;
        let next = self
            .revision
            .checked_add(1)
            .ok_or_else(|| "native project revision exhausted".to_owned())?;
        let outcome = self
            .editor
            .apply_guarded(command, locked_tracks)
            .map_err(|error| error.to_string())?;
        if outcome.applied {
            self.revision = next;
        }
        Ok(self.view_with(outcome.created_id))
    }

    /// Ends the gesture in progress: the next command naming it starts a
    /// new undo step.
    pub fn end_gesture(&mut self) {
        self.editor.end_gesture();
    }

    /// Steps the history back one edit.
    pub fn undo(&mut self) -> EditorView {
        if self.revision < u64::MAX && self.editor.undo() {
            self.revision += 1;
        }
        self.view()
    }

    /// Steps the history forward one edit.
    pub fn redo(&mut self) -> EditorView {
        if self.revision < u64::MAX && self.editor.redo() {
            self.revision += 1;
        }
        self.view()
    }

    /// Takes a new name, if there is one, and hands back what a save must
    /// write: the folder and the document. The frame is not a parameter any
    /// more - it is the active timeline's, set through [`Session::set_video`]
    /// as an edit. The disk write is the caller's, so it can happen off the
    /// thread that owns the session; [`Session::save`] does both. A changed
    /// name advances the same revision as native edits, before any snapshot is
    /// captured. An older save completion must not mark that new name saved.
    /// Revision exhaustion refuses the rename without changing the document.
    pub fn prepare_save(
        &mut self,
        name: Option<&str>,
    ) -> Result<(String, serde_json::Value), String> {
        if let Some(name) = name {
            let trimmed = name.trim();
            if !trimmed.is_empty() && trimmed != self.settings.name.as_str() {
                let next = self
                    .revision
                    .checked_add(1)
                    .ok_or_else(|| "native project revision exhausted".to_owned())?;
                self.settings.name = trimmed.to_owned();
                self.revision = next;
            }
        }
        Ok((self.path.clone(), self.document()))
    }

    /// Writes the session's document to its project folder.
    pub fn save(&mut self, name: Option<&str>) -> Result<(), String> {
        let (path, document) = self.prepare_save(name)?;
        projects::save_ordered_with_authority(
            &path,
            &document,
            projects::ticket(),
            &self.save_authority,
        )
        .map(|_| ())
    }

    /// The document as it would be saved.
    pub fn document(&self) -> serde_json::Value {
        let mut document = self.editor.to_document(&self.settings);
        document[crate::project_identity::FIELD] = self.project_identity.clone().into();
        document
    }

    /// The active timeline flattened for rendering. This is what export and
    /// preview consume: the engine flattens its own session, so the pixels
    /// rendered are the model's, never a copy of it.
    pub fn flattened_clips(&self) -> Vec<concat_export::ExportClip> {
        concat_export::flatten::flatten_timeline_in(
            self.editor.project(),
            None,
            Some(std::path::Path::new(&self.path)),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reopening_the_same_path_invalidates_a_plan() {
        let scratch = tempfile::tempdir().unwrap();
        let path = scratch.path().to_str().unwrap();
        let first = Session::open(path, settings()).unwrap();
        let second = Session::open(path, settings()).unwrap();
        assert_ne!(first.session_id(), second.session_id());
        assert!(
            second
                .check_revision(first.session_id(), first.revision())
                .is_err()
        );
    }

    #[test]
    fn manual_edit_and_undo_still_invalidate_old_plans() {
        let scratch = tempfile::tempdir().unwrap();
        let mut session = Session::open(scratch.path().to_str().unwrap(), settings()).unwrap();
        let original = session.document();
        let id = session.session_id();
        let revision = session.revision();
        session.apply(Command::AddTrack { bottom: false }).unwrap();
        session.undo();
        assert_eq!(session.document(), original);
        assert!(session.check_revision(id, revision).is_err());
        let before_redo = session.revision();
        session.redo();
        assert!(session.revision() > before_redo);
    }

    #[test]
    fn lock_rejection_does_not_advance_a_native_session_revision() {
        let scratch = tempfile::tempdir().unwrap();
        let mut session = Session::open(scratch.path().to_str().unwrap(), settings()).unwrap();
        let track = session.project().active().tracks[0].id.clone();
        let id = session.session_id();
        let revision = session.revision();
        let before = session.document();
        let command = Command::AddTextClip {
            track_id: Some(track.clone()),
            above: false,
            start: 0.0,
            style: None,
            duration: Some(2.0),
            offset_y: None,
        };
        assert!(
            session
                .apply_guarded(command.clone(), &HashSet::from([track]), id, revision)
                .is_err()
        );
        assert_eq!(session.revision(), revision);
        assert_eq!(session.document(), before);
        assert!(!session.can_undo());
        session
            .apply_guarded(command, &HashSet::new(), id, revision)
            .unwrap();
        assert!(session.revision() > revision);
        session.undo();
        assert_eq!(session.document(), before);
    }

    #[test]
    fn renaming_invalidates_plans_but_unchanged_or_empty_names_do_not() {
        let scratch = tempfile::tempdir().expect("isolated project");
        let mut session =
            Session::open(scratch.path().to_str().unwrap(), settings()).expect("opens");
        let id = session.session_id();
        let before = session.revision();
        let (_, renamed) = session
            .prepare_save(Some("  Renamed  "))
            .expect("prepares the changed name");
        assert_eq!(session.settings().name, "Renamed");
        assert_eq!(renamed["name"], "Renamed");
        assert_eq!(session.revision(), before + 1);
        assert!(
            session.check_revision(id, before).is_err(),
            "old plans are stale"
        );

        let current = session.revision();
        for name in [
            None,
            Some("Renamed"),
            Some("  Renamed  "),
            Some(""),
            Some(" \t\n "),
        ] {
            let (_, document) = session
                .prepare_save(name)
                .expect("captures without renaming");
            assert_eq!(document, renamed);
            assert_eq!(session.revision(), current, "no new document version");
        }

        session.revision = u64::MAX;
        assert!(session.prepare_save(Some("Cannot fit")).is_err());
        assert_eq!(session.revision(), u64::MAX);
        assert_eq!(
            session.document(),
            renamed,
            "exhaustion does not partially rename"
        );
        assert!(session.prepare_save(Some("Renamed")).is_ok());
        assert!(
            session.prepare_save(None).is_ok(),
            "an unchanged save still works"
        );
    }

    #[test]
    fn an_old_snapshot_written_after_a_rename_is_not_the_current_saved_version() {
        let scratch = tempfile::tempdir().expect("isolated project");
        let info = projects::create(
            scratch.path().to_str().unwrap(),
            "Before",
            1920,
            1080,
            30,
            1,
            [0, 0, 0, 255],
        )
        .expect("creates a real project");
        let mut session = Session::open_info(&info).expect("opens");
        let (path, old_document) = session.prepare_save(None).expect("captures old save");
        let old_ticket = projects::ticket();
        let saved_session = session.session_id();
        let saved_revision = session.revision();

        session
            .prepare_save(Some("After"))
            .expect("renames while old save is pending");
        // The native project panel also submits the unchanged video settings.
        // That no-op must not erase the revision allocated by the name change.
        session
            .set_video(session.video())
            .expect("unchanged video settings");
        assert_eq!(
            projects::save_ordered(&path, &old_document, old_ticket).expect("writes old capture"),
            projects::Saved::Written
        );
        assert_eq!(
            projects::read_document(&path).expect("reads actual disk"),
            Some(old_document)
        );
        assert_eq!(
            projects::open(&path).expect("opens actual manifest").name,
            "Before"
        );
        assert_eq!(session.settings().name, "After");
        // This is the same check the window's save completion uses before it
        // clears the unsaved marker or announces a successful current save.
        assert!(
            session
                .check_revision(saved_session, saved_revision)
                .is_err()
        );

        let (path, new_document) = session.prepare_save(None).expect("captures current save");
        let current_revision = session.revision();
        assert_eq!(
            projects::save_ordered(&path, &new_document, projects::ticket())
                .expect("writes current capture"),
            projects::Saved::Written
        );
        assert_eq!(
            projects::read_document(&path).expect("reads current disk"),
            Some(new_document)
        );
        assert_eq!(
            projects::open(&path).expect("opens current manifest").name,
            "After"
        );
        assert!(
            session
                .check_revision(saved_session, current_revision)
                .is_ok()
        );
    }

    fn settings() -> DocumentSettings {
        DocumentSettings {
            canvas: [0, 0, 0, 255],
            name: "Test".to_owned(),
            width: 1920,
            height: 1080,
            rate_num: 30,
            rate_den: 1,
        }
    }

    #[test]
    fn a_fresh_project_opens_empty_and_round_trips_a_save() {
        let scratch =
            std::env::temp_dir().join(format!("concat-session-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&scratch);
        std::fs::create_dir_all(&scratch).expect("scratch dir");
        let info = projects::create(
            &scratch.to_string_lossy(),
            "Fresh",
            1920,
            1080,
            30,
            1,
            [0, 0, 0, 255],
        )
        .expect("creates");

        let mut session = Session::open_info(&info).expect("opens the settings-only manifest");
        assert!(!session.can_undo());
        assert_eq!(
            (session.settings().width, session.settings().height),
            (1920, 1080),
            "the manifest's frame seeds the first timeline"
        );
        let view = session
            .apply(Command::AddTrack { bottom: false })
            .expect("adds a track");
        assert!(view.can_undo);
        session
            .set_video(VideoSettings {
                canvas: [0, 0, 0, 255],
                width: 1280,
                height: 720,
                rate_num: 25,
                rate_den: 1,
                color_space: Default::default(),
            })
            .expect("sets the frame");
        session.save(Some("Renamed")).expect("saves");

        let reopened = Session::open(&info.path, settings()).expect("reopens");
        assert_eq!(
            reopened.settings().name,
            "Test",
            "the manifest's name is what open gets"
        );
        assert_eq!(
            (reopened.settings().width, reopened.settings().height),
            (1280, 720),
            "the document's frame wins over the manifest's"
        );
        assert_eq!(reopened.settings().rate_num, 25, "and so does its rate");
        assert_eq!(
            reopened.project().active().tracks.len(),
            session.project().active().tracks.len()
        );

        let _ = std::fs::remove_dir_all(&scratch);
    }

    #[test]
    fn a_snapshot_is_the_project_as_it_stands_and_leaves_the_session_alone() {
        let scratch =
            std::env::temp_dir().join(format!("concat-snapshot-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&scratch);
        std::fs::create_dir_all(&scratch).expect("scratch dir");
        let mut session = Session::open(&scratch.to_string_lossy(), settings()).expect("opens");
        session
            .apply(Command::AddTrack { bottom: false })
            .expect("adds a track");

        let mut snapshot = session.snapshot();
        assert_eq!(snapshot.document(), session.document(), "the same document");
        assert_eq!(snapshot.path(), session.path());
        assert!(!snapshot.can_undo(), "with no history of its own");

        snapshot
            .apply(Command::AddTrack { bottom: false })
            .expect("a snapshot edits like any session");
        assert_ne!(
            snapshot.project().active().tracks.len(),
            session.project().active().tracks.len(),
            "without reaching the session it was taken from"
        );
        let _ = std::fs::remove_dir_all(&scratch);
    }

    #[test]
    fn a_corrupt_document_is_refused() {
        let scratch =
            std::env::temp_dir().join(format!("concat-corrupt-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&scratch);
        std::fs::create_dir_all(&scratch).expect("scratch dir");
        std::fs::write(scratch.join("concat.json"), br#"{"timelines": "garbage"}"#)
            .expect("writes");
        assert!(Session::open(&scratch.to_string_lossy(), settings()).is_err());
        let _ = std::fs::remove_dir_all(&scratch);
    }

    #[test]
    fn persistent_identity_survives_save_reopen_snapshot_undo_and_folder_move() {
        let root = tempfile::tempdir().unwrap();
        let original = root.path().join("original");
        std::fs::create_dir(&original).unwrap();
        let mut session = Session::open(original.to_str().unwrap(), settings()).unwrap();
        let identity = session.project_identity().to_owned();
        assert!(crate::project_identity::valid(&identity));
        assert_eq!(
            crate::project_identity::read_persisted(&original).unwrap(),
            None
        );
        let before = session.document();
        session.apply(Command::AddTrack { bottom: false }).unwrap();
        session.undo();
        assert_eq!(session.document(), before);
        assert_eq!(session.snapshot().project_identity(), identity);
        session.save(None).unwrap();
        assert_eq!(
            crate::project_identity::read_persisted(&original).unwrap(),
            Some(identity.clone())
        );
        drop(session);
        let moved = root.path().join("moved");
        std::fs::rename(&original, &moved).unwrap();
        let mut reopened = Session::open(moved.to_str().unwrap(), settings()).unwrap();
        assert_eq!(reopened.project_identity(), identity);
        reopened.save(Some("Changed name")).unwrap();
        assert_eq!(
            crate::project_identity::read_persisted(&moved).unwrap(),
            Some(identity.clone())
        );
        std::fs::create_dir(&original).unwrap();
        let replacement = Session::open(original.to_str().unwrap(), settings()).unwrap();
        assert_ne!(replacement.project_identity(), identity);
    }

    #[test]
    fn corrupted_persistent_metadata_is_not_replaced_by_a_fresh_identity() {
        let root = tempfile::tempdir().unwrap();
        let mut session = Session::open(root.path().to_str().unwrap(), settings()).unwrap();
        session.save(None).unwrap();
        let mut document = session.document();
        document[crate::project_identity::FIELD] = serde_json::Value::Null;
        let bytes = serde_json::to_vec(&document).unwrap();
        std::fs::write(root.path().join("concat.json"), &bytes).unwrap();
        assert!(Session::open(root.path().to_str().unwrap(), settings()).is_err());
        assert_eq!(
            std::fs::read(root.path().join("concat.json")).unwrap(),
            bytes
        );
    }

    #[test]
    fn a_truncated_document_is_refused_rather_than_replaced() {
        let scratch =
            std::env::temp_dir().join(format!("concat-truncated-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&scratch);
        std::fs::create_dir_all(&scratch).expect("scratch dir");
        // A save cut short by a power cut or an outside tool: the file is
        // there, and it is not JSON. Opening it as an empty project would
        // let the next save replace whatever was edited.
        std::fs::write(
            scratch.join("concat.json"),
            br#"{"version": 1, "timelines": ["#,
        )
        .expect("writes");
        let opened = Session::open(&scratch.to_string_lossy(), settings());
        assert!(opened.is_err(), "a half document is not an empty project");
        let _ = std::fs::remove_dir_all(&scratch);
    }
}
