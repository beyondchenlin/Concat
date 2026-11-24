// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 Jareer Abdullah

//! Persistent drafts use the existing manifest, identity and save authority.
//! Only an explicit marker identifies an unused draft; old projects never do.

use std::io::Write;
use std::path::Path;

use concat_project::model::{Project, VideoSettings};

use super::{MANIFEST, ProjectInfo, folder_name, now_millis};

/// Additive metadata; it does not rename the installation or project format.
pub const DRAFT_FIELD: &str = "bkCutDraft";

const KEEP_MARKER: &str = ".bkcut-draft-keep";
const REUSABLE_MARKER: &str = ".bkcut-unopened-draft";

/// Remember only an unused result that never reached an editor. The marker
/// contains its exact manifest, not an instruction to overwrite or delete it.
/// A later edit or separate-work keep marker makes the candidate ineligible.
pub fn remember_unopened(info: &ProjectInfo) -> Result<(), String> {
    let path = Path::new(&info.path);
    let _lock = crate::lock::ProjectLock::acquire(path, "unused-draft")
        .map_err(|error| error.to_string())?;
    if !reusable(&info.path) {
        return Ok(());
    }
    let bytes = std::fs::read(super::manifest_path(path))
        .map_err(|error| format!("{}: {error}", path.display()))?;
    let document: serde_json::Value =
        serde_json::from_slice(&bytes).map_err(|error| error.to_string())?;
    if crate::project_identity::from_document(&document)
        .map_err(|error| error.to_string())?
        .is_none()
    {
        return Ok(());
    }
    let marker = path.join(REUSABLE_MARKER);
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&marker)
    {
        Ok(mut file) => file
            .write_all(&bytes)
            .and_then(|()| file.sync_all())
            .map_err(|error| format!("{}: {error}", marker.display())),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            if std::fs::read(&marker).is_ok_and(|existing| existing == bytes) {
                Ok(())
            } else {
                Err(format!(
                    "{}: unused draft marker does not match",
                    marker.display()
                ))
            }
        }
        Err(error) => Err(format!("{}: {error}", marker.display())),
    }
}

/// Claim a previously superseded creation before making another empty folder.
/// Only explicit markers in this requested parent are considered, in stable
/// path order. The native project lock serializes claims; a user edit, keep
/// marker, mismatching creation defaults or held project is never reused.
pub fn take_unopened(
    location: &str,
    video: VideoSettings,
    match_media: bool,
) -> Result<Option<ProjectInfo>, String> {
    let entries = match std::fs::read_dir(location) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("{location}: {error}")),
    };
    let mut paths = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| format!("{location}: {error}"))?;
        if entry.file_type().is_ok_and(|kind| kind.is_dir())
            && entry.path().join(REUSABLE_MARKER).is_file()
        {
            paths.push(entry.path());
        }
    }
    paths.sort();
    for path in paths {
        let marker = path.join(REUSABLE_MARKER);
        if std::fs::symlink_metadata(&marker).map_or(true, |metadata| {
            metadata.file_type().is_symlink() || !metadata.is_file()
        }) {
            continue;
        }
        let Ok(_lock) = crate::lock::ProjectLock::acquire(&path, "unused-draft") else {
            continue;
        };
        let path_text = path.to_string_lossy();
        if !reusable(&path_text) {
            continue;
        }
        let Ok(original) = std::fs::read(&marker) else {
            continue;
        };
        let Ok(current) = std::fs::read(super::manifest_path(&path)) else {
            continue;
        };
        if original != current {
            continue;
        }
        let Ok(document) = serde_json::from_slice::<serde_json::Value>(&current) else {
            continue;
        };
        let Some(project) = concat_project::from_document(&document) else {
            continue;
        };
        let initial = concat_project::initial_format::read(&project);
        let state = if match_media {
            concat_project::initial_format::State::Waiting
        } else {
            concat_project::initial_format::State::Disabled
        };
        if initial.defaults != video || initial.state != state {
            continue;
        }
        let info = super::open(&path_text)?;
        // This is our metadata only. No project, media, history or keep file
        // is removed. A superseded/failed open remembers it again.
        std::fs::remove_file(&marker).map_err(|error| format!("{}: {error}", marker.display()))?;
        return Ok(Some(info));
    }
    Ok(None)
}

/// A separately durable keep marker protects independent text/conversation work
/// without saving or changing the native edit's current unsaved revision.
/// Existence is the entire marker protocol, so even an interrupted write fails
/// conservatively toward retaining the draft.
pub fn external_work_retained(path: &Path) -> bool {
    match std::fs::symlink_metadata(path.join(KEEP_MARKER)) {
        Ok(_) => true,
        Err(error) => error.kind() != std::io::ErrorKind::NotFound,
    }
}

/// Before independent user work is persisted, atomically retain a recognized
/// draft. Established projects are a no-op. Never overwrite any project file.
pub fn retain_external_work(path: &Path) -> Result<(), String> {
    let manifest = super::manifest_path(path);
    let document: serde_json::Value = serde_json::from_slice(
        &std::fs::read(&manifest).map_err(|error| format!("{}: {error}", manifest.display()))?,
    )
    .map_err(|error| format!("{}: {error}", manifest.display()))?;
    if document
        .get(DRAFT_FIELD)
        .and_then(|value| value.get("version"))
        .and_then(serde_json::Value::as_u64)
        != Some(1)
    {
        return Ok(());
    }
    let marker = path.join(KEEP_MARKER);
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&marker)
    {
        Ok(file) => file
            .sync_all()
            .map_err(|error| format!("{}: {error}", marker.display())),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            // A marker is immutable, and re-opening it is unnecessary.
            Ok(())
        }
        Err(error) => Err(format!("{}: {error}", marker.display())),
    }
}

/// Create a fully readable native document before exposing its editor.
pub fn create_draft(
    location: &str,
    name: &str,
    video: VideoSettings,
    match_media: bool,
) -> Result<ProjectInfo, String> {
    if !video.is_sane() || location.trim().is_empty() {
        return Err("invalid new-project defaults".into());
    }
    let identity = crate::project_identity::new()?;
    let mut project = Project::with_video(video);
    concat_project::initial_format::initialize(&mut project, match_media);
    project.extra.insert(
        DRAFT_FIELD.into(),
        serde_json::json!({
            "version": 1, "retained": false, "initialName": name,
        }),
    );
    let settings = concat_project::DocumentSettings {
        name: name.into(),
        width: video.width,
        height: video.height,
        rate_num: video.rate_num,
        rate_den: video.rate_den,
        canvas: video.canvas,
    };
    let mut document = concat_project::to_document(&settings, &project);
    document[crate::project_identity::FIELD] = identity.into();
    let bytes = serde_json::to_vec_pretty(&document).map_err(|error| error.to_string())?;
    let parent = Path::new(location);
    std::fs::create_dir_all(parent).map_err(|error| format!("{}: {error}", parent.display()))?;
    let root = parent.join(folder_name(name));
    // Never take an existing folder, even if it does not yet have a manifest.
    std::fs::create_dir(&root).map_err(|error| format!("{}: {error}", root.display()))?;
    let manifest = root.join(MANIFEST);
    let saved = (|| {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&manifest)?;
        file.write_all(&bytes)?;
        file.sync_all()
    })();
    if let Err(error) = saved {
        // This function created these two paths; no recursive cleanup.
        let _ = std::fs::remove_file(&manifest);
        let _ = std::fs::remove_dir(&root);
        return Err(format!("{}: {error}", manifest.display()));
    }
    Ok(ProjectInfo {
        path: root.to_string_lossy().into_owned(),
        name: name.into(),
        width: video.width,
        height: video.height,
        rate_num: video.rate_num,
        rate_den: video.rate_den,
        canvas: video.canvas,
        opened_at: now_millis(),
    })
}

/// A marker from this version with no user keep action.
pub fn unretained(project: &Project) -> bool {
    project.extra.get(DRAFT_FIELD).is_some_and(|value| {
        value.get("version").and_then(serde_json::Value::as_u64) == Some(1)
            && value.get("retained").and_then(serde_json::Value::as_bool) == Some(false)
    })
}

/// A disk draft is reusable, never deleted, after a previous run. Opening it
/// preserves its identity and any independently persisted conversation/writing.
pub fn reusable(path: &str) -> bool {
    if external_work_retained(Path::new(path)) {
        return false;
    }
    let Ok(bytes) = std::fs::read(super::manifest_path(Path::new(path))) else {
        return false;
    };
    let Ok(document) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        return false;
    };
    let Some(project) = concat_project::from_document(&document) else {
        return false;
    };
    unretained(&project)
        && project.media.is_empty()
        && project
            .timelines
            .iter()
            .all(|timeline| timeline.clips.is_empty())
}

/// Do not hide an established edit whose name, media or timelines were changed.
pub fn hidden_from_recents(path: &str) -> bool {
    reusable(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn superseded_creation_is_reused_once_with_its_original_identity() {
        let temp = tempfile::tempdir().unwrap();
        let location = temp.path().to_str().unwrap();
        let info = create_draft(location, "Late", VideoSettings::default(), true).unwrap();
        let original = std::fs::read(Path::new(&info.path).join(MANIFEST)).unwrap();
        remember_unopened(&info).unwrap();
        remember_unopened(&info).unwrap();
        let claimed = take_unopened(location, VideoSettings::default(), true)
            .unwrap()
            .unwrap();
        assert_eq!(claimed.path, info.path);
        assert_eq!(
            std::fs::read(Path::new(&claimed.path).join(MANIFEST)).unwrap(),
            original
        );
        assert!(
            take_unopened(location, VideoSettings::default(), true)
                .unwrap()
                .is_none()
        );
        // A second explicit open can supersede this claim too.
        remember_unopened(&claimed).unwrap();
        assert_eq!(
            take_unopened(location, VideoSettings::default(), true)
                .unwrap()
                .unwrap()
                .path,
            info.path
        );
    }

    #[test]
    fn unopened_candidate_never_replaces_changed_retained_or_live_work() {
        let temp = tempfile::tempdir().unwrap();
        let location = temp.path().to_str().unwrap();
        let info = create_draft(location, "Late", VideoSettings::default(), true).unwrap();
        remember_unopened(&info).unwrap();
        let lock = crate::lock::ProjectLock::acquire(Path::new(&info.path), "window").unwrap();
        assert!(
            take_unopened(location, VideoSettings::default(), true)
                .unwrap()
                .is_none()
        );
        drop(lock);
        let defaults = VideoSettings {
            width: 1280,
            ..VideoSettings::default()
        };
        assert!(take_unopened(location, defaults, true).unwrap().is_none());
        let manifest = Path::new(&info.path).join(MANIFEST);
        let original = std::fs::read(&manifest).unwrap();
        let mut changed = original.clone();
        changed.push(b'\n');
        std::fs::write(&manifest, &changed).unwrap();
        assert!(
            take_unopened(location, VideoSettings::default(), true)
                .unwrap()
                .is_none()
        );
        assert_eq!(std::fs::read(&manifest).unwrap(), changed);
        std::fs::write(&manifest, &original).unwrap();
        retain_external_work(Path::new(&info.path)).unwrap();
        assert!(
            take_unopened(location, VideoSettings::default(), true)
                .unwrap()
                .is_none()
        );
        assert_eq!(std::fs::read(&manifest).unwrap(), original);
    }

    #[test]
    fn external_work_survives_reopen_without_saving_the_native_edit() {
        let temp = tempfile::tempdir().unwrap();
        let info = create_draft(
            temp.path().to_str().unwrap(),
            "Keep",
            VideoSettings::default(),
            true,
        )
        .unwrap();
        let path = Path::new(&info.path);
        let before = std::fs::read(path.join(MANIFEST)).unwrap();
        let mut session = crate::session::Session::open_info(&info).unwrap();
        let revision = session.revision();
        assert!(session.is_unused_draft());
        retain_external_work(path).unwrap();
        retain_external_work(path).unwrap();
        assert!(external_work_retained(path));
        assert!(!reusable(&info.path));
        assert!(!hidden_from_recents(&info.path));
        assert!(!session.is_unused_draft());
        assert_eq!(session.revision(), revision);
        assert_eq!(std::fs::read(path.join(MANIFEST)).unwrap(), before);
        session.preserve_draft().unwrap();
        assert!(
            !crate::session::Session::open_info(&info)
                .unwrap()
                .is_unused_draft()
        );
    }

    #[test]
    fn retaining_legacy_work_does_not_add_draft_metadata() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path();
        std::fs::write(path.join(MANIFEST), b"{}").unwrap();
        retain_external_work(path).unwrap();
        assert!(!external_work_retained(path));
        assert_eq!(std::fs::read(path.join(MANIFEST)).unwrap(), b"{}");
        assert!(retain_external_work(&path.join("absent")).is_err());
    }

    #[test]
    fn drafts_are_complete_persistent_documents_and_do_not_overwrite() {
        let temp = tempfile::tempdir().unwrap();
        let base = temp.path().to_str().unwrap();
        let info = create_draft(base, "Draft", VideoSettings::default(), true).unwrap();
        assert!(reusable(&info.path));
        let value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(Path::new(&info.path).join(MANIFEST)).unwrap())
                .unwrap();
        assert!(
            crate::project_identity::from_document(&value)
                .unwrap()
                .is_some()
        );
        let project = concat_project::from_document(&value).unwrap();
        assert_eq!(
            concat_project::initial_format::read(&project).state,
            concat_project::initial_format::State::Waiting
        );
        assert!(create_draft(base, "Draft", VideoSettings::default(), true).is_err());
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(
                &std::fs::read(Path::new(&info.path).join(MANIFEST)).unwrap()
            )
            .unwrap(),
            value
        );
    }
}
