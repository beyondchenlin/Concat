// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 BK CAT contributors

use std::collections::HashSet;

use crate::commands::{ClipMove, Command, ExactMediaKind};
use crate::model::{ClipKind, MediaItem, MediaKind, TextStyle};
use crate::{CommandError, Editor, GuardError};

// Core metadata fixtures deliberately do not claim real-file validation.
// concat-host::agent_media tests exercise the actual native file probe.
fn seeded() -> Editor {
    let mut project = Editor::new().project().clone();
    for (id, kind) in [("m1", MediaKind::Video), ("m2", MediaKind::Audio)] {
        project.media.push(MediaItem {
            id: id.into(),
            path: format!("/test-fixture/{id}"),
            name: id.into(),
            kind,
            duration: Some(12.0),
            ..MediaItem::default()
        });
    }
    Editor::from_project(project)
}

fn media(track: &str, kind: ExactMediaKind, start: f64, from: f64, to: f64) -> Command {
    Command::AddMediaClipExact {
        media_id: match kind {
            ExactMediaKind::Video => "m1",
            ExactMediaKind::Audio => "m2",
        }
        .into(),
        track_id: track.into(),
        kind,
        start,
        source_start: from,
        source_end: to,
    }
}

fn text(start: f64, duration: Option<f64>) -> Command {
    Command::AddTextClip {
        track_id: Some("T3".into()),
        above: false,
        start,
        duration,
        style: Some(TextStyle {
            content: "独立字幕".into(),
            ..TextStyle::default()
        }),
        offset_y: None,
    }
}

#[test]
fn exact_video_audio_and_captions_are_one_native_undo_step() {
    let mut editor = seeded();
    let before = editor.project().clone();
    let plan = Command::Batch {
        commands: vec![
            media("T1", ExactMediaKind::Video, 4.25, 1.5, 5.0),
            media("T2", ExactMediaKind::Audio, 4.25, 2.0, 5.5),
            text(4.25, Some(1.5)),
            text(5.75, Some(2.0)),
        ],
    };
    editor.apply_guarded(plan, &HashSet::new()).unwrap();
    let after = editor.project().clone();
    let clips = &after.active().clips;
    assert_eq!(clips.len(), 4);
    assert_eq!(
        (
            clips[0].start,
            clips[0].source_start,
            clips[0].duration,
            clips[0].speed
        ),
        (4.25, 1.5, 3.5, 1.0)
    );
    assert_eq!(
        (
            clips[1].start,
            clips[1].source_start,
            clips[1].duration,
            clips[1].kind
        ),
        (4.25, 2.0, 3.5, ClipKind::Audio)
    );
    assert_eq!((clips[2].start, clips[3].start), (4.25, 5.75));
    assert!(editor.undo());
    assert_eq!(editor.project(), &before);
    assert!(!editor.can_undo());
    assert!(editor.redo());
    assert_eq!(editor.project(), &after);
    assert!(!editor.can_redo());
}

#[test]
fn exact_overlap_refuses_instead_of_moving_and_rolls_back_the_whole_batch() {
    let mut editor = seeded();
    editor
        .apply_guarded(
            media("T1", ExactMediaKind::Video, 2.0, 0.0, 4.0),
            &HashSet::new(),
        )
        .unwrap();
    let before = editor.project().clone();
    let error = editor
        .apply_guarded(
            Command::Batch {
                commands: vec![
                    text(0.0, Some(1.0)),
                    media("T1", ExactMediaKind::Video, 3.0, 1.0, 3.0),
                ],
            },
            &HashSet::new(),
        )
        .unwrap_err();
    assert_eq!(error, GuardError::Command(CommandError::ExactOverlap));
    assert_eq!(editor.project(), &before);
    let id = editor
        .apply_guarded(text(0.0, Some(1.0)), &HashSet::new())
        .unwrap()
        .created_id;
    let mut clean = Editor::from_project(before.clone());
    assert_eq!(
        id,
        clean
            .apply_guarded(text(0.0, Some(1.0)), &HashSet::new())
            .unwrap()
            .created_id
    );
    assert!(editor.undo());
    assert_eq!(editor.project(), &before);
}

#[test]
fn exact_source_ranges_never_clamp_or_use_a_default_duration() {
    let plans = [
        media("T1", ExactMediaKind::Video, -1.0, 0.0, 1.0),
        media("T1", ExactMediaKind::Video, 0.0, -1.0, 1.0),
        media("T1", ExactMediaKind::Video, 0.0, 2.0, 2.0),
        media("T1", ExactMediaKind::Video, 0.0, 2.0, 2.001),
        media("T1", ExactMediaKind::Video, 0.0, 2.0, 13.0),
        media("T1", ExactMediaKind::Video, 360_000.0, 0.0, 1.0),
        media("T1", ExactMediaKind::Video, f64::NAN, 0.0, 1.0),
        media("T1", ExactMediaKind::Video, 0.0, 0.0, f64::INFINITY),
    ];
    for plan in plans {
        let mut editor = seeded();
        let before = editor.project().clone();
        assert!(editor.apply_guarded(plan, &HashSet::new()).is_err());
        assert_eq!(editor.project(), &before);
        assert!(!editor.can_undo());
    }
    for duration in [None, Some(0.0), Some(f64::NAN)] {
        let mut project = seeded().project().clone();
        project.media[0].duration = duration;
        let mut editor = Editor::from_project(project);
        assert!(
            editor
                .apply_guarded(
                    media("T1", ExactMediaKind::Video, 0.0, 0.0, 1.0),
                    &HashSet::new()
                )
                .is_err()
        );
    }
}

#[test]
fn exact_kind_missing_ids_placeholders_and_mixed_lanes_are_refused() {
    let mut editor = seeded();
    let before = editor.project().clone();
    for (media_id, track_id, kind) in [
        ("m2", "T1", ExactMediaKind::Video),
        ("missing", "T1", ExactMediaKind::Video),
        ("m1", "missing", ExactMediaKind::Video),
    ] {
        assert!(
            editor
                .apply_guarded(
                    Command::AddMediaClipExact {
                        media_id: media_id.into(),
                        track_id: track_id.into(),
                        kind,
                        start: 0.0,
                        source_start: 0.0,
                        source_end: 1.0,
                    },
                    &HashSet::new()
                )
                .is_err()
        );
        assert_eq!(editor.project(), &before);
    }
    editor
        .apply_guarded(text(0.0, Some(1.0)), &HashSet::new())
        .unwrap();
    let with_text = editor.project().clone();
    assert_eq!(
        editor.apply_guarded(
            media("T3", ExactMediaKind::Video, 4.0, 0.0, 1.0),
            &HashSet::new()
        ),
        Err(GuardError::Command(CommandError::ExactKind))
    );
    assert_eq!(editor.project(), &with_text);
    let mut placeholder = before;
    placeholder.media[0].placeholder = true;
    assert!(
        Editor::from_project(placeholder)
            .apply_guarded(
                media("T1", ExactMediaKind::Video, 0.0, 0.0, 1.0),
                &HashSet::new()
            )
            .is_err()
    );
}

#[test]
fn exact_add_checks_current_track_locks_and_preserves_redo() {
    let mut editor = seeded();
    editor
        .apply_guarded(
            media("T1", ExactMediaKind::Video, 1.0, 0.0, 2.0),
            &HashSet::new(),
        )
        .unwrap();
    let after = editor.project().clone();
    editor.undo();
    let before = editor.project().clone();
    assert_eq!(
        editor.apply_guarded(
            Command::Batch {
                commands: vec![
                    text(0.0, Some(1.0)),
                    media("T1", ExactMediaKind::Video, 1.0, 0.0, 2.0),
                ]
            },
            &HashSet::from(["T1".into()])
        ),
        Err(GuardError::LockedTracks {
            track_ids: vec!["T1".into()]
        })
    );
    assert_eq!(editor.project(), &before);
    assert!(editor.can_redo());
    assert!(editor.redo());
    assert_eq!(editor.project(), &after);
}

#[test]
fn explicit_caption_times_refuse_overlap_missing_duration_and_clamps() {
    let mut editor = seeded();
    editor
        .apply_guarded(text(1.0, Some(2.0)), &HashSet::new())
        .unwrap();
    let before = editor.project().clone();
    for plan in [
        text(2.0, Some(1.0)),
        text(-1.0, Some(1.0)),
        text(5.0, None),
        text(5.0, Some(0.001)),
    ] {
        assert!(editor.apply_guarded(plan, &HashSet::new()).is_err());
        assert_eq!(editor.project(), &before);
    }
    editor
        .apply_guarded(text(3.0, Some(1.0)), &HashSet::new())
        .unwrap();
    assert_eq!(editor.project().active().clips[1].start, 3.0);
}

#[test]
fn manual_gap_search_commands_are_still_not_agent_exact_commands() {
    let mut editor = seeded();
    let before = editor.project().clone();
    for plan in [
        Command::AddClip {
            media_id: "m1".into(),
            track_id: "T1".into(),
            start: 0.0,
            ripple: false,
        },
        Command::AddClipAtFirstFree {
            media_id: "m1".into(),
            start: 0.0,
        },
        Command::AddClipsInSequence {
            media_ids: vec!["m1".into()],
            track_id: Some("T1".into()),
            start: 0.0,
            ripple: false,
        },
    ] {
        assert!(matches!(
            editor.apply_guarded(plan, &HashSet::new()),
            Err(GuardError::Unsupported { .. })
        ));
        assert_eq!(editor.project(), &before);
    }
}

#[test]
fn guarded_moves_refuse_manual_gap_search_and_commit_exact_groups_once() {
    let mut editor = seeded();
    let first = editor
        .apply_guarded(text(0.0, Some(1.0)), &HashSet::new())
        .unwrap()
        .created_id
        .unwrap();
    let second = editor
        .apply_guarded(text(3.0, Some(1.0)), &HashSet::new())
        .unwrap()
        .created_id
        .unwrap();
    let before = editor.project().clone();
    let at = |clip_id: &str, start| ClipMove {
        clip_id: clip_id.into(),
        start,
        track_id: "T3".into(),
    };
    for moves in [
        vec![at(&first, 3.25)],
        vec![at(&first, -1.0)],
        vec![at(&first, 360_000.0)],
        vec![at(&first, 10.0), at(&second, 10.0)],
    ] {
        let plan = Command::Batch {
            commands: vec![text(6.0, Some(1.0)), Command::MoveClips { moves }],
        };
        assert!(editor.apply_guarded(plan, &HashSet::new()).is_err());
        assert_eq!(
            editor.project(),
            &before,
            "refusal rolls back the earlier text too"
        );
    }
    let mut manual = Editor::from_project(before.clone());
    manual
        .apply(Command::MoveClips {
            moves: vec![at(&first, 3.25)],
        })
        .unwrap();
    assert_ne!(
        manual.project().active().clip(&first).unwrap().start,
        3.25,
        "the ordinary manual command still chooses a gap"
    );

    editor
        .apply_guarded(
            Command::MoveClips {
                moves: vec![at(&first, 1.25), at(&second, 4.25)],
            },
            &HashSet::new(),
        )
        .unwrap();
    let after = editor.project().clone();
    assert_eq!(after.active().clip(&first).unwrap().start, 1.25);
    assert_eq!(after.active().clip(&second).unwrap().start, 4.25);
    assert!(editor.undo());
    assert_eq!(editor.project(), &before);
    assert!(editor.redo());
    assert_eq!(editor.project(), &after);
}

#[test]
fn exact_media_command_round_trips_through_the_real_wire_shape() {
    let value = serde_json::json!({"op":"addMediaClipExact","mediaId":"m1","trackId":"T1","kind":"video","start":4.25,"sourceStart":1.5,"sourceEnd":5.0});
    let command: Command = serde_json::from_value(value.clone()).unwrap();
    assert_eq!(serde_json::to_value(command).unwrap(), value);
    let mut wrong = value;
    wrong["kind"] = "image".into();
    assert!(serde_json::from_value::<Command>(wrong).is_err());
}
