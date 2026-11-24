// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 Jareer Abdullah

//! One-time native canvas matching. The state lives in the project and is
//! captured by the same undo snapshot as the visual insertion that changes it.
//! Documents without this field are established edits, never fresh drafts.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};

use crate::model::{MediaItem, MediaKind, Project, VideoSettings};

/// Versioned, additive document field, preserved by older document readers.
pub const FIELD: &str = "bkCutInitialFormat";

/// Why the project has its current initial format.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum State {
    /// Await the first successfully inserted visual source.
    Waiting,
    /// A native visual insertion selected the initial frame.
    Matched,
    /// A user selected a frame, or this is an established legacy project.
    UserSet,
    /// The future-project default disabled matching at creation.
    Disabled,
}

/// Persisted facts, not inferred again from the first item in the media bin.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InitialFormat {
    /// Additive metadata schema version.
    pub version: u32,
    /// Current one-time matching state.
    pub state: State,
    /// Creation-time defaults; never read back from global preferences.
    pub defaults: VideoSettings,
    /// Stable provenance identifier, independent of interface text.
    pub source: String,
    /// Native media identity which determined the frame.
    pub media_id: Option<String>,
    /// Native timeline changed by the first insertion.
    pub timeline_id: Option<String>,
    /// Probed visual display width, including orientation and pixel aspect.
    pub display_width: Option<u32>,
    /// Probed visual display height, including orientation and pixel aspect.
    pub display_height: Option<u32>,
    /// True when an unreadable source rate retained the native default.
    pub used_default_rate: bool,
}

impl InitialFormat {
    fn established(project: &Project) -> Self {
        Self {
            version: 1,
            state: State::UserSet,
            defaults: project.active().video,
            source: "legacyProject".into(),
            media_id: None,
            timeline_id: None,
            display_width: None,
            display_height: None,
            used_default_rate: false,
        }
    }
}

/// Unknown or malformed metadata must never opt an existing edit into matching.
pub fn read(project: &Project) -> InitialFormat {
    project
        .extra
        .get(FIELD)
        .and_then(|value| serde_json::from_value::<InitialFormat>(value.clone()).ok())
        .filter(|value| value.version == 1 && value.defaults.is_sane())
        .unwrap_or_else(|| InitialFormat::established(project))
}

fn write(project: &mut Project, value: &InitialFormat) {
    // All members are finite native settings or strings.
    if let Ok(value) = serde_json::to_value(value) {
        project.extra.insert(FIELD.to_owned(), value);
    }
}

/// Called only while creating a new draft, before its first manifest is written.
pub fn initialize(project: &mut Project, enabled: bool) {
    let mut value = InitialFormat::established(project);
    value.state = if enabled {
        State::Waiting
    } else {
        State::Disabled
    };
    value.source = "newProjectDefaults".into();
    write(project, &value);
}

/// Record a genuine manual frame/rate change. Colour and name changes are not
/// passed here, and confirming the same frame does not end the waiting state.
pub fn user_set(project: &mut Project) {
    let mut value = read(project);
    value.state = State::UserSet;
    value.source = "user".into();
    write(project, &value);
}

/// Capture only while waiting; normal editing does not scan the entire timeline.
pub(crate) fn before(project: &Project) -> Option<HashSet<String>> {
    (read(project).state == State::Waiting).then(|| {
        project
            .timelines
            .iter()
            .flat_map(|timeline| timeline.clips.iter())
            .map(|clip| clip.id.clone())
            .collect()
    })
}

/// The source probe already resolves rotation and sample aspect ratio into
/// display dimensions. Never derive a frame rate from its rounded display fps.
fn settings(item: &MediaItem, defaults: VideoSettings) -> Option<(VideoSettings, bool)> {
    let (width, height) = (item.width?, item.height?);
    if width == 0 || height == 0 {
        return None;
    }
    let mut video = defaults;
    let mut fallback = false;
    match item.kind {
        MediaKind::Audio => return None,
        MediaKind::Image => {
            let scale = (1920.0 / f64::from(width.max(height))).min(1.0);
            // The nearest encoder-safe even frame must not enlarge a small
            // source. A one-pixel edge needs the minimum two-pixel canvas.
            let scaled = |side: u32| {
                let nearest = ((f64::from(side) * scale / 2.0).round() as u32) * 2;
                nearest.clamp(2, (side / 2 * 2).max(2))
            };
            video.width = scaled(width);
            video.height = scaled(height);
        }
        MediaKind::Video => {
            video.width = width;
            video.height = height;
            let rate = item
                .frame_rate_fraction
                .as_deref()
                .and_then(|fraction| fraction.split_once('/'))
                .and_then(|(num, den)| Some((num.parse::<i64>().ok()?, den.parse::<i64>().ok()?)))
                .filter(|(num, den)| {
                    *num > 0 && *den > 0 && i128::from(*num) <= 1_000 * i128::from(*den)
                });
            if let Some((num, den)) = rate {
                video.rate_num = num;
                video.rate_den = den;
            } else {
                fallback = true;
            }
        }
    }
    video.is_sane().then_some((video, fallback))
}

/// Run only after a successful native command. It observes actual new visual
/// clips, so library imports, previews, failed inserts and audio never trigger it.
pub(crate) fn after(project: &mut Project, before: Option<HashSet<String>>) {
    let Some(before) = before else {
        return;
    };
    let mut value = read(project);
    if value.state != State::Waiting {
        return;
    }
    let first = project.timelines.iter().find_map(|timeline| {
        timeline
            .clips
            .iter()
            .filter(|clip| !before.contains(&clip.id) && clip.kind.is_visual())
            .find_map(|clip| {
                let item = project.media_by_id(&clip.media_id)?;
                let (video, fallback) = settings(item, timeline.video)?;
                Some((
                    timeline.id.clone(),
                    item.id.clone(),
                    item.width,
                    item.height,
                    video,
                    fallback,
                ))
            })
    });
    let Some((timeline_id, media_id, width, height, video, fallback)) = first else {
        return;
    };
    let Some(timeline) = project
        .timelines
        .iter_mut()
        .find(|timeline| timeline.id == timeline_id)
    else {
        return;
    };
    std::sync::Arc::make_mut(timeline).video = video;
    value.state = State::Matched;
    value.source = "visualTimelineInsertion".into();
    value.media_id = Some(media_id);
    value.timeline_id = Some(timeline_id);
    value.display_width = width;
    value.display_height = height;
    value.used_default_rate = fallback;
    write(project, &value);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::NewMedia;
    use crate::{Command, Editor};

    fn editor() -> Editor {
        let mut project = Project::new();
        initialize(&mut project, true);
        Editor::from_project(project)
    }

    fn media(editor: &mut Editor, kind: MediaKind, width: u32, height: u32, rate: &str) -> String {
        editor
            .apply(Command::AddMedia {
                item: NewMedia {
                    path: format!("{kind:?}-{width}-{height}-{rate}"),
                    kind,
                    width: Some(width),
                    height: Some(height),
                    frame_rate_fraction: Some(rate.into()),
                    duration: Some(2.0),
                    name: "Fixture".into(),
                    frame_rate: None,
                    video_codec: None,
                    audio_codec: None,
                    has_audio: false,
                    audio_tracks: Vec::new(),
                    origin: None,
                    color_space: Default::default(),
                },
            })
            .unwrap()
            .created_id
            .unwrap()
    }

    fn insert(editor: &mut Editor, ids: Vec<String>) {
        editor
            .apply(Command::AddClipsInSequence {
                media_ids: ids,
                track_id: None,
                start: 0.0,
                ripple: false,
            })
            .unwrap();
    }

    #[test]
    fn library_audio_then_visual_matches_once_and_undo_restores_waiting() {
        let mut editor = editor();
        let sound = media(&mut editor, MediaKind::Audio, 0, 0, "");
        insert(&mut editor, vec![sound]);
        let original = editor.project().clone();
        let visual = media(&mut editor, MediaKind::Video, 1080, 1920, "30000/1001");
        assert_eq!(read(editor.project()).state, State::Waiting);
        let before = editor.project().clone();
        insert(&mut editor, vec![visual]);
        let matched = editor.project().clone();
        assert_eq!(
            (matched.active().video.width, matched.active().video.height),
            (1080, 1920)
        );
        assert_eq!(
            (
                matched.active().video.rate_num,
                matched.active().video.rate_den
            ),
            (30000, 1001)
        );
        assert_eq!(read(&matched).state, State::Matched);
        assert!(editor.undo());
        assert_eq!(editor.project(), &before);
        assert_eq!(
            editor.project().active().clips.len(),
            original.active().clips.len()
        );
        assert!(editor.redo());
        assert_eq!(editor.project(), &matched);
        let other = media(&mut editor, MediaKind::Video, 1920, 1080, "60/1");
        insert(&mut editor, vec![other]);
        assert_eq!(editor.project().active().video, matched.active().video);
    }

    #[test]
    fn manual_parameters_survive_later_insert_and_its_undo() {
        let mut editor = editor();
        let visual = media(&mut editor, MediaKind::Video, 1080, 1920, "30/1");
        let mut video = editor.project().active().video;
        video.width = 1280;
        video.height = 720;
        editor
            .apply(Command::SetTimelineVideo {
                timeline_id: "TL1".into(),
                video,
            })
            .unwrap();
        let manual = editor.project().clone();
        assert_eq!(read(&manual).state, State::UserSet);
        insert(&mut editor, vec![visual]);
        assert_eq!(editor.project().active().video, video);
        assert!(editor.undo());
        assert_eq!(editor.project(), &manual);
    }

    #[test]
    fn same_frame_and_colour_only_leave_matching_pending() {
        let mut editor = editor();
        let mut video = editor.project().active().video;
        editor
            .apply(Command::SetTimelineVideo {
                timeline_id: "TL1".into(),
                video,
            })
            .unwrap();
        assert_eq!(read(editor.project()).state, State::Waiting);
        video.canvas = [12, 34, 56, 255];
        editor
            .apply(Command::SetTimelineVideo {
                timeline_id: "TL1".into(),
                video,
            })
            .unwrap();
        assert_eq!(read(editor.project()).state, State::Waiting);
    }

    #[test]
    fn images_keep_aspect_cap_and_do_not_upscale_small_images() {
        let defaults = VideoSettings::default();
        for (w, h, expected) in [
            (6000, 4000, (1920, 1280)),
            (300, 200, (300, 200)),
            (301, 201, (300, 200)),
            (3, 3, (2, 2)),
            (4000, 6000, (1280, 1920)),
            (6000, 4005, (1920, 1282)),
        ] {
            let item = MediaItem {
                kind: MediaKind::Image,
                width: Some(w),
                height: Some(h),
                ..Default::default()
            };
            let (video, _) = settings(&item, defaults).unwrap();
            assert_eq!((video.width, video.height), expected);
            assert_eq!(
                (video.rate_num, video.rate_den),
                (defaults.rate_num, defaults.rate_den)
            );
        }
    }

    #[test]
    fn legacy_and_disabled_projects_never_auto_match() {
        for mut project in [Project::new(), Project::new()] {
            let legacy = !project.extra.contains_key(FIELD);
            if !legacy {
                initialize(&mut project, false);
            }
            assert!(before(&project).is_none());
        }
        let mut project = Project::new();
        initialize(&mut project, false);
        assert_eq!(read(&project).state, State::Disabled);
        assert!(before(&project).is_none());
    }
}
