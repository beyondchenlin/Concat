// SPDX-License-Identifier: AGPL-3.0-or-later
//! Real-file persistence, concurrency and failure tests for independent scripts.
//! These cover the storage core, not the complete native UI or Android app.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Barrier};
use std::time::{SystemTime, UNIX_EPOCH};

use concat_agent::scripts::{SCRIPT_DATABASE_FILE_NAME, ScriptDraft, ScriptError, ScriptStore};
use rusqlite::{Connection, params};

static NEXT: AtomicU64 = AtomicU64::new(0);
struct Directory(PathBuf);
impl Directory {
    fn new() -> Self {
        let time = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "bk-cat-script-versions-{}-{time}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
    fn path(&self) -> &Path {
        &self.0
    }
    fn database(&self) -> PathBuf {
        self.0.join(SCRIPT_DATABASE_FILE_NAME)
    }
}
impl Drop for Directory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn chinese_paragraphs_and_old_selected_version_survive_reopen_and_further_edits() {
    let root = Directory::new();
    let original = "第一段：你好，世界！\r\n\r\n第二段……保留  空格。\n末尾换行\n";
    let binding;
    {
        let store = ScriptStore::open(root.path()).unwrap();
        let first = store.create(r"E:\\视频工程\\知识讲解", original).unwrap();
        assert!(first.selected.is_none());
        assert!(matches!(
            store.bind_selected(&first.project_id),
            Err(ScriptError::NoSelectedVersion)
        ));
        let first = store.select(&first, 1).unwrap();
        binding = store.bind_selected(&first.project_id).unwrap();
        let mut draft = ScriptDraft::from_snapshot(first);
        draft
            .set_text("手动修改的第二版。\n第一版仍是剪辑依据。".into())
            .unwrap();
        let saved = store.save(&mut draft).unwrap();
        assert_eq!(saved.latest.version, 2);
        assert_eq!(saved.selected.as_ref(), Some(&binding));
        assert!(!draft.is_dirty());
    }
    let store = ScriptStore::open(root.path()).unwrap();
    let versions = store.versions(&binding.project_id).unwrap();
    assert_eq!(versions.len(), 2);
    assert_eq!(versions[0].content.as_bytes(), original.as_bytes());
    assert_eq!(store.resolve_binding(&binding).unwrap(), versions[0]);
    assert_eq!(store.bind_selected(&binding.project_id).unwrap(), binding);
}

#[test]
fn project_identity_and_document_identity_prevent_cross_project_binding() {
    let root = Directory::new();
    let store = ScriptStore::open(root.path()).unwrap();
    let first = store.create("project-a", "相同正文").unwrap();
    let second = store.create("project-b", "相同正文").unwrap();
    assert_ne!(first.document_id, second.document_id);
    let first = store.select(&first, 1).unwrap();
    let binding = first.selected.unwrap();
    let mut wrong = binding.clone();
    wrong.project_id = second.project_id;
    assert!(matches!(
        store.resolve_binding(&wrong),
        Err(ScriptError::WrongVersion)
    ));
    let mut wrong = binding;
    wrong.sha256 = "0".repeat(64);
    assert!(matches!(
        store.resolve_binding(&wrong),
        Err(ScriptError::WrongVersion)
    ));
    assert!(matches!(
        store.create("project-a", "overwrite"),
        Err(ScriptError::AlreadyExists)
    ));
}

#[test]
fn concurrent_snapshots_allow_exactly_one_writer_without_losing_the_other_draft() {
    let root = Directory::new();
    let initial = ScriptStore::open(root.path())
        .unwrap()
        .create("project", "原文")
        .unwrap();
    let barrier = Arc::new(Barrier::new(2));
    let handles = ["修改甲", "修改乙"]
        .into_iter()
        .map(|text| {
            let path = root.path().to_path_buf();
            let snapshot = initial.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                let store = ScriptStore::open(&path).unwrap();
                let mut draft = ScriptDraft::from_snapshot(snapshot);
                draft.set_text(text.into()).unwrap();
                barrier.wait();
                let result = store.save(&mut draft);
                match result {
                    Ok(_) => {
                        assert!(!draft.is_dirty());
                        true
                    }
                    Err(ScriptError::StaleSnapshot) => {
                        assert!(draft.is_dirty());
                        assert_eq!(draft.text(), text);
                        assert_eq!(draft.snapshot().latest.version, 1);
                        false
                    }
                    other => panic!("unexpected concurrent result: {other:?}"),
                }
            })
        })
        .collect::<Vec<_>>();
    let wins = handles
        .into_iter()
        .map(|handle| usize::from(handle.join().unwrap()))
        .sum::<usize>();
    assert_eq!(wins, 1);
    assert_eq!(
        ScriptStore::open(root.path())
            .unwrap()
            .versions("project")
            .unwrap()
            .len(),
        2
    );
}

#[test]
fn selection_updates_preserve_unsaved_edits_and_reject_old_snapshots() {
    let root = Directory::new();
    let store = ScriptStore::open(root.path()).unwrap();
    let original = store.create("project", "原文").unwrap();
    let mut draft = ScriptDraft::from_snapshot(original.clone());
    draft.set_text("尚未保存的手稿".into()).unwrap();
    let selected = store.select(&original, 1).unwrap();
    assert!(matches!(
        store.select(&original, 1),
        Err(ScriptError::StaleSnapshot)
    ));
    draft.update_selection(selected).unwrap();
    assert!(draft.is_dirty());
    assert_eq!(draft.text(), "尚未保存的手稿");
    let saved = store.save(&mut draft).unwrap();
    assert_eq!(saved.latest.version, 2);
    assert_eq!(saved.selected.unwrap().version, 1);
}

#[test]
fn late_generation_remains_a_persisted_candidate_without_overwriting_manual_text() {
    let root = Directory::new();
    let store = ScriptStore::open(root.path()).unwrap();
    let original = store.create("project", "原文").unwrap();
    let mut draft = ScriptDraft::from_snapshot(original.clone());
    let ticket = draft.generation_ticket();
    draft.set_text("生成期间，我已手动改稿。".into()).unwrap();
    let candidate = store.record_candidate(&ticket, "迟到的模型建议").unwrap();
    assert_eq!(store.load("project").unwrap().unwrap(), original);
    assert_eq!(store.versions("project").unwrap().len(), 1);
    assert!(matches!(
        store.apply_candidate(&mut draft, &candidate.candidate_id),
        Err(ScriptError::StaleCandidate)
    ));
    assert_eq!(draft.text(), "生成期间，我已手动改稿。");
    drop(store);
    let reopened = ScriptStore::open(root.path()).unwrap();
    assert_eq!(reopened.candidates("project").unwrap(), vec![candidate]);
}

#[test]
fn edit_then_restore_same_text_still_invalidates_a_generation_request() {
    let root = Directory::new();
    let store = ScriptStore::open(root.path()).unwrap();
    let mut draft = ScriptDraft::from_snapshot(store.create("project", "原文").unwrap());
    let ticket = draft.generation_ticket();
    draft.set_text("手改".into()).unwrap();
    draft.set_text("原文".into()).unwrap();
    let candidate = store.record_candidate(&ticket, "候选").unwrap();
    assert!(matches!(
        store.apply_candidate(&mut draft, &candidate.candidate_id),
        Err(ScriptError::StaleCandidate)
    ));
}

#[test]
fn explicit_candidate_acceptance_is_a_draft_until_a_separate_save() {
    let root = Directory::new();
    let store = ScriptStore::open(root.path()).unwrap();
    let first = store.create("project", "原文").unwrap();
    let first = store.select(&first, 1).unwrap();
    let mut draft = ScriptDraft::from_snapshot(first.clone());
    let candidate = store
        .record_candidate(&draft.generation_ticket(), "改写候选\n第二行")
        .unwrap();
    store
        .apply_candidate(&mut draft, &candidate.candidate_id)
        .unwrap();
    assert!(draft.is_dirty());
    assert_eq!(store.load("project").unwrap().unwrap(), first);
    let saved = store.save(&mut draft).unwrap();
    assert_eq!(saved.latest.content, "改写候选\n第二行");
    assert_eq!(saved.latest.candidate_id, Some(candidate.candidate_id));
    assert_eq!(saved.selected.unwrap().version, 1);
}

#[test]
fn a_saved_intervening_edit_keeps_old_candidate_visible_but_ineligible() {
    let root = Directory::new();
    let store = ScriptStore::open(root.path()).unwrap();
    let mut draft = ScriptDraft::from_snapshot(store.create("project", "原文").unwrap());
    let ticket = draft.generation_ticket();
    draft.set_text("已经保存的手改".into()).unwrap();
    store.save(&mut draft).unwrap();
    let candidate = store.record_candidate(&ticket, "迟到候选").unwrap();
    assert!(matches!(
        store.apply_candidate(&mut draft, &candidate.candidate_id),
        Err(ScriptError::StaleCandidate)
    ));
    assert_eq!(store.candidates("project").unwrap().len(), 1);
    assert_eq!(draft.text(), "已经保存的手改");
    assert!(!draft.is_dirty());
}

#[test]
fn read_only_connection_refuses_save_without_clearing_the_draft() {
    let root = Directory::new();
    let initial = ScriptStore::open(root.path())
        .unwrap()
        .create("project", "原文")
        .unwrap();
    let store = ScriptStore::open_read_only(root.path()).unwrap();
    let mut draft = ScriptDraft::from_snapshot(initial.clone());
    draft.set_text("保存失败必须保留我".into()).unwrap();
    assert!(matches!(store.save(&mut draft), Err(ScriptError::ReadOnly)));
    assert_eq!(draft.snapshot(), &initial);
    assert_eq!(draft.text(), "保存失败必须保留我");
    assert!(draft.is_dirty());
}

#[test]
fn real_database_busy_failure_preserves_the_draft_then_allows_an_explicit_retry() {
    let root = Directory::new();
    let store = ScriptStore::open(root.path()).unwrap();
    let initial = store.create("project", "原文").unwrap();
    let blocker = Connection::open(root.database()).unwrap();
    blocker.execute_batch("BEGIN IMMEDIATE").unwrap();
    let mut draft = ScriptDraft::from_snapshot(initial.clone());
    draft.set_text("保留这段未保存内容".into()).unwrap();
    assert!(matches!(
        store.save(&mut draft),
        Err(ScriptError::Database(_))
    ));
    assert_eq!(draft.snapshot(), &initial);
    assert!(draft.is_dirty());
    blocker.execute_batch("ROLLBACK").unwrap();
    store.save(&mut draft).unwrap();
    assert_eq!(store.versions("project").unwrap().len(), 2);
}

#[test]
fn failure_after_version_insert_rolls_back_the_entire_save() {
    let root = Directory::new();
    let store = ScriptStore::open(root.path()).unwrap();
    let initial = store.create("project", "原文").unwrap();
    let injector = Connection::open(root.database()).unwrap();
    injector.execute_batch(
        "CREATE TRIGGER test_refuse_head BEFORE UPDATE ON script_documents BEGIN SELECT RAISE(ABORT, 'injected persistent failure'); END"
    ).unwrap();
    let mut draft = ScriptDraft::from_snapshot(initial.clone());
    draft.set_text("不得产生半保存版本".into()).unwrap();
    assert!(matches!(
        store.save(&mut draft),
        Err(ScriptError::Database(_))
    ));
    assert!(draft.is_dirty());
    assert_eq!(draft.snapshot(), &initial);
    let count: i64 = injector
        .query_row("SELECT count(*) FROM script_versions", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count, 1);
    injector
        .execute_batch("DROP TRIGGER test_refuse_head")
        .unwrap();
    assert_eq!(store.load("project").unwrap().unwrap(), initial);
}

#[test]
fn corrupted_or_unrecognized_existing_files_are_never_reinitialized() {
    for bytes in [b"not a SQLite database".as_slice(), b"".as_slice()] {
        let root = Directory::new();
        fs::write(root.database(), bytes).unwrap();
        assert!(ScriptStore::open(root.path()).is_err());
        assert_eq!(fs::read(root.database()).unwrap(), bytes);
    }
    let root = Directory::new();
    {
        let foreign = Connection::open(root.database()).unwrap();
        foreign
            .execute_batch(
                "CREATE TABLE unrelated(secret TEXT); INSERT INTO unrelated VALUES('keep');",
            )
            .unwrap();
    }
    let before = fs::read(root.database()).unwrap();
    assert!(matches!(
        ScriptStore::open(root.path()),
        Err(ScriptError::InvalidStorage)
    ));
    assert_eq!(fs::read(root.database()).unwrap(), before);
}

#[test]
fn changed_content_with_old_digest_is_refused_on_read_and_plan_resolution() {
    let root = Directory::new();
    let store = ScriptStore::open(root.path()).unwrap();
    let initial = store.create("project", "原文").unwrap();
    let binding = store.select(&initial, 1).unwrap().selected.unwrap();
    let injector = Connection::open(root.database()).unwrap();
    let trigger: String = injector
        .query_row(
            "SELECT sql FROM sqlite_schema WHERE name='immutable_script_versions_update'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    injector
        .execute_batch("DROP TRIGGER immutable_script_versions_update")
        .unwrap();
    injector
        .execute("UPDATE script_versions SET content='damaged'", [])
        .unwrap();
    injector.execute_batch(&trigger).unwrap();
    assert!(matches!(
        store.load("project"),
        Err(ScriptError::InvalidStorage)
    ));
    assert!(matches!(
        store.resolve_binding(&binding),
        Err(ScriptError::InvalidStorage)
    ));
}

#[test]
fn immutable_rows_are_protected_even_from_accidental_direct_sql_updates() {
    let root = Directory::new();
    let store = ScriptStore::open(root.path()).unwrap();
    let draft = ScriptDraft::from_snapshot(store.create("project", "原文").unwrap());
    store
        .record_candidate(&draft.generation_ticket(), "候选")
        .unwrap();
    let sql = Connection::open(root.database()).unwrap();
    for statement in [
        "UPDATE script_versions SET content='override'",
        "DELETE FROM script_versions",
        "UPDATE script_candidates SET content='override'",
        "DELETE FROM script_candidates",
    ] {
        assert!(sql.execute(statement, []).is_err());
    }
    assert_eq!(store.versions("project").unwrap().len(), 1);
    assert_eq!(store.candidates("project").unwrap().len(), 1);
}

#[test]
fn storage_objects_and_invalid_identities_fail_closed() {
    let root = Directory::new();
    fs::create_dir(root.database()).unwrap();
    assert!(ScriptStore::open(root.path()).is_err());
    let valid = Directory::new();
    let store = ScriptStore::open(valid.path()).unwrap();
    for identity in ["", "with\ncontrol"] {
        assert!(matches!(
            store.create(identity, ""),
            Err(ScriptError::InvalidInput)
        ));
    }
    assert!(matches!(store.load("not-created"), Ok(None)));
    assert!(matches!(
        store.select(&store.create("project", "").unwrap(), 999),
        Err(ScriptError::WrongVersion)
    ));
}

#[test]
fn crash_before_commit_rolls_back_and_crash_after_commit_retains_the_saved_version() {
    for stage in ["before-commit", "after-commit"] {
        let root = Directory::new();
        let original = ScriptStore::open(root.path())
            .unwrap()
            .create("project", "原文")
            .unwrap();
        let status = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "script_crash_child", "--ignored", "--nocapture"])
            .env("BK_CAT_SCRIPT_TEST_ROOT", root.path())
            .env("BK_CAT_SCRIPT_TEST_STAGE", stage)
            .status()
            .unwrap();
        assert_eq!(status.code(), Some(77));
        let reopened = ScriptStore::open(root.path()).unwrap();
        let current = reopened.load("project").unwrap().unwrap();
        if stage == "before-commit" {
            assert_eq!(current, original);
            assert_eq!(reopened.versions("project").unwrap().len(), 1);
        } else {
            assert_eq!(current.latest.version, 2);
            assert_eq!(current.latest.content, "已真正提交");
            let mut stale = ScriptDraft::from_snapshot(original);
            stale.set_text("丢回执后不得覆盖已提交文本".into()).unwrap();
            assert!(matches!(
                reopened.save(&mut stale),
                Err(ScriptError::StaleSnapshot)
            ));
        }
    }
}

#[test]
#[ignore = "invoked by the real process-crash parent test"]
fn script_crash_child() {
    let root = PathBuf::from(std::env::var_os("BK_CAT_SCRIPT_TEST_ROOT").unwrap());
    match std::env::var("BK_CAT_SCRIPT_TEST_STAGE").unwrap().as_str() {
        "before-commit" => {
            let connection = Connection::open(root.join(SCRIPT_DATABASE_FILE_NAME)).unwrap();
            connection
                .execute_batch("PRAGMA synchronous=FULL; BEGIN IMMEDIATE")
                .unwrap();
            connection.execute(
                "INSERT INTO script_versions(document_id,version,content,sha256) SELECT document_id,2,?1,?2 FROM script_documents WHERE project_id='project'",
                params!["未提交", "0".repeat(64)],
            ).unwrap();
            connection.execute("UPDATE script_documents SET head_version=2, revision=2 WHERE project_id='project'", []).unwrap();
            std::process::exit(77);
        }
        "after-commit" => {
            let store = ScriptStore::open(&root).unwrap();
            let mut draft = ScriptDraft::from_snapshot(store.load("project").unwrap().unwrap());
            draft.set_text("已真正提交".into()).unwrap();
            store.save(&mut draft).unwrap();
            std::process::exit(77);
        }
        _ => panic!("unexpected isolated crash stage"),
    }
}

#[test]
fn corrupted_head_or_revision_cannot_hide_saved_history() {
    let root = Directory::new();
    let store = ScriptStore::open(root.path()).unwrap();
    let mut draft = ScriptDraft::from_snapshot(store.create("project", "first").unwrap());
    draft.set_text("second".into()).unwrap();
    store.save(&mut draft).unwrap();
    let injector = Connection::open(root.database()).unwrap();
    injector
        .execute("UPDATE script_documents SET head_version=1", [])
        .unwrap();
    assert!(matches!(
        store.load("project"),
        Err(ScriptError::InvalidStorage)
    ));
    injector
        .execute("UPDATE script_documents SET head_version=2, revision=1", [])
        .unwrap();
    assert!(matches!(
        store.load("project"),
        Err(ScriptError::InvalidStorage)
    ));
}
#[test]
fn simultaneous_first_open_publishes_one_complete_database_for_both_projects() {
    let root = Directory::new();
    let barrier = Arc::new(Barrier::new(2));
    let handles = ["project-first", "project-second"]
        .into_iter()
        .map(|project| {
            let path = root.path().to_path_buf();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                let store = ScriptStore::open(&path).unwrap();
                store.create(project, "first manual version").unwrap()
            })
        })
        .collect::<Vec<_>>();
    let snapshots = handles
        .into_iter()
        .map(|handle| handle.join().unwrap())
        .collect::<Vec<_>>();
    let reopened = ScriptStore::open(root.path()).unwrap();
    for snapshot in snapshots {
        assert_eq!(
            reopened.load(&snapshot.project_id).unwrap().unwrap(),
            snapshot
        );
    }
}
