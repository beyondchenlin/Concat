// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 BK CAT contributors

use std::cell::Cell;
use std::collections::HashSet;
use std::path::Path;

use concat_project::DocumentSettings;
use concat_project::commands::{Command, ExactMediaKind};
use concat_project::model::{ClipKind, TextStyle};

use super::ExactMediaChecks;
use crate::Session;

fn settings() -> DocumentSettings {
    DocumentSettings {
        name: "Exact native source test".into(),
        width: 640,
        height: 360,
        rate_num: 30,
        rate_den: 1,
        canvas: [0, 0, 0, 255],
    }
}

// A real, well-formed mono PCM wave file. No probe replacement:
// every test calling ExactMediaChecks::probe goes through the media library.
fn wave(path: &Path, seconds: u32) {
    let samples = 8_000 * seconds;
    let data_len = samples * 2;
    let mut bytes = Vec::with_capacity((44 + data_len) as usize);
    bytes.extend_from_slice(b"RIFF");
    bytes.extend_from_slice(&(36 + data_len).to_le_bytes());
    bytes.extend_from_slice(b"WAVEfmt ");
    bytes.extend_from_slice(&16u32.to_le_bytes());
    bytes.extend_from_slice(&1u16.to_le_bytes());
    bytes.extend_from_slice(&1u16.to_le_bytes());
    bytes.extend_from_slice(&8_000u32.to_le_bytes());
    bytes.extend_from_slice(&16_000u32.to_le_bytes());
    bytes.extend_from_slice(&2u16.to_le_bytes());
    bytes.extend_from_slice(&16u16.to_le_bytes());
    bytes.extend_from_slice(b"data");
    bytes.extend_from_slice(&data_len.to_le_bytes());
    for index in 0..samples {
        let sample = if index % 40 < 20 { 2_000i16 } else { -2_000i16 };
        bytes.extend_from_slice(&sample.to_le_bytes());
    }
    std::fs::write(path, bytes).unwrap();
}

fn fixture() -> (tempfile::TempDir, Session, Command) {
    let scratch = tempfile::tempdir().unwrap();
    let path = scratch.path().join("source.wav");
    wave(&path, 2);
    let source = crate::media::probe(path.to_str().unwrap()).expect("real wave probe");
    let mut session =
        Session::open(scratch.path().join("project").to_str().unwrap(), settings()).unwrap();
    let media_id = session
        .apply(Command::AddMedia {
            item: source.to_new_media(),
        })
        .unwrap()
        .created_id
        .unwrap();
    let command = Command::AddMediaClipExact {
        media_id,
        track_id: "T1".into(),
        kind: ExactMediaKind::Audio,
        start: 3.25,
        source_start: 0.5,
        source_end: 1.75,
    };
    (scratch, session, command)
}

#[test]
fn real_source_exact_edit_uses_one_history_and_survives_native_save_reopen() {
    let (_scratch, mut session, command) = fixture();
    let text = Command::AddTextClip {
        track_id: Some("T2".into()),
        above: false,
        start: 3.25,
        duration: Some(1.25),
        style: Some(TextStyle {
            content: "可编辑字幕".into(),
            ..TextStyle::default()
        }),
        offset_y: None,
    };
    let plan = Command::Batch {
        commands: vec![command, text],
    };
    let before = session.project().clone();
    let proof = ExactMediaChecks::probe(&before, &plan, || Ok(())).unwrap();
    let id = session.session_id();
    let revision = session.revision();
    proof.recheck(session.project()).unwrap();
    session
        .apply_guarded(plan, &HashSet::new(), id, revision)
        .unwrap();
    let after = session.project().clone();
    let audio = &after.active().clips[0];
    assert_eq!(
        (audio.start, audio.source_start, audio.duration, audio.kind),
        (3.25, 0.5, 1.25, ClipKind::Audio)
    );
    assert_eq!(
        after.active().clips[1].text.as_ref().unwrap().content,
        "可编辑字幕"
    );
    session.undo();
    assert_eq!(session.project(), &before);
    session.redo();
    assert_eq!(session.project(), &after);
    session.save(None).expect("the real native durable save");
    let reopened = Session::open(session.path(), session.settings()).unwrap();
    assert_eq!(reopened.project(), &after);
    assert_eq!(reopened.project_identity(), session.project_identity());
    assert_ne!(reopened.session_id(), id);
    assert!(reopened.check_revision(id, session.revision()).is_err());
}

#[test]
fn probe_cannot_authorize_an_old_session_revision_or_a_newly_locked_lane() {
    let (_scratch, mut session, command) = fixture();
    let proof = ExactMediaChecks::probe(session.project(), &command, || Ok(())).unwrap();
    let id = session.session_id();
    let revision = session.revision();
    let before = session.project().clone();
    assert!(
        session
            .apply_guarded(command.clone(), &HashSet::from(["T1".into()]), id, revision)
            .is_err()
    );
    assert_eq!(session.project(), &before);
    assert_eq!(session.revision(), revision);
    session.apply(Command::AddTrack { bottom: false }).unwrap();
    session.undo();
    assert_eq!(session.project(), &before);
    proof.recheck(session.project()).unwrap();
    assert!(
        session
            .apply_guarded(command, &HashSet::new(), id, revision)
            .is_err()
    );
    assert_eq!(session.project(), &before);
}

#[test]
fn real_probe_rejects_counterfeit_imported_duration_and_kind() {
    let (_scratch, session, command) = fixture();
    let mut stale = session.project().clone();
    stale.media[0].duration = Some(9.0);
    assert!(ExactMediaChecks::probe(&stale, &command, || Ok(())).is_err());
    stale.media[0].duration = Some(2.0);
    stale.media[0].kind = concat_project::model::MediaKind::Video;
    let mut wrong_kind = command;
    if let Command::AddMediaClipExact { kind, .. } = &mut wrong_kind {
        *kind = ExactMediaKind::Video;
    }
    assert!(ExactMediaChecks::probe(&stale, &wrong_kind, || Ok(())).is_err());
}

#[test]
fn a_same_path_same_duration_file_replacement_invalidates_native_proof() {
    let (scratch, session, command) = fixture();
    let proof = ExactMediaChecks::probe(session.project(), &command, || Ok(())).unwrap();
    let replacement = scratch.path().join("replacement.wav");
    wave(&replacement, 2);
    std::fs::remove_file(scratch.path().join("source.wav")).unwrap();
    std::fs::rename(replacement, scratch.path().join("source.wav")).unwrap();
    assert!(proof.recheck(session.project()).is_err());
    assert!(session.project().active().clips.is_empty());
}

#[test]
fn removal_and_in_place_changes_are_refused_before_the_batch() {
    let (scratch, session, command) = fixture();
    let proof = ExactMediaChecks::probe(session.project(), &command, || Ok(())).unwrap();
    wave(&scratch.path().join("source.wav"), 3);
    assert!(proof.recheck(session.project()).is_err());
    std::fs::remove_file(scratch.path().join("source.wav")).unwrap();
    assert!(ExactMediaChecks::probe(session.project(), &command, || Ok(())).is_err());
    assert!(session.project().active().clips.is_empty());
}

#[test]
fn stop_during_a_real_probe_refuses_the_late_admission() {
    let (_scratch, session, command) = fixture();
    let calls = Cell::new(0);
    let proof = ExactMediaChecks::probe(session.project(), &command, || {
        calls.set(calls.get() + 1);
        if calls.get() == 1 {
            Ok(())
        } else {
            Err("stopped during source probe".into())
        }
    });
    assert!(proof.is_err());
    assert_eq!(calls.get(), 2);
    assert!(session.project().active().clips.is_empty());
}

#[test]
fn native_proof_cannot_be_reused_after_the_imported_item_changes() {
    let (_scratch, session, command) = fixture();
    let proof = ExactMediaChecks::probe(session.project(), &command, || Ok(())).unwrap();
    let mut changed = session.project().clone();
    changed.media[0].path = "/some-other-file.wav".into();
    assert!(proof.recheck(&changed).is_err());
    let mut ambiguous = session.project().clone();
    ambiguous.media.push(ambiguous.media[0].clone());
    assert!(proof.recheck(&ambiguous).is_err());
}
