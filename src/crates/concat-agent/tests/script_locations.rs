// SPDX-License-Identifier: AGPL-3.0-or-later
//! Real filesystem location/identity admission. No network or model calls.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Barrier};

use concat_agent::script_locations::{with_project_location, with_read_only_project_location};
use concat_agent::scripts::ScriptStore;
use sha2::{Digest, Sha256};

const FIRST: &str = "bkcat-project:11111111111111111111111111111111";
const SECOND: &str = "bkcat-project:22222222222222222222222222222222";
static NEXT: AtomicU64 = AtomicU64::new(0);

struct Directory(PathBuf);
impl Directory {
    fn new() -> Self {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "bk-cat-location-{}-{stamp}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
    fn project(&self, name: &str, identity: &str) -> PathBuf {
        let path = self.0.join(name);
        fs::create_dir(&path).unwrap();
        fs::write(
            path.join("concat.json"),
            serde_json::to_vec(&serde_json::json!({"bkCatProjectId": identity})).unwrap(),
        )
        .unwrap();
        path
    }
    fn record(&self, suffix: &str) -> PathBuf {
        self.0
            .join("project-locations")
            .join(format!("{:x}.{suffix}", Sha256::digest(FIRST.as_bytes())))
    }
}
impl Drop for Directory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn read_identity(path: &Path) -> Result<Option<String>, String> {
    match fs::read(path.join("concat.json")) {
        Ok(bytes) => {
            let document: serde_json::Value =
                serde_json::from_slice(&bytes).map_err(|error| error.to_string())?;
            Ok(document
                .get("bkCatProjectId")
                .and_then(|value| value.as_str())
                .map(str::to_owned))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.to_string()),
    }
}

#[test]
fn recovery_location_read_never_creates_missing_registration() {
    let root = Directory::new();
    let original = root.project("original", FIRST);
    let ran = AtomicBool::new(false);
    assert!(
        with_read_only_project_location(&root.0, FIRST, &original, read_identity, || {
            ran.store(true, Ordering::SeqCst);
            Ok(())
        })
        .is_err()
    );
    assert!(!ran.load(Ordering::SeqCst));
    assert!(!root.0.join("project-locations").exists());
    with_project_location(&root.0, FIRST, &original, read_identity, || Ok(())).unwrap();
    fs::remove_file(root.record("json")).unwrap();
    assert!(
        with_read_only_project_location(&root.0, FIRST, &original, read_identity, || Ok(()))
            .is_err()
    );
    assert!(!root.record("json").exists());
}

#[test]
fn recovery_location_read_refuses_copies_and_allows_moves_without_rewriting() {
    let root = Directory::new();
    let original = root.project("original", FIRST);
    let copy = root.project("copy", FIRST);
    with_project_location(&root.0, FIRST, &original, read_identity, || Ok(())).unwrap();
    let before = fs::read(root.record("json")).unwrap();
    let ran = AtomicBool::new(false);
    assert!(
        with_read_only_project_location(&root.0, FIRST, &copy, read_identity, || {
            ran.store(true, Ordering::SeqCst);
            Ok(())
        })
        .unwrap_err()
        .contains("副本")
    );
    assert!(!ran.load(Ordering::SeqCst));
    let moved = root.0.join("moved");
    fs::rename(&original, &moved).unwrap();
    root.project("original", SECOND);
    assert_eq!(
        with_read_only_project_location(&root.0, FIRST, &moved, read_identity, || Ok(71)).unwrap(),
        71
    );
    assert_eq!(fs::read(root.record("json")).unwrap(), before);
    // The inspection released the existing kernel lock and did not steal the
    // registry update which belongs to the next explicit document operation.
    with_project_location(&root.0, FIRST, &moved, read_identity, || Ok(())).unwrap();
    assert_ne!(fs::read(root.record("json")).unwrap(), before);
}

#[test]
fn a_move_keeps_the_script_and_a_reused_old_path_gets_no_old_script() {
    let root = Directory::new();
    let original = root.project("original", FIRST);
    let store = ScriptStore::open(&root.0).unwrap();
    with_project_location(&root.0, FIRST, &original, read_identity, || {
        store
            .create(FIRST, "原工程独立文案")
            .map(|_| ())
            .map_err(|error| error.to_string())
    })
    .unwrap();
    let moved = root.0.join("moved");
    fs::rename(&original, &moved).unwrap();
    root.project("original", SECOND);
    with_project_location(&root.0, SECOND, &original, read_identity, || {
        assert!(store.load(SECOND).unwrap().is_none());
        store
            .create(SECOND, "另一工程文案")
            .map(|_| ())
            .map_err(|error| error.to_string())
    })
    .unwrap();
    with_project_location(&root.0, FIRST, &moved, read_identity, || {
        assert_eq!(
            store.load(FIRST).unwrap().unwrap().latest.content,
            "原工程独立文案"
        );
        Ok(())
    })
    .unwrap();
    assert_eq!(
        store.load(SECOND).unwrap().unwrap().latest.content,
        "另一工程文案"
    );
}

#[test]
fn a_live_copy_and_a_mismatched_current_manifest_are_refused_before_writing() {
    let root = Directory::new();
    let original = root.project("original", FIRST);
    let copy = root.project("copy", FIRST);
    with_project_location(&root.0, FIRST, &original, read_identity, || Ok(())).unwrap();
    let ran = AtomicBool::new(false);
    let result = with_project_location(&root.0, FIRST, &copy, read_identity, || {
        ran.store(true, Ordering::SeqCst);
        Ok(())
    });
    assert!(result.unwrap_err().contains("副本"));
    assert!(!ran.load(Ordering::SeqCst));
    assert!(
        with_project_location(&root.0, SECOND, &original, read_identity, || {
            ran.store(true, Ordering::SeqCst);
            Ok(())
        })
        .is_err()
    );
    assert!(!ran.load(Ordering::SeqCst));
    with_project_location(&root.0, FIRST, &original, read_identity, || Ok(())).unwrap();
}

#[test]
fn corrupt_registration_and_unreadable_previous_identity_preserve_the_record() {
    let root = Directory::new();
    let original = root.project("original", FIRST);
    let copy = root.project("copy", FIRST);
    with_project_location(&root.0, FIRST, &original, read_identity, || Ok(())).unwrap();
    let record = root.record("json");
    let before = fs::read(&record).unwrap();
    fs::write(original.join("concat.json"), b"{").unwrap();
    assert!(with_project_location(&root.0, FIRST, &copy, read_identity, || Ok(())).is_err());
    assert_eq!(fs::read(&record).unwrap(), before);
    fs::write(&record, b"unknown previous record").unwrap();
    assert!(with_project_location(&root.0, FIRST, &copy, read_identity, || Ok(())).is_err());
    assert_eq!(fs::read(record).unwrap(), b"unknown previous record");
}

#[test]
fn simultaneous_copies_never_both_acquire_one_identity() {
    let root = Directory::new();
    let paths = [root.project("first", FIRST), root.project("second", FIRST)];
    let barrier = Arc::new(Barrier::new(2));
    let handles = paths
        .into_iter()
        .map(|path| {
            let barrier = Arc::clone(&barrier);
            let root = root.0.clone();
            std::thread::spawn(move || {
                barrier.wait();
                with_project_location(&root, FIRST, &path, read_identity, || Ok(())).is_ok()
            })
        })
        .collect::<Vec<_>>();
    let successes = handles
        .into_iter()
        .map(|handle| usize::from(handle.join().unwrap()))
        .sum::<usize>();
    assert_eq!(successes, 1);
}

#[test]
fn the_kernel_lock_covers_the_document_action_and_is_released_after_failure() {
    use std::sync::mpsc::channel;
    use std::time::Duration;
    let root = Directory::new();
    let path = root.project("project", FIRST);
    let worker_root = root.0.clone();
    let worker_path = path.clone();
    let (entered_tx, entered_rx) = channel();
    let (release_tx, release_rx) = channel();
    let worker = std::thread::spawn(move || {
        with_project_location(&worker_root, FIRST, &worker_path, read_identity, || {
            entered_tx.send(()).unwrap();
            release_rx.recv_timeout(Duration::from_secs(10)).unwrap();
            Err::<(), _>("injected document failure".into())
        })
    });
    entered_rx.recv_timeout(Duration::from_secs(10)).unwrap();
    let refused = with_project_location(&root.0, FIRST, &path, read_identity, || Ok(()));
    let read_refused =
        with_read_only_project_location(&root.0, FIRST, &path, read_identity, || Ok(()));
    release_tx.send(()).unwrap();
    assert!(worker.join().unwrap().is_err());
    assert!(refused.is_err());
    assert!(read_refused.is_err());
    assert!(
        root.record("lock").is_file(),
        "the permanent lock inode must not be unlinked"
    );
    with_project_location(&root.0, FIRST, &path, read_identity, || Ok(())).unwrap();
}

#[cfg(unix)]
#[test]
fn linked_registry_and_lock_objects_are_refused_without_touching_the_target() {
    let root = Directory::new();
    let original = root.project("original", FIRST);
    let outside = root.0.join("outside");
    fs::write(&outside, b"keep these unrelated bytes").unwrap();
    fs::create_dir(root.0.join("project-locations")).unwrap();
    for suffix in ["lock", "json"] {
        let path = root.record(suffix);
        std::os::unix::fs::symlink(&outside, &path).unwrap();
        assert!(
            with_project_location(&root.0, FIRST, &original, read_identity, || Ok(())).is_err()
        );
        assert_eq!(fs::read(&outside).unwrap(), b"keep these unrelated bytes");
        fs::remove_file(path).unwrap();
    }
}

#[cfg(windows)]
#[test]
fn a_windows_junction_cannot_redirect_the_registration_directory() {
    let root = Directory::new();
    let original = root.project("original", FIRST);
    let outside = root.0.join("outside");
    fs::create_dir(&outside).unwrap();
    let junction = root.0.join("project-locations");
    let result = std::process::Command::new("cmd")
        .args(["/C", "mklink", "/J"])
        .arg(&junction)
        .arg(&outside)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(with_project_location(&root.0, FIRST, &original, read_identity, || Ok(())).is_err());
    assert_eq!(fs::read_dir(outside).unwrap().count(), 0);
    fs::remove_dir(junction).unwrap();
}
