// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 BK CAT contributors

use std::collections::HashSet;

use super::{GuardError, MAX_BATCH_DEPTH, MAX_BATCH_OPERATIONS};
use crate::Editor;
use crate::commands::{ClipMove, ClipPatch, Command};
use crate::model::TextStyle;

fn locks(ids: &[&str]) -> HashSet<String> {
    ids.iter().map(|id| (*id).to_owned()).collect()
}

fn text(track: &str, start: f64, content: &str) -> Command {
    Command::AddTextClip {
        track_id: Some(track.to_owned()),
        above: true,
        start,
        style: Some(TextStyle {
            content: content.to_owned(),
            ..TextStyle::default()
        }),
        duration: Some(2.0),
        offset_y: None,
    }
}

fn batch(commands: Vec<Command>) -> Command {
    Command::Batch { commands }
}

fn seed() -> (Editor, String) {
    let mut editor = Editor::new();
    let id = editor
        .apply(text("T1", 0.0, "original"))
        .unwrap()
        .created_id
        .unwrap();
    // An opened project begins with no undo steps. The capture gives the test
    // that state while preserving the actual native project and minted ids.
    (editor.snapshot(), id)
}

fn is_locked(error: GuardError, expected: &[&str]) {
    assert_eq!(
        error,
        GuardError::LockedTracks {
            track_ids: expected.iter().map(|id| (*id).to_owned()).collect(),
        }
    );
}

#[test]
fn two_subtitles_share_one_native_undo_and_redo_step() {
    let mut editor = Editor::new();
    let before = editor.project().clone();
    let outcome = editor
        .apply_guarded(
            batch(vec![
                text("T2", 0.0, "第一段字幕"),
                text("T2", 3.0, "第二段字幕"),
            ]),
            &locks(&[]),
        )
        .unwrap();
    assert!(outcome.applied);
    let after = editor.project().clone();
    assert_eq!(after.active().clips.len(), 2);
    assert_eq!(
        after.active().clips[0].text.as_ref().unwrap().content,
        "第一段字幕"
    );
    assert_eq!(
        after.active().clips[1].text.as_ref().unwrap().content,
        "第二段字幕"
    );
    assert!(editor.undo());
    assert_eq!(editor.project(), &before);
    assert!(!editor.can_undo());
    assert!(editor.redo());
    assert_eq!(editor.project(), &after);
    assert!(!editor.can_redo());
}

#[test]
fn mixed_locked_batch_is_wholly_rejected_without_consuming_ids() {
    let mut editor = Editor::new();
    let before = editor.project().clone();
    let held = locks(&["T2"]);
    is_locked(
        editor
            .apply_guarded(
                batch(vec![
                    text("T1", 0.0, "staged only"),
                    text("T2", 3.0, "locked"),
                ]),
                &held,
            )
            .unwrap_err(),
        &["T2"],
    );
    assert_eq!(editor.project(), &before);
    assert!(!editor.can_undo());
    assert!(!editor.can_redo());
    assert_eq!(held, locks(&["T2"]), "the editor cannot unlock the window");
    let next = editor
        .apply_guarded(text("T1", 0.0, "accepted"), &locks(&[]))
        .unwrap();
    let clean = Editor::new()
        .apply_guarded(text("T1", 0.0, "accepted"), &locks(&[]))
        .unwrap();
    assert_eq!(
        next.created_id, clean.created_id,
        "rejection does not advance the mint"
    );
}

#[test]
fn lock_added_after_planning_is_checked_at_execution_and_unlock_retries() {
    let mut editor = Editor::new();
    let plan = batch(vec![text("T2", 0.0, "one"), text("T2", 3.0, "two")]);
    let before = editor.project().clone();
    // The plan was made while all tracks were unlocked. The host passes the
    // new lock set from the editing thread when the queued call actually runs.
    is_locked(
        editor
            .apply_guarded(plan.clone(), &locks(&["T2"]))
            .unwrap_err(),
        &["T2"],
    );
    assert_eq!(editor.project(), &before);
    assert!(!editor.can_undo());
    assert!(editor.apply_guarded(plan, &locks(&[])).unwrap().applied);
    assert_eq!(editor.project().active().clips.len(), 2);
    assert!(editor.undo());
    assert_eq!(editor.project(), &before);
}

#[test]
fn an_already_locked_target_rejects_even_a_single_add() {
    let mut editor = Editor::new();
    is_locked(
        editor
            .apply_guarded(text("T1", 0.0, "blocked"), &locks(&["T1"]))
            .unwrap_err(),
        &["T1"],
    );
    assert!(editor.project().active().clips.is_empty());
    assert!(!editor.can_undo());
}

#[test]
fn a_cross_track_move_checks_both_source_and_destination() {
    let (mut editor, id) = seed();
    let before = editor.project().clone();
    let move_it = Command::MoveClips {
        moves: vec![ClipMove {
            clip_id: id.clone(),
            start: 3.0,
            track_id: "T2".into(),
        }],
    };
    for held in [&["T1"][..], &["T2"][..], &["T1", "T2"][..]] {
        is_locked(
            editor
                .apply_guarded(move_it.clone(), &locks(held))
                .unwrap_err(),
            held,
        );
        assert_eq!(editor.project(), &before);
        assert!(!editor.can_undo());
    }
    assert!(editor.apply_guarded(move_it, &locks(&[])).unwrap().applied);
    let moved = editor.project().active().clip(&id).unwrap();
    assert_eq!(moved.track_id, "T2");
    assert_eq!(moved.start, 3.0);
    assert!(editor.undo());
    assert_eq!(editor.project(), &before);
}

#[test]
fn a_no_op_patch_still_touches_a_locked_track() {
    let (mut editor, id) = seed();
    let before = editor.project().clone();
    let patch = Command::UpdateClip {
        clip_id: id,
        patch: ClipPatch {
            name: Some("original".into()),
            ..ClipPatch::default()
        },
    };
    is_locked(
        editor
            .apply_guarded(patch.clone(), &locks(&["T1"]))
            .unwrap_err(),
        &["T1"],
    );
    assert_eq!(editor.project(), &before);
    assert!(!editor.apply_guarded(patch, &locks(&[])).unwrap().applied);
    assert!(!editor.can_undo());
}

#[test]
fn a_no_op_move_still_touches_a_locked_track() {
    let (mut editor, id) = seed();
    let unchanged = Command::MoveClips {
        moves: vec![ClipMove {
            clip_id: id,
            start: 0.0,
            track_id: "T1".into(),
        }],
    };
    is_locked(
        editor
            .apply_guarded(unchanged, &locks(&["T1"]))
            .unwrap_err(),
        &["T1"],
    );
    assert!(!editor.can_undo());
}

#[test]
fn nested_batches_cannot_hide_a_locked_intermediate_edit() {
    let (mut editor, id) = seed();
    let before = editor.project().clone();
    let edit = |name: &str| Command::UpdateClip {
        clip_id: id.clone(),
        patch: ClipPatch {
            name: Some(name.into()),
            ..ClipPatch::default()
        },
    };
    let commands = batch(vec![
        text("T2", 5.0, "also must roll back"),
        batch(vec![edit("temporary"), edit("original")]),
    ]);
    is_locked(
        editor.apply_guarded(commands, &locks(&["T1"])).unwrap_err(),
        &["T1"],
    );
    assert_eq!(editor.project(), &before);
    assert!(!editor.can_undo());
}

#[test]
fn unsupported_commands_roll_back_earlier_staged_commands() {
    let mut editor = Editor::new();
    let before = editor.project().clone();
    let error = editor
        .apply_guarded(
            batch(vec![text("T1", 0.0, "staged"), Command::AddTimeline]),
            &locks(&[]),
        )
        .unwrap_err();
    assert!(matches!(error, GuardError::Unsupported { .. }));
    assert_eq!(editor.project(), &before);
    assert!(!editor.can_undo());
    assert_eq!(
        editor
            .apply(text("T1", 0.0, "next"))
            .unwrap()
            .created_id
            .as_deref(),
        Some("c1")
    );
}

#[test]
fn unknown_objects_are_rejected_instead_of_tolerated_as_success() {
    let (mut editor, id) = seed();
    let before = editor.project().clone();
    let commands = [
        Command::UpdateClip {
            clip_id: "missing".into(),
            patch: ClipPatch::default(),
        },
        Command::MoveClips {
            moves: vec![ClipMove {
                clip_id: "missing".into(),
                start: 0.0,
                track_id: "T1".into(),
            }],
        },
        Command::RemoveClips {
            clip_ids: vec![id, "missing".into()],
            ripple: false,
        },
    ];
    for command in commands {
        assert!(matches!(
            editor.apply_guarded(command, &locks(&[])),
            Err(GuardError::ClipGone { .. })
        ));
        assert_eq!(editor.project(), &before);
        assert!(!editor.can_undo());
    }
}

#[test]
fn automatic_or_vanished_tracks_do_not_fall_back_to_an_unchecked_lane() {
    let (mut editor, id) = seed();
    let before = editor.project().clone();
    let mut automatic = text("T1", 0.0, "automatic");
    if let Command::AddTextClip { track_id, .. } = &mut automatic {
        *track_id = None;
    }
    assert!(matches!(
        editor.apply_guarded(automatic, &locks(&[])),
        Err(GuardError::Unsupported { .. })
    ));
    for command in [
        text("missing", 0.0, "missing"),
        Command::MoveClips {
            moves: vec![ClipMove {
                clip_id: id,
                start: 4.0,
                track_id: "missing".into(),
            }],
        },
    ] {
        assert!(matches!(
            editor.apply_guarded(command, &locks(&[])),
            Err(GuardError::TrackGone { .. })
        ));
    }
    assert_eq!(editor.project(), &before);
    assert!(!editor.can_undo());
}

#[test]
fn deletion_is_guarded_and_ripple_remains_explicitly_unsupported() {
    let (mut editor, id) = seed();
    let before = editor.project().clone();
    let plain = Command::RemoveClips {
        clip_ids: vec![id.clone()],
        ripple: false,
    };
    is_locked(
        editor
            .apply_guarded(plain.clone(), &locks(&["T1"]))
            .unwrap_err(),
        &["T1"],
    );
    let ripple = Command::RemoveClips {
        clip_ids: vec![id],
        ripple: true,
    };
    assert!(matches!(
        editor.apply_guarded(ripple, &locks(&[])),
        Err(GuardError::Unsupported { .. })
    ));
    assert_eq!(editor.project(), &before);
    assert!(editor.apply_guarded(plain, &locks(&[])).unwrap().applied);
    assert!(editor.project().active().clips.is_empty());
    assert!(editor.undo());
    assert_eq!(editor.project(), &before);
}

#[test]
fn native_validation_failure_cannot_commit_an_earlier_batch_member() {
    let mut editor = Editor::new();
    let before = editor.project().clone();
    let command = batch(vec![text("T1", 0.0, "staged"), text("T1", f64::NAN, "bad")]);
    assert_eq!(
        editor.apply_guarded(command, &locks(&[])),
        Err(GuardError::Command(crate::CommandError::NotANumber))
    );
    assert_eq!(editor.project(), &before);
    assert!(!editor.can_undo());
    assert_eq!(
        editor
            .apply(text("T1", 0.0, "next"))
            .unwrap()
            .created_id
            .as_deref(),
        Some("c1")
    );
}

#[test]
fn rejected_batches_preserve_redo_and_the_live_mint() {
    let mut editor = Editor::new();
    editor.apply(text("T1", 0.0, "manual")).unwrap();
    let after = editor.project().clone();
    assert!(editor.undo());
    assert!(editor.can_redo());
    is_locked(
        editor
            .apply_guarded(
                batch(vec![text("T1", 5.0, "staged"), text("T2", 8.0, "locked")]),
                &locks(&["T2"]),
            )
            .unwrap_err(),
        &["T2"],
    );
    assert!(editor.can_redo());
    assert!(!editor.can_undo());
    assert!(editor.redo());
    assert_eq!(editor.project(), &after);
    assert_eq!(
        editor
            .apply(text("T1", 5.0, "next"))
            .unwrap()
            .created_id
            .as_deref(),
        Some("c2")
    );
}

#[test]
fn manual_and_automation_edits_continue_in_one_history() {
    let mut editor = Editor::new();
    let manual_id = editor
        .apply(text("T1", 0.0, "manual"))
        .unwrap()
        .created_id
        .unwrap();
    let manual = editor.project().clone();
    editor
        .apply_guarded(
            batch(vec![text("T2", 0.0, "one"), text("T2", 3.0, "two")]),
            &locks(&[]),
        )
        .unwrap();
    let captions = editor.project().clone();
    editor
        .apply(Command::UpdateClip {
            clip_id: manual_id.clone(),
            patch: ClipPatch {
                name: Some("manual adjustment".into()),
                ..ClipPatch::default()
            },
        })
        .unwrap();
    let adjusted = editor.project().clone();
    editor
        .apply_guarded(
            Command::UpdateClip {
                clip_id: manual_id,
                patch: ClipPatch {
                    name: Some("conversation continues".into()),
                    ..ClipPatch::default()
                },
            },
            &locks(&[]),
        )
        .unwrap();
    assert!(editor.undo());
    assert_eq!(editor.project(), &adjusted);
    assert!(editor.undo());
    assert_eq!(editor.project(), &captions);
    assert!(editor.undo());
    assert_eq!(editor.project(), &manual);
    assert!(editor.redo());
    assert_eq!(editor.project(), &captions);
}

#[test]
fn nesting_and_operation_limits_reject_without_history_changes() {
    let mut editor = Editor::new();
    let before = editor.project().clone();
    let mut deep = text("T1", 0.0, "deep");
    for _ in 0..=MAX_BATCH_DEPTH {
        deep = batch(vec![deep]);
    }
    assert_eq!(
        editor.apply_guarded(deep, &locks(&[])),
        Err(GuardError::BatchLimit)
    );
    let wide = batch(
        (0..MAX_BATCH_OPERATIONS)
            .map(|_| Command::MoveClips { moves: vec![] })
            .collect(),
    );
    assert_eq!(
        editor.apply_guarded(wide, &locks(&[])),
        Err(GuardError::BatchLimit)
    );
    assert_eq!(editor.project(), &before);
    assert!(!editor.can_undo());
}

#[test]
fn duplicate_clip_targets_and_invalid_active_identity_fail_closed() {
    let (mut editor, id) = seed();
    let before = editor.project().clone();
    let duplicate = Command::RemoveClips {
        clip_ids: vec![id.clone(), id],
        ripple: false,
    };
    assert!(matches!(
        editor.apply_guarded(duplicate, &locks(&[])),
        Err(GuardError::DuplicateClip { .. })
    ));
    assert_eq!(editor.project(), &before);
    editor.project_mut().active_timeline_id = "missing".into();
    assert_eq!(
        editor.apply_guarded(text("T1", 0.0, "bad binding"), &locks(&[])),
        Err(GuardError::InvalidProject)
    );
    assert!(!editor.can_undo());
}

#[test]
fn an_unlocked_round_trip_no_op_does_not_consume_history() {
    let (mut editor, id) = seed();
    let before = editor.project().clone();
    let patch = |name: &str| Command::UpdateClip {
        clip_id: id.clone(),
        patch: ClipPatch {
            name: Some(name.into()),
            ..ClipPatch::default()
        },
    };
    assert!(
        !editor
            .apply_guarded(
                batch(vec![patch("temporary"), patch("original")]),
                &locks(&[])
            )
            .unwrap()
            .applied
    );
    assert_eq!(editor.project(), &before);
    assert!(!editor.can_undo());
}

#[test]
fn snapshot_is_detached_from_live_history_and_project_mutation() {
    let (mut editor, _) = seed();
    editor.apply(text("T2", 3.0, "live edit")).unwrap();
    let before = editor.project().clone();
    let mut capture = editor.snapshot();
    assert_eq!(capture.project(), &before);
    assert!(!capture.can_undo());
    capture.apply(text("T3", 8.0, "not merged")).unwrap();
    assert_eq!(editor.project(), &before);
    assert!(editor.can_undo());
}
