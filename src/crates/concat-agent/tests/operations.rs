// SPDX-License-Identifier: AGPL-3.0-or-later
//! Real disk journal tests. The crash subprocess is a test helper, not a
//! whole BK CUT application-process or Android lifecycle acceptance test.

use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use concat_agent::operations::{
    Admission, Invocation, JournalError, OperationJournal, RecoveryState, inspect_operations,
    inspect_recovery,
};
use concat_agent::{Phase, Reconciliation, RecoveryDecision, SessionOwner};
use serde_json::json;

static NEXT: AtomicU64 = AtomicU64::new(0);
struct Directory(PathBuf);
impl Directory {
    fn new() -> Self {
        let time = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let folder = std::env::temp_dir().join(format!(
            "bk-cat-operations-{}-{time}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&folder).unwrap();
        Self(folder)
    }
    fn path(&self) -> &Path {
        &self.0
    }
}
impl Drop for Directory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn approve(owner: &SessionOwner) {
    owner
        .confirm_recovery(
            &owner.token(),
            Reconciliation {
                project_identity: "isolated-native-project".into(),
                project_revision: "1".into(),
                session_revision: "1".into(),
            },
            RecoveryDecision::DiscardInterrupted,
        )
        .unwrap();
}

fn invocation() -> Invocation {
    Invocation {
        conversation_id: 12,
        task_id: 34,
        call_id: "call/child".into(),
    }
}

fn request(path: &str) -> serde_json::Value {
    json!({"method":"edit.applyGuarded","path":path,
        "expectedSession":7,"expectedRevision":1,
        "command":{"type":"batch","commands":["caption-a","caption-b"]}})
}

fn prepare(journal: &OperationJournal, path: &str) -> u64 {
    match journal
        .prepare(&invocation(), &request(path), path, 7, 1)
        .unwrap()
    {
        Admission::New(id) => id,
        other => panic!("new intent expected: {other:?}"),
    }
}

fn saved_project(path: &Path) {
    let mut file = File::create(path.join("concat.json")).unwrap();
    file.write_all(b"{\"native-project\":\"saved-after-edit\"}")
        .unwrap();
    file.sync_all().unwrap();
}

#[test]
fn four_durable_transitions_are_distinct_and_repeated_work_is_never_replayed() {
    let folder = Directory::new();
    let owner = SessionOwner::acquire(folder.path()).unwrap();
    assert_eq!(
        inspect_operations(&owner, &owner.token())
            .unwrap()
            .needs_project_reconciliation,
        0
    );
    assert!(!folder.path().join("native-operations.sqlite3").exists());
    assert!(OperationJournal::open(&owner, &owner.token()).is_err());
    approve(&owner);
    let journal = OperationJournal::open(&owner, &owner.token()).unwrap();
    let path = folder.path().to_str().unwrap();
    let id = prepare(&journal, path);
    let inspection = inspect_operations(&owner, &owner.token()).unwrap();
    assert_eq!(inspection.needs_project_reconciliation, 1);
    assert_eq!(inspection.applied_not_saved, 0);
    assert!(
        matches!(journal.prepare(&invocation(), &request(path), path, 7, 1),
        Err(JournalError::NeedsReconciliation { operation_id }) if operation_id == id)
    );
    let mut changed = request(path);
    changed["expectedRevision"] = json!(2);
    assert!(matches!(
        journal.prepare(&invocation(), &changed, path, 7, 2),
        Err(JournalError::ConflictingInvocation)
    ));
    assert!(journal.record_notified(id).is_err());
    assert!(journal.record_saved(path, 7, 2).is_err());
    let reply = json!({"sessionId":7,"revision":2,"applied":true});
    journal.record_receipt(id, 7, 2, &reply).unwrap();
    assert_eq!(
        inspect_operations(&owner, &owner.token())
            .unwrap()
            .applied_not_saved,
        1
    );
    assert!(journal.record_notified(id).is_err());
    saved_project(folder.path());
    // The exact project target in the intent is required; an unrelated
    // saved file cannot prove this operation was saved.
    assert_eq!(journal.record_saved(path, 7, 3).unwrap(), 0);
    assert_eq!(
        inspect_operations(&owner, &owner.token())
            .unwrap()
            .applied_not_saved,
        1
    );
    assert_eq!(journal.record_saved(path, 7, 2).unwrap(), 1);
    assert_eq!(
        inspect_operations(&owner, &owner.token())
            .unwrap()
            .saved_not_notified,
        1
    );
    drop(journal);
    owner.finish_handoff(std::time::Duration::ZERO).unwrap();
    let next = SessionOwner::acquire(folder.path()).unwrap();
    assert_eq!(next.status().unwrap().phase, Phase::RecoveryRequired);
    assert_eq!(
        inspect_operations(&next, &next.token())
            .unwrap()
            .saved_not_notified,
        1
    );
    approve(&next);
    let reopened = OperationJournal::open(&next, &next.token()).unwrap();
    assert_eq!(
        reopened
            .prepare(&invocation(), &request(path), path, 7, 1)
            .unwrap(),
        Admission::AlreadyApplied {
            operation_id: id,
            receipt: reply
        }
    );
    reopened.record_notified(id).unwrap();
    assert_eq!(
        inspect_operations(&next, &next.token()).unwrap().notified,
        1
    );
}

/// Run this ignored helper only from the test below. It terminates this test
/// subprocess without Drop or normal Rust teardown at each real disk point.
#[test]
#[ignore = "owned subprocess crash fixture"]
fn crash_child_phase() {
    let Ok(path) = std::env::var("BK_CAT_JOURNAL_CRASH_PATH") else {
        return;
    };
    let phase: u8 = std::env::var("BK_CAT_JOURNAL_CRASH_PHASE")
        .unwrap()
        .parse()
        .unwrap();
    let owner = SessionOwner::acquire(&path).unwrap();
    approve(&owner);
    let journal = OperationJournal::open(&owner, &owner.token()).unwrap();
    let id = prepare(&journal, &path);
    if phase == 2 {
        fs::write(
            Path::new(&path).join("staged-edit-before-receipt"),
            b"edited only in staged native state",
        )
        .unwrap();
    }
    if phase == 3 || phase == 4 {
        journal
            .record_receipt(id, 7, 2, &json!({"sessionId":7,"revision":2}))
            .unwrap();
    }
    if phase == 4 {
        saved_project(Path::new(&path));
        assert_eq!(journal.record_saved(&path, 7, 2).unwrap(), 1);
    }
    if phase == 5 {
        let document = json!({"native-project":"saved-before-receipt","clips":["native-change"]});
        let mut file = File::create(Path::new(&path).join("concat.json")).unwrap();
        file.write_all(&serde_json::to_vec(&document).unwrap())
            .unwrap();
        file.sync_all().unwrap();
        assert_eq!(
            journal
                .record_saved_document(&path, 7, 2, &document)
                .unwrap(),
            0
        );
        assert_eq!(
            inspect_recovery(&owner, &owner.token(), &path)
                .unwrap()
                .operations[0]
                .saved_document_candidates
                .len(),
            1
        );
    }
    let mut marker = File::create(Path::new(&path).join("crash-marker")).unwrap();
    marker.write_all(&[phase]).unwrap();
    marker.sync_all().unwrap();
    std::process::abort();
}

#[test]
fn actual_subprocess_crashes_require_reconciliation_without_replaying() {
    for phase in 1_u8..=5 {
        let folder = Directory::new();
        let status = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "crash_child_phase", "--ignored", "--nocapture"])
            .env("BK_CAT_JOURNAL_CRASH_PATH", folder.path())
            .env("BK_CAT_JOURNAL_CRASH_PHASE", phase.to_string())
            .status()
            .unwrap();
        assert!(!status.success(), "phase {phase} must terminate abnormally");
        assert_eq!(
            fs::read(folder.path().join("crash-marker")).unwrap(),
            [phase]
        );
        let owner = SessionOwner::acquire(folder.path()).expect("crashed process released OS lock");
        assert_eq!(owner.status().unwrap().phase, Phase::RecoveryRequired);
        let inspection = inspect_operations(&owner, &owner.token()).unwrap();
        if phase < 3 || phase == 5 {
            assert_eq!(inspection.needs_project_reconciliation, 1);
            assert_eq!(inspection.applied_not_saved, 0);
        } else if phase == 3 {
            assert_eq!(inspection.needs_project_reconciliation, 0);
            assert_eq!(inspection.applied_not_saved, 1);
        } else {
            assert_eq!(inspection.needs_project_reconciliation, 0);
            assert_eq!(inspection.applied_not_saved, 0);
            assert_eq!(inspection.saved_not_notified, 1);
            assert!(folder.path().join("concat.json").metadata().unwrap().len() > 0);
        }
        if phase != 4 {
            assert_eq!(inspection.saved_not_notified, 0);
        }
        assert_eq!(inspection.notified, 0);
        let review =
            inspect_recovery(&owner, &owner.token(), folder.path().to_str().unwrap()).unwrap();
        assert_eq!(review.unreviewed_current, 1);
        assert_eq!(review.pending_other_projects, 0);
        assert_eq!(review.inspection, inspection);
        let operation = &review.operations[0];
        assert_eq!(
            operation.state,
            match phase {
                3 => RecoveryState::AppliedNotSaved,
                4 => RecoveryState::SavedNotNotified,
                _ => RecoveryState::OutcomeUnknown,
            }
        );
        if phase == 5 {
            assert_eq!(operation.saved_document_candidates.len(), 1);
            let evidence = &operation.saved_document_candidates[0];
            assert_eq!(evidence["currentFileMatches"], true);
            assert_eq!(evidence["provesThisOperation"], false);
            assert!(operation.receipt.is_none());
            assert!(operation.saved.is_none());
        }
        approve(&owner);
        let journal = OperationJournal::open(&owner, &owner.token()).unwrap();
        let reconciled = journal.reconcile_review(&review).unwrap();
        assert_eq!(reconciled.unreviewed_current, 0);
        assert_eq!(
            reconciled.inspection, inspection,
            "review does not change original facts"
        );
        assert!(reconciled.operations[0].reviewed);
        if phase == 5 {
            assert_eq!(
                reconciled.facts()["currentProjectStates"]["outcome-unknown"]["capturedSaveWithoutReceipt"]
                    ["reviewed"],
                1
            );
        } else if phase == 3 {
            assert_eq!(
                reconciled.facts()["currentProjectStates"]["applied-not-saved"]["reviewed"],
                1
            );
        }
        assert!(journal.take_saved_notifications().unwrap().is_empty());
        if phase < 3 || phase == 5 {
            assert!(matches!(
                journal.prepare(
                    &invocation(),
                    &request(folder.path().to_str().unwrap()),
                    folder.path().to_str().unwrap(),
                    7,
                    1
                ),
                Err(JournalError::NeedsReconciliation { .. })
            ));
        } else {
            assert!(matches!(
                journal.prepare(
                    &invocation(),
                    &request(folder.path().to_str().unwrap()),
                    folder.path().to_str().unwrap(),
                    7,
                    1
                ),
                Ok(Admission::AlreadyApplied { .. })
            ));
        }
        let later = Invocation {
            call_id: "new-plan-after-explicit-review".into(),
            ..invocation()
        };
        assert!(matches!(
            journal
                .prepare(
                    &later,
                    &request(folder.path().to_str().unwrap()),
                    folder.path().to_str().unwrap(),
                    7,
                    1
                )
                .unwrap(),
            Admission::New(_)
        ));
    }
}

#[test]
fn saved_document_must_match_the_exact_written_snapshot_and_receipt() {
    let folder = Directory::new();
    let owner = SessionOwner::acquire(folder.path()).unwrap();
    approve(&owner);
    let journal = OperationJournal::open(&owner, &owner.token()).unwrap();
    let path = folder.path().to_str().unwrap();
    let id = prepare(&journal, path);
    let expected = json!({"native-project":"expected-save","clips":["a","b"]});
    fs::write(
        folder.path().join("concat.json"),
        serde_json::to_vec(&expected).unwrap(),
    )
    .unwrap();

    journal
        .record_receipt(id, 7, 2, &json!({"sessionId":7,"revision":2}))
        .unwrap();
    let newer = json!({"native-project":"newer-save","clips":["different"]});
    fs::write(
        folder.path().join("concat.json"),
        serde_json::to_vec(&newer).unwrap(),
    )
    .unwrap();
    assert!(matches!(
        journal.record_saved_document(path, 7, 2, &expected),
        Err(JournalError::InvalidSaveEvidence)
    ));
    assert_eq!(
        inspect_operations(&owner, &owner.token())
            .unwrap()
            .applied_not_saved,
        1
    );

    // The JSON value, rather than incidental key order or whitespace, is
    // compared. Its exact bytes are the bytes included in the saved digest.
    fs::write(
        folder.path().join("concat.json"),
        b"{ \"clips\": [\"a\", \"b\"], \"native-project\": \"expected-save\" }",
    )
    .unwrap();
    assert_eq!(
        journal
            .record_saved_document(path, 7, 3, &expected)
            .unwrap(),
        0
    );
    assert_eq!(
        journal
            .record_saved_document(path, 7, 2, &expected)
            .unwrap(),
        1
    );
    assert_eq!(
        inspect_operations(&owner, &owner.token())
            .unwrap()
            .saved_not_notified,
        1
    );
}

#[test]
fn invalid_saved_document_never_produces_save_evidence() {
    let folder = Directory::new();
    let owner = SessionOwner::acquire(folder.path()).unwrap();
    approve(&owner);
    let journal = OperationJournal::open(&owner, &owner.token()).unwrap();
    let path = folder.path().to_str().unwrap();
    let id = prepare(&journal, path);
    journal
        .record_receipt(id, 7, 2, &json!({"sessionId":7,"revision":2}))
        .unwrap();
    fs::write(folder.path().join("concat.json"), b"truncated {").unwrap();
    assert!(matches!(
        journal.record_saved_document(path, 7, 2, &json!({})),
        Err(JournalError::InvalidSaveEvidence)
    ));
    assert_eq!(
        inspect_operations(&owner, &owner.token())
            .unwrap()
            .applied_not_saved,
        1
    );
}

#[test]
fn early_verified_save_joins_only_the_matching_later_receipt() {
    for saved_session in [7, 8] {
        for saved_revision in [2, 3] {
            let folder = Directory::new();
            let owner = SessionOwner::acquire(folder.path()).unwrap();
            approve(&owner);
            let journal = OperationJournal::open(&owner, &owner.token()).unwrap();
            let path = folder.path().to_str().unwrap();
            let id = prepare(&journal, path);
            let document = json!({"clips":["first","second"]});
            fs::write(
                folder.path().join("concat.json"),
                serde_json::to_vec(&document).unwrap(),
            )
            .unwrap();
            assert_eq!(
                journal
                    .record_saved_document(path, saved_session, saved_revision, &document)
                    .unwrap(),
                0
            );
            assert_eq!(
                inspect_operations(&owner, &owner.token())
                    .unwrap()
                    .needs_project_reconciliation,
                1
            );
            assert!(journal.take_saved_notifications().unwrap().is_empty());
            journal
                .record_receipt(id, 7, 2, &json!({"sessionId":7,"revision":2}))
                .unwrap();
            let inspection = inspect_operations(&owner, &owner.token()).unwrap();
            let notifications = journal.take_saved_notifications().unwrap();
            if (saved_session, saved_revision) == (7, 2) {
                assert_eq!(inspection.applied_not_saved, 0);
                assert_eq!(inspection.saved_not_notified, 1);
                assert_eq!(notifications.len(), 1);
                assert_eq!(notifications[0].operation_id, id);
                assert_eq!(notifications[0].saved["revision"], "2");
                assert_eq!(
                    inspection.notified, 0,
                    "taking a notification is not delivery"
                );
                assert!(journal.take_saved_notifications().unwrap().is_empty());
            } else {
                assert_eq!(inspection.applied_not_saved, 1);
                assert_eq!(inspection.saved_not_notified, 0);
                assert!(notifications.is_empty());
            }
        }
    }
}

#[test]
fn notifications_never_auto_resume_after_reopening_the_journal() {
    let folder = Directory::new();
    let owner = SessionOwner::acquire(folder.path()).unwrap();
    approve(&owner);
    let journal = OperationJournal::open(&owner, &owner.token()).unwrap();
    let path = folder.path().to_str().unwrap();
    let id = prepare(&journal, path);
    journal
        .record_receipt(id, 7, 2, &json!({"sessionId":7,"revision":2}))
        .unwrap();
    let document = json!({"clips":["first","second"]});
    fs::write(
        folder.path().join("concat.json"),
        serde_json::to_vec(&document).unwrap(),
    )
    .unwrap();
    assert_eq!(
        journal
            .record_saved_document(path, 7, 2, &document)
            .unwrap(),
        1
    );
    drop(journal);
    owner.finish_handoff(std::time::Duration::ZERO).unwrap();
    let next = SessionOwner::acquire(folder.path()).unwrap();
    assert_eq!(
        inspect_operations(&next, &next.token())
            .unwrap()
            .saved_not_notified,
        1
    );
    approve(&next);
    let reopened = OperationJournal::open(&next, &next.token()).unwrap();
    assert!(reopened.take_saved_notifications().unwrap().is_empty());
    assert_eq!(
        inspect_operations(&next, &next.token()).unwrap().notified,
        0
    );
}

#[test]
fn proven_native_refusals_are_terminal_but_unknown_failures_are_not() {
    let folder = Directory::new();
    let owner = SessionOwner::acquire(folder.path()).unwrap();
    approve(&owner);
    let journal = OperationJournal::open(&owner, &owner.token()).unwrap();
    let path = folder.path().to_str().unwrap();
    let id = prepare(&journal, path);
    assert!(
        journal
            .record_rejected(id, &json!({"code":"busy","message":"outcome unknown"}))
            .is_err()
    );
    assert!(
        journal
            .record_rejected(id, &json!({"code":"failed","message":"reply lost"}))
            .is_err()
    );
    assert_eq!(
        inspect_operations(&owner, &owner.token())
            .unwrap()
            .needs_project_reconciliation,
        1
    );
    let refusal = json!({"code":"refused","message":"the subtitle track is locked"});
    journal.record_rejected(id, &refusal).unwrap();
    assert!(journal.record_notified(id).is_err());
    assert!(journal.take_saved_notifications().unwrap().is_empty());
    drop(journal);
    owner.finish_handoff(std::time::Duration::ZERO).unwrap();
    let next = SessionOwner::acquire(folder.path()).unwrap();
    let inspection = inspect_operations(&next, &next.token()).unwrap();
    assert_eq!(inspection.needs_project_reconciliation, 0);
    assert_eq!(inspection.applied_not_saved, 0);
    assert_eq!(inspection.saved_not_notified, 0);
    assert_eq!(inspection.notified, 0);
    assert_eq!(inspection.not_applied, 1);
    approve(&next);
    let reopened = OperationJournal::open(&next, &next.token()).unwrap();
    assert_eq!(
        reopened
            .prepare(&invocation(), &request(path), path, 7, 1)
            .unwrap(),
        Admission::PreviouslyRejected {
            operation_id: id,
            error: refusal
        }
    );
}

#[test]
fn unchanged_native_revision_finishes_without_inventing_a_save() {
    let folder = Directory::new();
    let owner = SessionOwner::acquire(folder.path()).unwrap();
    approve(&owner);
    let journal = OperationJournal::open(&owner, &owner.token()).unwrap();
    let path = folder.path().to_str().unwrap();
    let id = prepare(&journal, path);
    assert!(
        journal
            .record_unchanged(id, 7, 2, &json!({"sessionId":7,"revision":2}))
            .is_err()
    );
    journal
        .record_unchanged(id, 7, 1, &json!({"sessionId":7,"revision":1}))
        .unwrap();
    let inspection = inspect_operations(&owner, &owner.token()).unwrap();
    assert_eq!(inspection.not_applied, 1);
    assert_eq!(inspection.applied_not_saved, 0);
    assert_eq!(inspection.saved_not_notified, 0);
    assert!(journal.take_saved_notifications().unwrap().is_empty());
    assert!(
        journal
            .changed_receipt(&invocation(), path, 7, 1)
            .unwrap()
            .is_none()
    );
}

#[test]
fn model_answer_is_not_edit_evidence_without_the_exact_durable_changed_receipt() {
    let folder = Directory::new();
    let owner = SessionOwner::acquire(folder.path()).unwrap();
    approve(&owner);
    let journal = OperationJournal::open(&owner, &owner.token()).unwrap();
    let path = folder.path().to_str().unwrap();
    let changed = json!({"sessionId":7,"revision":2,"applied":true});
    assert!(
        journal
            .changed_receipt(&invocation(), path, 7, 1)
            .unwrap()
            .is_none()
    );
    let operation = prepare(&journal, path);
    assert!(
        journal
            .changed_receipt(&invocation(), path, 7, 1)
            .unwrap()
            .is_none()
    );
    journal.record_receipt(operation, 7, 2, &changed).unwrap();
    assert_eq!(
        journal.changed_receipt(&invocation(), path, 7, 1).unwrap(),
        Some(changed)
    );
    assert!(
        journal
            .changed_receipt(&invocation(), path, 7, 2)
            .unwrap()
            .is_none()
    );
    assert!(
        journal
            .changed_receipt(&invocation(), "another-project", 7, 1)
            .unwrap()
            .is_none()
    );
    assert!(
        journal
            .changed_receipt(
                &Invocation {
                    conversation_id: 12,
                    task_id: 35,
                    call_id: "call/child".into(),
                },
                path,
                7,
                1
            )
            .unwrap()
            .is_none()
    );
    assert!(
        journal
            .changed_receipt(
                &Invocation {
                    conversation_id: 12,
                    task_id: 34,
                    call_id: "another-call".into(),
                },
                path,
                7,
                1
            )
            .unwrap()
            .is_none()
    );
    assert_eq!(
        inspect_operations(&owner, &owner.token())
            .unwrap()
            .applied_not_saved,
        1
    );
}

#[test]
fn read_only_recovery_creates_nothing_and_confirmation_rejects_changed_project_bytes() {
    let folder = Directory::new();
    let owner = SessionOwner::acquire(folder.path()).unwrap();
    let path = folder.path().to_str().unwrap();
    let empty = inspect_recovery(&owner, &owner.token(), path).unwrap();
    assert_eq!(empty.unreviewed_current, 0);
    assert!(!folder.path().join("native-operations.sqlite3").exists());
    approve(&owner);
    let journal = OperationJournal::open(&owner, &owner.token()).unwrap();
    let id = prepare(&journal, path);
    saved_project(folder.path());
    let review = inspect_recovery(&owner, &owner.token(), path).unwrap();
    fs::write(
        folder.path().join("concat.json"),
        br#"{"native-project":"later-manual-save"}"#,
    )
    .unwrap();
    assert!(matches!(
        journal.reconcile_review(&review),
        Err(JournalError::StaleRecoveryReview)
    ));
    let current = inspect_recovery(&owner, &owner.token(), path).unwrap();
    assert_eq!(current.unreviewed_current, 1);
    assert_eq!(current.operations[0].operation_id, id);
    assert_eq!(current.operations[0].state, RecoveryState::OutcomeUnknown);
    assert_ne!(current.fingerprint, review.fingerprint);
    let result = journal.reconcile_review(&current).unwrap();
    assert_eq!(result.unreviewed_current, 0);
    assert_eq!(result.inspection.needs_project_reconciliation, 1);
    assert!(result.operations[0].receipt.is_none());
}

#[test]
fn another_project_and_another_owner_generation_cannot_settle_a_review() {
    let folder = Directory::new();
    let other_project = Directory::new();
    let owner = SessionOwner::acquire(folder.path()).unwrap();
    approve(&owner);
    let journal = OperationJournal::open(&owner, &owner.token()).unwrap();
    let path = folder.path().to_str().unwrap();
    prepare(&journal, path);
    let wrong = inspect_recovery(
        &owner,
        &owner.token(),
        other_project.path().to_str().unwrap(),
    )
    .unwrap();
    assert_eq!(wrong.unreviewed_current, 0);
    assert_eq!(wrong.pending_other_projects, 1);
    assert!(matches!(
        journal.reconcile_review(&wrong),
        Err(JournalError::OtherProjectPending)
    ));
    let previous = inspect_recovery(&owner, &owner.token(), path).unwrap();
    drop(journal);
    owner.finish_handoff(std::time::Duration::ZERO).unwrap();
    let next = SessionOwner::acquire(folder.path()).unwrap();
    approve(&next);
    let reopened = OperationJournal::open(&next, &next.token()).unwrap();
    assert!(matches!(
        reopened.reconcile_review(&previous),
        Err(JournalError::StaleRecoveryReview)
    ));
    let current = inspect_recovery(&next, &next.token(), path).unwrap();
    let reviewed = reopened.reconcile_review(&current).unwrap();
    assert_eq!(reviewed.unreviewed_current, 0);
    assert_eq!(reviewed.operations[0].state, RecoveryState::OutcomeUnknown);
    assert!(
        reopened.reconcile_review(&current).is_err(),
        "consumed stale evidence is not reusable"
    );
}

#[test]
fn persisted_early_capture_cannot_prove_a_new_invocation_after_session_numbers_repeat() {
    let folder = Directory::new();
    let owner = SessionOwner::acquire(folder.path()).unwrap();
    approve(&owner);
    let journal = OperationJournal::open(&owner, &owner.token()).unwrap();
    let path = folder.path().to_str().unwrap();
    let original = prepare(&journal, path);
    let document = json!({"native-project":"original-snapshot"});
    fs::write(
        folder.path().join("concat.json"),
        serde_json::to_vec(&document).unwrap(),
    )
    .unwrap();
    journal
        .record_saved_document(path, 7, 2, &document)
        .unwrap();
    drop(journal);
    owner.finish_handoff(std::time::Duration::ZERO).unwrap();
    let next = SessionOwner::acquire(folder.path()).unwrap();
    let review = inspect_recovery(&next, &next.token(), path).unwrap();
    assert_eq!(review.operations[0].saved_document_candidates.len(), 1);
    approve(&next);
    let reopened = OperationJournal::open(&next, &next.token()).unwrap();
    reopened.reconcile_review(&review).unwrap();
    let invocation = Invocation {
        call_id: "a-different-current-task".into(),
        ..invocation()
    };
    let Admission::New(new) = reopened
        .prepare(&invocation, &request(path), path, 7, 1)
        .unwrap()
    else {
        panic!("new operation")
    };
    assert_ne!(new, original);
    reopened
        .record_receipt(new, 7, 2, &json!({"sessionId":7,"revision":2}))
        .unwrap();
    let facts = inspect_recovery(&next, &next.token(), path).unwrap();
    assert_eq!(facts.operations[0].state, RecoveryState::OutcomeUnknown);
    assert_eq!(facts.operations[1].state, RecoveryState::AppliedNotSaved);
    assert!(facts.operations[1].saved_document_candidates.is_empty());
    assert!(facts.operations[1].saved.is_none());
    assert!(reopened.take_saved_notifications().unwrap().is_empty());
    let updated = json!({"native-project":"new-current-snapshot"});
    fs::write(
        folder.path().join("concat.json"),
        serde_json::to_vec(&updated).unwrap(),
    )
    .unwrap();
    assert_eq!(
        reopened
            .record_saved_document(path, 7, 2, &updated)
            .unwrap(),
        1
    );
    let facts = inspect_recovery(&next, &next.token(), path).unwrap();
    assert_eq!(facts.operations[0].state, RecoveryState::OutcomeUnknown);
    assert_eq!(facts.operations[1].state, RecoveryState::SavedNotNotified);
}

#[test]
fn current_save_does_not_credit_an_old_unsaved_receipt_after_reopening() {
    let folder = Directory::new();
    let owner = SessionOwner::acquire(folder.path()).unwrap();
    approve(&owner);
    let journal = OperationJournal::open(&owner, &owner.token()).unwrap();
    let path = folder.path().to_str().unwrap();
    let original = prepare(&journal, path);
    journal
        .record_receipt(original, 7, 2, &json!({"sessionId":7,"revision":2}))
        .unwrap();
    drop(journal);
    owner.finish_handoff(std::time::Duration::ZERO).unwrap();
    let next = SessionOwner::acquire(folder.path()).unwrap();
    approve(&next);
    let reopened = OperationJournal::open(&next, &next.token()).unwrap();
    let unrelated = json!({"native-project":"current-process-manual-save"});
    fs::write(
        folder.path().join("concat.json"),
        serde_json::to_vec(&unrelated).unwrap(),
    )
    .unwrap();
    assert_eq!(
        reopened
            .record_saved_document(path, 7, 2, &unrelated)
            .unwrap(),
        0
    );
    let review = inspect_recovery(&next, &next.token(), path).unwrap();
    assert_eq!(review.operations[0].state, RecoveryState::AppliedNotSaved);
    assert!(review.operations[0].saved.is_none());
    assert!(reopened.take_saved_notifications().unwrap().is_empty());
}

#[test]
fn one_native_version_cannot_capture_two_different_documents() {
    let folder = Directory::new();
    let owner = SessionOwner::acquire(folder.path()).unwrap();
    approve(&owner);
    let journal = OperationJournal::open(&owner, &owner.token()).unwrap();
    let path = folder.path().to_str().unwrap();
    prepare(&journal, path);
    let original = json!({"native-project":"first"});
    fs::write(
        folder.path().join("concat.json"),
        serde_json::to_vec(&original).unwrap(),
    )
    .unwrap();
    journal
        .record_saved_document(path, 7, 2, &original)
        .unwrap();
    let replacement = json!({"native-project":"different-at-same-version"});
    fs::write(
        folder.path().join("concat.json"),
        serde_json::to_vec(&replacement).unwrap(),
    )
    .unwrap();
    assert!(matches!(
        journal.record_saved_document(path, 7, 2, &replacement),
        Err(JournalError::InvalidSaveEvidence)
    ));
    let facts = inspect_recovery(&owner, &owner.token(), path).unwrap();
    assert_eq!(facts.operations[0].saved_document_candidates.len(), 1);
    assert_eq!(
        facts.operations[0].saved_document_candidates[0]["currentFileMatches"],
        false
    );
    assert!(facts.operations[0].receipt.is_none());
}

#[test]
fn full_history_is_reconciled_while_runtime_facts_use_a_bounded_display_page() {
    let folder = Directory::new();
    let owner = SessionOwner::acquire(folder.path()).unwrap();
    approve(&owner);
    let journal = OperationJournal::open(&owner, &owner.token()).unwrap();
    let path = folder.path().to_str().unwrap();
    for index in 0..40 {
        let call = Invocation {
            call_id: format!("history-{index}"),
            ..invocation()
        };
        let Admission::New(id) = journal.prepare(&call, &request(path), path, 7, 1).unwrap() else {
            panic!("new operation")
        };
        journal
            .record_receipt(
                id,
                7,
                2,
                &json!({"sessionId":7,"revision":2,"largeNativeDetail":"x".repeat(32*1024)}),
            )
            .unwrap();
    }
    let review = inspect_recovery(&owner, &owner.token(), path).unwrap();
    assert_eq!(review.operations.len(), 40);
    assert_eq!(review.unreviewed_current, 40);
    let facts = review.facts();
    assert_eq!(facts["operations"].as_array().unwrap().len(), 32);
    assert_eq!(facts["operationDetails"]["remaining"], 8);
    assert!(
        facts.to_string().len() < 64 * 1024,
        "receipt size and history size do not become a context quota"
    );
    let reconciled = journal.reconcile_review(&review).unwrap();
    assert_eq!(reconciled.operations.len(), 40);
    assert_eq!(reconciled.unreviewed_current, 0);
    assert_eq!(reconciled.inspection.applied_not_saved, 40);
    assert!(
        reconciled
            .operations
            .iter()
            .all(|operation| operation.reviewed)
    );
}

#[test]
fn legacy_journal_adds_evidence_tables_only_after_native_approval() {
    let folder = Directory::new();
    let owner = SessionOwner::acquire(folder.path()).unwrap();
    approve(&owner);
    let journal = OperationJournal::open(&owner, &owner.token()).unwrap();
    let path = folder.path().to_str().unwrap();
    let id = prepare(&journal, path);
    drop(journal);
    {
        let legacy =
            rusqlite::Connection::open(folder.path().join("native-operations.sqlite3")).unwrap();
        legacy
            .execute_batch("DROP TABLE operation_reviews; DROP TABLE saved_documents;")
            .unwrap();
    }
    owner.finish_handoff(std::time::Duration::ZERO).unwrap();
    let next = SessionOwner::acquire(folder.path()).unwrap();
    let review = inspect_recovery(&next, &next.token(), path).unwrap();
    let tables = || {
        let database =
            rusqlite::Connection::open(folder.path().join("native-operations.sqlite3")).unwrap();
        database.query_row("SELECT count(*) FROM sqlite_schema WHERE name IN ('operation_reviews','saved_documents')",[],|row| row.get::<_,i64>(0)).unwrap()
    };
    assert_eq!(tables(), 0, "read-only inspection cannot migrate");
    assert!(OperationJournal::open(&next, &next.token()).is_err());
    approve(&next);
    let reopened = OperationJournal::open(&next, &next.token()).unwrap();
    assert_eq!(tables(), 2);
    let current = reopened.reconcile_review(&review).unwrap();
    assert_eq!(current.operations[0].operation_id, id);
    assert_eq!(current.operations[0].state, RecoveryState::OutcomeUnknown);
    assert_eq!(current.inspection.needs_project_reconciliation, 1);
}

#[test]
fn new_owner_cannot_supply_any_receipt_for_an_earlier_unknown_intent() {
    let folder = Directory::new();
    let owner = SessionOwner::acquire(folder.path()).unwrap();
    approve(&owner);
    let journal = OperationJournal::open(&owner, &owner.token()).unwrap();
    let path = folder.path().to_str().unwrap();
    let original = prepare(&journal, path);
    drop(journal);
    owner.finish_handoff(std::time::Duration::ZERO).unwrap();
    let next = SessionOwner::acquire(folder.path()).unwrap();
    let review = inspect_recovery(&next, &next.token(), path).unwrap();
    approve(&next);
    let reopened = OperationJournal::open(&next, &next.token()).unwrap();
    reopened.reconcile_review(&review).unwrap();
    assert!(matches!(
        reopened.record_receipt(original, 7, 2, &json!({"sessionId":7,"revision":2})),
        Err(JournalError::InvalidTransition)
    ));
    assert!(matches!(
        reopened.record_rejected(original, &json!({"code":"refused"})),
        Err(JournalError::InvalidTransition)
    ));
    assert!(matches!(
        reopened.record_unchanged(original, 7, 1, &json!({"sessionId":7,"revision":1})),
        Err(JournalError::InvalidTransition)
    ));
    let facts = inspect_recovery(&next, &next.token(), path).unwrap();
    assert_eq!(facts.unreviewed_current, 0);
    assert_eq!(facts.operations[0].state, RecoveryState::OutcomeUnknown);
    assert!(facts.operations[0].receipt.is_none());
    let call = Invocation {
        call_id: "genuinely-new-call".into(),
        ..invocation()
    };
    let Admission::New(new) = reopened.prepare(&call, &request(path), path, 7, 1).unwrap() else {
        panic!("new operation")
    };
    reopened
        .record_receipt(new, 7, 2, &json!({"sessionId":7,"revision":2}))
        .unwrap();
    let facts = inspect_recovery(&next, &next.token(), path).unwrap();
    assert_eq!(facts.operations[0].state, RecoveryState::OutcomeUnknown);
    assert_eq!(facts.operations[1].state, RecoveryState::AppliedNotSaved);
}

#[test]
fn two_projects_can_be_reviewed_separately_without_settling_each_others_facts() {
    let folder = Directory::new();
    let project_a = Directory::new();
    let project_b = Directory::new();
    let path_a = project_a.path().to_str().unwrap();
    let path_b = project_b.path().to_str().unwrap();
    saved_project(project_a.path());
    saved_project(project_b.path());
    let owner = SessionOwner::acquire(folder.path()).unwrap();
    approve(&owner);
    let journal = OperationJournal::open(&owner, &owner.token()).unwrap();
    let call_a = Invocation {
        call_id: "project-a-call".into(),
        ..invocation()
    };
    let call_b = Invocation {
        call_id: "project-b-call".into(),
        ..invocation()
    };
    assert!(matches!(
        journal
            .prepare(&call_a, &request(path_a), path_a, 7, 1)
            .unwrap(),
        Admission::New(_)
    ));
    assert!(matches!(
        journal
            .prepare(&call_b, &request(path_b), path_b, 7, 1)
            .unwrap(),
        Admission::New(_)
    ));
    let review_a = inspect_recovery(&owner, &owner.token(), path_a).unwrap();
    assert_eq!(
        (review_a.unreviewed_current, review_a.pending_other_projects),
        (1, 1)
    );
    let after_a = journal.reconcile_review(&review_a).unwrap();
    assert_eq!(
        (after_a.unreviewed_current, after_a.pending_other_projects),
        (0, 1)
    );
    assert!(after_a.operations[0].reviewed);
    assert!(!after_a.operations[1].reviewed);
    assert!(
        after_a
            .operations
            .iter()
            .all(|operation| operation.state == RecoveryState::OutcomeUnknown
                && operation.receipt.is_none())
    );
    let review_b = inspect_recovery(&owner, &owner.token(), path_b).unwrap();
    assert_eq!(
        (review_b.unreviewed_current, review_b.pending_other_projects),
        (1, 0)
    );
    let after_b = journal.reconcile_review(&review_b).unwrap();
    assert_eq!(
        (after_b.unreviewed_current, after_b.pending_other_projects),
        (0, 0)
    );
    assert_eq!(after_b.inspection.needs_project_reconciliation, 2);
    assert!(
        after_b
            .operations
            .iter()
            .all(|operation| operation.reviewed)
    );
    assert!(matches!(
        journal.prepare(&call_a, &request(path_a), path_a, 7, 1),
        Err(JournalError::NeedsReconciliation { .. })
    ));
    assert!(matches!(
        journal.prepare(&call_b, &request(path_b), path_b, 7, 1),
        Err(JournalError::NeedsReconciliation { .. })
    ));
}
