// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 Jareer Abdullah

//! A manual source import is one native transaction. Probing happens outside
//! the editor; source order, lock checks, insertion and format matching do not.

use std::collections::HashSet;

use super::*;
use crate::commands::NewMedia;
use crate::model::MediaKind;

impl Editor {
    /// Imports a complete, already-probed selection. Only a new draft's first
    /// import asks for automatic placement. Later imports only populate the bin.
    /// An import cannot grant an agent permission or admit another guarded op.
    pub fn import_media(
        &mut self,
        items: Vec<NewMedia>,
        place_initial: bool,
        locked_tracks: &HashSet<String>,
    ) -> Result<Outcome, GuardError> {
        let before = self.project.clone();
        let place = place_initial
            && before.media.is_empty()
            && before
                .timelines
                .iter()
                .all(|timeline| timeline.clips.is_empty());
        let visual = items.iter().any(|item| item.kind != MediaKind::Audio);
        let audio = items.iter().any(|item| item.kind == MediaKind::Audio);
        if place {
            let touched = before
                .active()
                .tracks
                .iter()
                .enumerate()
                .filter(|(index, _)| (*index == 0 && visual) || (*index == 1 && audio))
                .map(|(_, track)| track.id.clone())
                .filter(|id| locked_tracks.contains(id))
                .collect::<Vec<_>>();
            if !touched.is_empty() {
                return Err(GuardError::LockedTracks { track_ids: touched });
            }
        }
        let mut staged = self.snapshot();
        let mut visual_ids = Vec::new();
        let mut audio_ids = Vec::new();
        let mut combined = Outcome::default();
        for item in items {
            let kind = item.kind;
            let outcome = apply(
                &mut staged.project,
                &mut staged.mint,
                Command::AddMedia { item },
            )?;
            if let Some(id) = outcome.created_id.clone() {
                if kind == MediaKind::Audio {
                    audio_ids.push(id);
                } else {
                    visual_ids.push(id);
                }
            }
            combined.applied |= outcome.applied;
            if outcome.created_id.is_some() {
                combined.created_id = outcome.created_id;
            }
        }
        if place && combined.applied {
            // Keep the lowest picture lane available even for an audio-only
            // first selection. Separate sound files never duplicate a video's
            // existing embedded sound or ripple the picture sequence.
            let wanted = if audio { 2 } else { 1 };
            while staged.project.active().tracks.len() < wanted {
                apply(
                    &mut staged.project,
                    &mut staged.mint,
                    Command::AddTrack { bottom: false },
                )?;
            }
            for (ids, index) in [(visual_ids, 0), (audio_ids, 1)] {
                if ids.is_empty() {
                    continue;
                }
                let track_id = staged.project.active().tracks[index].id.clone();
                let outcome = apply(
                    &mut staged.project,
                    &mut staged.mint,
                    Command::AddClipsInSequence {
                        media_ids: ids,
                        track_id: Some(track_id),
                        start: 0.0,
                        ripple: false,
                    },
                )?;
                combined.applied |= outcome.applied;
            }
        }
        if !combined.applied {
            return Ok(combined);
        }
        staged.tidy_touched(&before);
        staged.follow_first_hdr(&before);
        self.project = staged.project;
        self.mint = staged.mint;
        self.record_edit(before, None);
        Ok(combined)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{initial_format, model::VideoSettings};

    fn item(path: &str, kind: MediaKind) -> NewMedia {
        NewMedia {
            path: path.into(),
            name: path.into(),
            duration: Some(2.0),
            kind,
            width: Some(1080),
            height: Some(1920),
            frame_rate: Some(29.97),
            frame_rate_fraction: Some("30000/1001".into()),
            video_codec: None,
            audio_codec: None,
            has_audio: kind == MediaKind::Video,
            audio_tracks: Vec::new(),
            origin: None,
            color_space: Default::default(),
        }
    }

    fn draft() -> Editor {
        let mut project = Project::new();
        initial_format::initialize(&mut project, true);
        Editor::from_project(project)
    }

    #[test]
    fn selection_order_is_preserved_with_separate_picture_and_audio_lanes() {
        let mut editor = draft();
        editor
            .import_media(
                vec![
                    item("sound-a", MediaKind::Audio),
                    item("picture-a", MediaKind::Video),
                    item("sound-b", MediaKind::Audio),
                    item("picture-b", MediaKind::Image),
                ],
                true,
                &HashSet::new(),
            )
            .unwrap();
        let timeline = editor.project().active();
        let names = |lane: &str| {
            timeline
                .clips
                .iter()
                .filter(|clip| clip.track_id == lane)
                .map(|clip| (clip.name.as_str(), clip.start))
                .collect::<Vec<_>>()
        };
        assert_eq!(names("T1"), vec![("picture-a", 0.0), ("picture-b", 2.0)]);
        assert_eq!(names("T2"), vec![("sound-a", 0.0), ("sound-b", 2.0)]);
        assert_eq!(timeline.clips.len(), 4, "embedded audio is not duplicated");
        assert_eq!((timeline.video.width, timeline.video.height), (1080, 1920));
        assert_eq!(
            (timeline.video.rate_num, timeline.video.rate_den),
            (30000, 1001)
        );
        assert!(editor.undo());
        assert!(editor.project().media.is_empty());
        assert!(editor.project().active().clips.is_empty());
        assert_eq!(
            initial_format::read(editor.project()).state,
            initial_format::State::Waiting
        );
        assert!(!editor.can_undo(), "one selection is exactly one undo step");
        assert!(editor.redo());
        assert_eq!(editor.project().active().clips.len(), 4);
    }

    #[test]
    fn late_lock_rejects_bin_format_clips_and_undo_before_native_commit() {
        let mut editor = draft();
        let before = editor.project().clone();
        let result = editor.import_media(
            vec![
                item("picture", MediaKind::Video),
                item("sound", MediaKind::Audio),
            ],
            true,
            &HashSet::from(["T2".into()]),
        );
        assert!(matches!(result, Err(GuardError::LockedTracks { .. })));
        assert_eq!(*editor.project(), before);
        assert!(!editor.can_undo());
        editor
            .import_media(
                vec![item("picture", MediaKind::Video)],
                true,
                &HashSet::new(),
            )
            .unwrap();
        assert_eq!(
            editor.project().media[0].id,
            "m5",
            "refusal does not consume identities after T1–T4"
        );
    }

    #[test]
    fn audio_only_waits_for_a_later_explicit_visual_insertion() {
        let mut editor = draft();
        editor
            .import_media(vec![item("sound", MediaKind::Audio)], true, &HashSet::new())
            .unwrap();
        assert_eq!(editor.project().active().clips[0].track_id, "T2");
        assert_eq!(
            initial_format::read(editor.project()).state,
            initial_format::State::Waiting
        );
        editor
            .import_media(
                vec![item("picture", MediaKind::Video)],
                false,
                &HashSet::new(),
            )
            .unwrap();
        assert_eq!(
            editor.project().active().clips.len(),
            1,
            "later import stays in the bin"
        );
        let id = editor.project().media[1].id.clone();
        editor
            .apply(Command::AddClipsInSequence {
                media_ids: vec![id],
                track_id: Some("T1".into()),
                start: 0.0,
                ripple: false,
            })
            .unwrap();
        assert_eq!(
            initial_format::read(editor.project()).state,
            initial_format::State::Matched
        );
    }

    #[test]
    fn a_manual_format_applied_while_sources_probe_survives_import_and_undo() {
        let mut editor = draft();
        let chosen = VideoSettings {
            width: 1280,
            height: 720,
            ..Default::default()
        };
        editor
            .apply(Command::SetTimelineVideo {
                timeline_id: "TL1".into(),
                video: chosen,
            })
            .unwrap();
        editor
            .import_media(
                vec![item("picture", MediaKind::Video)],
                true,
                &HashSet::new(),
            )
            .unwrap();
        assert_eq!(editor.project().active().video, chosen);
        assert!(editor.undo());
        assert_eq!(editor.project().active().video, chosen);
        assert_eq!(
            initial_format::read(editor.project()).state,
            initial_format::State::UserSet
        );
    }
}
