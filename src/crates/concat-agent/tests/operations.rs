// SPDX-License-Identifier: AGPL-3.0-or-later
//! Real disk journal tests. The crash subprocess is a test helper, not a
//! whole BK CAT application-process or Android lifecycle acceptance test.

use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use concat_agent::operations::{
    Admission, Invocation, JournalError, OperationJournal, inspect_operations,
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
    if phase >= 3 {
        journal
            .record_receipt(id, 7, 2, &json!({"sessionId":7,"revision":2}))
            .unwrap();
    }
    if phase >= 4 {
        saved_project(Path::new(&path));
        assert_eq!(journal.record_saved(&path, 7, 2).unwrap(), 1);
    }
    let mut marker = File::create(Path::new(&path).join("crash-marker")).unwrap();
    marker.write_all(&[phase]).unwrap();
    marker.sync_all().unwrap();
    std::process::abort();
}

#[test]
fn actual_subprocess_crashes_require_reconciliation_without_replaying() {
    for phase in 1_u8..=4 {
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
        if phase < 3 {
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
        if phase < 4 {
            assert_eq!(inspection.saved_not_notified, 0);
        }
        assert_eq!(inspection.notified, 0);
        approve(&owner);
        let journal = OperationJournal::open(&owner, &owner.token()).unwrap();
        if phase < 3 {
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
}
