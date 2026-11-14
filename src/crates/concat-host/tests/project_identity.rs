// SPDX-License-Identifier: AGPL-3.0-or-later
//! Real native saves must never adopt a project which replaced the open one.

use std::path::Path;

use concat_host::{ProjectInfo, Session, project_identity, projects};
use concat_project::{Command, DocumentSettings};
use serde_json::Value;

fn create(location: &Path, name: &str) -> ProjectInfo {
    projects::create(
        location.to_str().unwrap(),
        name,
        640,
        360,
        24,
        1,
        [0, 0, 0, 255],
    )
    .expect("creates a native project")
}

fn settings() -> DocumentSettings {
    DocumentSettings {
        name: "First save".into(),
        width: 640,
        height: 360,
        rate_num: 24,
        rate_den: 1,
        canvas: [0, 0, 0, 255],
    }
}

fn legacy(info: &ProjectInfo) -> Vec<u8> {
    let mut document = projects::read_document(&info.path).unwrap().unwrap();
    document
        .as_object_mut()
        .unwrap()
        .remove(project_identity::FIELD);
    let bytes = serde_json::to_vec_pretty(&document).unwrap();
    std::fs::write(projects::manifest_path(Path::new(&info.path)), &bytes).unwrap();
    bytes
}

fn bytes(info: &ProjectInfo) -> Vec<u8> {
    std::fs::read(projects::manifest_path(Path::new(&info.path))).unwrap()
}

#[test]
fn identity_replacement_refuses_pending_autosave_and_manual_save() {
    let scratch = tempfile::tempdir().unwrap();
    let first = create(scratch.path(), "Original");
    let mut session = Session::open_info(&first).unwrap();
    session.save(None).unwrap();
    let original = bytes(&first);
    session.apply(Command::AddTrack { bottom: false }).unwrap();
    let unsaved = session.document();
    let (path, document) = session.prepare_save(None).unwrap();
    let ticket = projects::ticket();
    let authority = session.save_authority();

    let replacement = create(scratch.path(), "Replacement");
    let mut other = Session::open_info(&replacement).unwrap();
    other.apply(Command::AddTrack { bottom: false }).unwrap();
    other.save(None).unwrap();
    let other_document = other.document();
    let archived = scratch.path().join("Original moved");
    std::fs::rename(&first.path, &archived).unwrap();
    std::fs::rename(&replacement.path, &first.path).unwrap();
    // Also make the pending ticket older than the replacement's actual save
    // at this path. Identity refusal must precede the Superseded branch.
    projects::save(&first.path, &other_document).unwrap();
    let protected = bytes(&first);
    let saved = std::thread::spawn(move || {
        projects::save_ordered_with_authority(&path, &document, ticket, &authority)
    })
    .join()
    .unwrap();
    assert!(
        saved.is_err(),
        "a foreign project is not a superseded successful save"
    );
    assert!(
        session.save(None).is_err(),
        "manual/close save must refuse too"
    );
    assert!(projects::save_ordered(&first.path, &unsaved, ticket).is_err());
    assert_eq!(bytes(&first), protected);
    assert_eq!(
        std::fs::read(archived.join("concat.json")).unwrap(),
        original
    );
    assert_eq!(
        session.document(),
        unsaved,
        "the unsaved native edit is kept"
    );
}

#[test]
fn identified_session_refuses_removed_identity_and_damaged_manifests() {
    let scratch = tempfile::tempdir().unwrap();
    let info = create(scratch.path(), "Protected");
    let mut session = Session::open_info(&info).unwrap();
    session.save(None).unwrap();
    let mut no_identity = session.document();
    no_identity
        .as_object_mut()
        .unwrap()
        .remove(project_identity::FIELD);
    let mut bad_identity = session.document();
    bad_identity[project_identity::FIELD] = Value::Null;
    let corrupt_edit = serde_json::json!({
        "concat": "0.2.0", "name": "Damaged",
        "video": {"width": 640, "height": 360, "rateNum": 24, "rateDen": 1},
        "bkCatProjectId": session.project_identity(), "timelines": "damaged"
    });
    let manifest = projects::manifest_path(Path::new(&info.path));
    for damaged in [
        serde_json::to_vec(&no_identity).unwrap(),
        serde_json::to_vec(&bad_identity).unwrap(),
        serde_json::to_vec(&corrupt_edit).unwrap(),
        br#"{"timelines":["#.to_vec(),
        b"null".to_vec(),
    ] {
        std::fs::write(&manifest, &damaged).unwrap();
        assert!(session.save(None).is_err());
        assert_eq!(std::fs::read(&manifest).unwrap(), damaged);
        assert_eq!(std::fs::read_dir(&info.path).unwrap().count(), 1);
    }
    std::fs::remove_file(&manifest).unwrap();
    assert!(session.save(None).is_err());
    assert!(
        !manifest.exists(),
        "a missing identified manifest is never recreated"
    );
}

#[test]
fn generic_saves_keep_legacy_support_without_minting_or_erasing_identity() {
    let scratch = tempfile::tempdir().unwrap();
    let info = create(scratch.path(), "Legacy");
    legacy(&info);
    let mut old = projects::read_document(&info.path).unwrap().unwrap();
    old["name"] = "Older native caller".into();
    projects::save(&info.path, &old).expect("unidentified legacy saves still work");
    assert_eq!(
        project_identity::read_persisted(Path::new(&info.path)).unwrap(),
        None
    );
    let mut session = Session::open_info(&info).unwrap();
    let before = bytes(&info);
    assert!(projects::save(&info.path, &session.document()).is_err());
    assert_eq!(
        bytes(&info),
        before,
        "a generic caller cannot infer first-binding permission"
    );
    session
        .save(None)
        .expect("the native source proof permits the first binding");
    let identified = bytes(&info);
    assert!(projects::save(&info.path, &old).is_err());
    let mut malformed = session.document();
    malformed[project_identity::FIELD] = "bkcat-project:invalid".into();
    assert!(projects::save(&info.path, &malformed).is_err());
    assert_eq!(
        bytes(&info),
        identified,
        "neither input removes the durable identity"
    );
}

#[test]
fn legacy_initialization_checks_original_content_and_allows_a_failed_save_retry() {
    let scratch = tempfile::tempdir().unwrap();
    let info = create(scratch.path(), "Legacy");
    let original = legacy(&info);
    let mut session = Session::open_info(&info).unwrap();
    let manifest = projects::manifest_path(Path::new(&info.path));
    let mut replacement: Value = serde_json::from_slice(&original).unwrap();
    replacement["name"] = "Another old project".into();
    let replacement = serde_json::to_vec(&replacement).unwrap();
    std::fs::write(&manifest, &replacement).unwrap();
    assert!(session.save(None).is_err());
    assert_eq!(std::fs::read(&manifest).unwrap(), replacement);
    std::fs::remove_file(&manifest).unwrap();
    std::fs::create_dir(&manifest).unwrap();
    assert!(session.save(None).is_err());
    assert!(manifest.is_dir());
    std::fs::remove_dir(&manifest).unwrap();
    std::fs::write(&manifest, &original).unwrap();
    session
        .save(None)
        .expect("a failed write has not consumed first-binding authority");
    assert_eq!(
        project_identity::read_persisted(Path::new(&info.path))
            .unwrap()
            .as_deref(),
        Some(session.project_identity())
    );
}

#[test]
fn legacy_permission_is_shared_and_cannot_be_reused_after_persistence() {
    let scratch = tempfile::tempdir().unwrap();
    let info = create(scratch.path(), "Legacy");
    let original = legacy(&info);
    let mut session = Session::open_info(&info).unwrap();
    let mut snapshot = session.snapshot();
    let authority = session.save_authority();
    session.save(None).unwrap();
    let manifest = projects::manifest_path(Path::new(&info.path));
    std::fs::write(&manifest, &original).unwrap();
    assert!(snapshot.save(None).is_err());
    assert!(
        projects::save_ordered_with_authority(
            &info.path,
            &session.document(),
            projects::ticket(),
            &authority,
        )
        .is_err()
    );
    assert_eq!(std::fs::read(&manifest).unwrap(), original);
    std::fs::remove_file(&manifest).unwrap();
    assert!(session.save(None).is_err());
    assert!(!manifest.exists());
}

#[test]
fn first_binding_authority_cannot_be_borrowed_by_another_identity_or_location() {
    let scratch = tempfile::tempdir().unwrap();
    let first = create(scratch.path(), "First legacy");
    let first_bytes = legacy(&first);
    let second = create(scratch.path(), "Second legacy");
    let second_bytes = legacy(&second);
    let first_session = Session::open_info(&first).unwrap();
    let second_session = Session::open_info(&second).unwrap();
    let authority = first_session.save_authority();
    for (path, document) in [
        (&first.path, second_session.document()),
        (&second.path, first_session.document()),
    ] {
        assert!(
            projects::save_ordered_with_authority(path, &document, projects::ticket(), &authority,)
                .is_err()
        );
    }
    assert_eq!(bytes(&first), first_bytes);
    assert_eq!(bytes(&second), second_bytes);
}

#[test]
fn observing_the_persisted_identity_consumes_even_a_superseded_first_permission() {
    let scratch = tempfile::tempdir().unwrap();
    let info = create(scratch.path(), "Legacy");
    let original = legacy(&info);
    let mut session = Session::open_info(&info).unwrap();
    let (path, document) = session.prepare_save(None).unwrap();
    let older = projects::ticket();
    let authority = session.save_authority();
    let manifest = projects::manifest_path(Path::new(&info.path));
    // Simulate the same native identity having reached disk independently of
    // this captured permission, then advance the normal write-order receipt.
    std::fs::write(&manifest, serde_json::to_vec_pretty(&document).unwrap()).unwrap();
    projects::save(&path, &document).unwrap();
    assert_eq!(
        projects::save_ordered_with_authority(&path, &document, older, &authority).unwrap(),
        projects::Saved::Superseded
    );
    std::fs::write(&manifest, &original).unwrap();
    assert!(session.save(None).is_err());
    assert_eq!(std::fs::read(&manifest).unwrap(), original);
}

#[test]
fn identical_legacy_content_in_a_replaced_directory_is_not_first_save_authority() {
    let scratch = tempfile::tempdir().unwrap();
    let info = create(scratch.path(), "Legacy");
    let original = legacy(&info);
    let mut session = Session::open_info(&info).unwrap();
    let moved = scratch.path().join("Moved legacy");
    std::fs::rename(&info.path, &moved).unwrap();
    std::fs::create_dir(&info.path).unwrap();
    std::fs::write(Path::new(&info.path).join("concat.json"), &original).unwrap();
    assert!(session.save(None).is_err());
    assert_eq!(bytes(&info), original);
    assert_eq!(std::fs::read(moved.join("concat.json")).unwrap(), original);
}

#[test]
fn first_save_can_create_its_missing_folder_but_cannot_adopt_a_new_or_replaced_one() {
    let scratch = tempfile::tempdir().unwrap();
    let missing = scratch.path().join("new parent").join("First");
    let mut session = Session::open(missing.to_str().unwrap(), settings()).unwrap();
    session.save(None).unwrap();
    assert_eq!(
        project_identity::read_persisted(&missing)
            .unwrap()
            .as_deref(),
        Some(session.project_identity())
    );

    let appeared = scratch.path().join("Appeared");
    let mut session = Session::open(appeared.to_str().unwrap(), settings()).unwrap();
    std::fs::create_dir(&appeared).unwrap();
    assert!(session.save(None).is_err());
    assert_eq!(std::fs::read_dir(&appeared).unwrap().count(), 0);

    let replaced = scratch.path().join("Replaced empty");
    std::fs::create_dir(&replaced).unwrap();
    let mut session = Session::open(replaced.to_str().unwrap(), settings()).unwrap();
    std::fs::rename(&replaced, scratch.path().join("Moved empty")).unwrap();
    std::fs::create_dir(&replaced).unwrap();
    assert!(session.save(None).is_err());
    assert_eq!(std::fs::read_dir(replaced).unwrap().count(), 0);
}

#[test]
fn concurrent_legacy_sessions_cannot_bind_two_identities_to_the_same_project() {
    let scratch = tempfile::tempdir().unwrap();
    let info = create(scratch.path(), "Legacy");
    legacy(&info);
    let mut first = Session::open_info(&info).unwrap();
    let mut second = Session::open_info(&info).unwrap();
    let first_identity = first.project_identity().to_owned();
    let second_identity = second.project_identity().to_owned();
    assert_ne!(first_identity, second_identity);
    let start = std::sync::Barrier::new(2);
    let (left, right) = std::thread::scope(|scope| {
        let left = scope.spawn(|| {
            start.wait();
            first.save(Some("First"))
        });
        let right = scope.spawn(|| {
            start.wait();
            second.save(Some("Second"))
        });
        (left.join().unwrap(), right.join().unwrap())
    });
    assert_ne!(
        left.is_ok(),
        right.is_ok(),
        "exactly one first binding lands"
    );
    let identity = project_identity::read_persisted(Path::new(&info.path))
        .unwrap()
        .unwrap();
    assert_eq!(
        identity,
        if left.is_ok() {
            first_identity
        } else {
            second_identity
        }
    );
}

#[test]
fn first_identity_save_preserves_the_legacy_manifest_filename() {
    let scratch = tempfile::tempdir().unwrap();
    let info = create(scratch.path(), "Legacy filename");
    legacy(&info);
    let root = Path::new(&info.path);
    std::fs::rename(root.join("concat.json"), root.join("wolfcut.json")).unwrap();
    let mut session = Session::open_info(&info).unwrap();
    session.save(None).unwrap();
    assert!(!root.join("concat.json").exists());
    assert!(root.join("wolfcut.json").is_file());
    assert_eq!(
        project_identity::read_persisted(root).unwrap().as_deref(),
        Some(session.project_identity())
    );
}
