// SPDX-License-Identifier: AGPL-3.0-or-later
//! Immutable native authorization metadata for one original Pi submission.
//! This contains no scheduler state or credential value. Pi remains the only
//! task/message store; the native file only lets a later explicit recovery
//! verify the model, language and project choices that authorized that work.

use crate::{OwnerToken, SessionOwner};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
#[cfg(unix)]
use std::fs::File;
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

/// Directory within the exclusive native session root.
pub const SNAPSHOTS_DIRECTORY: &str = "native-task-snapshots-v1";
const MAX_SNAPSHOT_BYTES: u64 = 2 * 1024 * 1024;

/// The immutable binding of native metadata to an original durable submission.
#[derive(Clone, Debug, Serialize, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TaskSnapshot {
    /// Reviewed host metadata format.
    pub schema_version: u32,
    /// The native action's original Pi requestId, never a new recovery id.
    pub request_id: String,
    /// Original Pi conversation assigned before submit can run.
    pub conversation_id: u64,
    /// Native persistent project identity.
    pub project_id: String,
    /// Verified native project location at submission.
    pub project_path: String,
    /// Host-owned, non-secret model/language/script authorization snapshot.
    pub metadata: Value,
}

impl TaskSnapshot {
    fn validate(&self) -> Result<(), String> {
        if self.schema_version != 1
            || self.request_id.is_empty()
            || self.request_id.len() > 256
            || self.request_id.chars().any(char::is_control)
            || self.conversation_id > 9_007_199_254_740_991
            || self.project_id.is_empty()
            || self.project_id.len() > 256
            || self.project_path.is_empty()
            || self.project_path.len() > 32_768
            || !self.metadata.is_object()
        {
            return Err("agent.errorRecoverySnapshotInvalid".into());
        }
        Ok(())
    }
}

fn folder(root: &Path) -> Result<PathBuf, String> {
    let folder = root.join(SNAPSHOTS_DIRECTORY);
    match fs::symlink_metadata(&folder) {
        Ok(metadata) => {
            if !metadata.is_dir() || metadata.file_type().is_symlink() || reparse(&metadata) {
                return Err("agent.errorRecoverySnapshotInvalid".into());
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(_) => return Err("agent.errorRecoverySnapshotRead".into()),
    }
    Ok(folder)
}
fn file_path(root: &Path, request_id: &str) -> Result<PathBuf, String> {
    if request_id.is_empty() || request_id.len() > 256 || request_id.chars().any(char::is_control) {
        return Err("agent.errorRecoverySnapshotInvalid".into());
    }
    Ok(folder(root)?.join(format!("{:x}.json", Sha256::digest(request_id.as_bytes()))))
}
fn reparse(metadata: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        metadata.file_attributes() & 0x400 != 0
    }
    #[cfg(not(windows))]
    {
        let _ = metadata;
        false
    }
}
fn read(path: &Path) -> Result<Option<TaskSnapshot>, String> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(value) => value,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err("agent.errorRecoverySnapshotRead".into()),
    };
    if !metadata.is_file()
        || metadata.file_type().is_symlink()
        || reparse(&metadata)
        || metadata.len() > MAX_SNAPSHOT_BYTES
        || metadata.len() == 0
    {
        return Err("agent.errorRecoverySnapshotInvalid".into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.nlink() != 1 {
            return Err("agent.errorRecoverySnapshotInvalid".into());
        }
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        // Retain the existing snapshot while it is read.
        options.share_mode(0x0000_0001 | 0x0000_0002);
    }
    let file = options
        .open(path)
        .map_err(|_| "agent.errorRecoverySnapshotRead")?;
    let opened = file
        .metadata()
        .map_err(|_| "agent.errorRecoverySnapshotRead")?;
    if !opened.is_file() || opened.len() != metadata.len() || reparse(&opened) {
        return Err("agent.errorRecoverySnapshotInvalid".into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if opened.dev() != metadata.dev() || opened.ino() != metadata.ino() {
            return Err("agent.errorRecoverySnapshotInvalid".into());
        }
    }
    let mut bytes = Vec::new();
    file.take(MAX_SNAPSHOT_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "agent.errorRecoverySnapshotRead")?;
    if bytes.len() as u64 != metadata.len() {
        return Err("agent.errorRecoverySnapshotInvalid".into());
    }
    let snapshot: TaskSnapshot =
        serde_json::from_slice(&bytes).map_err(|_| "agent.errorRecoverySnapshotInvalid")?;
    snapshot.validate()?;
    Ok(Some(snapshot))
}

/// Read only; a legacy task without this metadata remains explicitly unknown.
/// It cannot acquire today's model/languages by falling back to preferences.
pub fn inspect(
    owner: &SessionOwner,
    token: &OwnerToken,
    request_id: &str,
) -> Result<Option<TaskSnapshot>, String> {
    owner
        .with_owned(token, || ())
        .map_err(|error| error.to_string())?;
    let result = read(&file_path(owner.storage_root(), request_id)?);
    owner
        .with_owned(token, || ())
        .map_err(|error| error.to_string())?;
    let snapshot = result?;
    if snapshot
        .as_ref()
        .is_some_and(|item| item.request_id != request_id)
    {
        return Err("agent.errorRecoverySnapshotInvalid".into());
    }
    Ok(snapshot)
}

/// Persist before original Pi submit. A repeated identical bind is idempotent;
/// a conflicting bind is refused and the first immutable metadata is retained.
/// Slow writes do not hold the ownership gate; publication rechecks the permit.
pub fn persist(
    owner: &SessionOwner,
    token: &OwnerToken,
    snapshot: &TaskSnapshot,
) -> Result<(), String> {
    snapshot.validate()?;
    let permit = owner.begin(token).map_err(|error| error.to_string())?;
    let path = file_path(owner.storage_root(), &snapshot.request_id)?;
    if let Some(previous) = read(&path)? {
        return if previous == *snapshot {
            Ok(())
        } else {
            Err("agent.errorRecoverySnapshotConflict".into())
        };
    }
    let bytes = serde_json::to_vec(snapshot).map_err(|_| "agent.errorRecoverySnapshotInvalid")?;
    if bytes.len() as u64 > MAX_SNAPSHOT_BYTES {
        return Err("agent.errorRecoverySnapshotInvalid".into());
    }
    let directory = path.parent().expect("snapshot folder");
    permit
        .with_valid(|| fs::create_dir_all(directory))
        .map_err(|error| error.to_string())?
        .map_err(|_| "agent.errorRecoverySnapshotWrite")?;
    // create_new and an unpredictable suffix keep abandoned partial files
    // isolated; a hard link publishes without ever replacing an existing file.
    let mut nonce = [0u8; 16];
    getrandom::fill(&mut nonce).map_err(|_| "agent.errorRecoverySnapshotWrite")?;
    let suffix = nonce
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let temporary = directory.join(format!(".pending-{suffix}"));
    let result = (|| -> Result<(), String> {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options
            .open(&temporary)
            .map_err(|_| "agent.errorRecoverySnapshotWrite")?;
        for block in bytes.chunks(64 * 1024) {
            permit
                .with_valid(|| ())
                .map_err(|error| error.to_string())?;
            file.write_all(block)
                .map_err(|_| "agent.errorRecoverySnapshotWrite")?;
        }
        file.sync_all()
            .map_err(|_| "agent.errorRecoverySnapshotWrite")?;
        drop(file);
        permit
            .with_valid(|| fs::hard_link(&temporary, &path))
            .map_err(|error| error.to_string())?
            .map_err(|_| "agent.errorRecoverySnapshotConflict")?;
        fs::remove_file(&temporary).map_err(|_| "agent.errorRecoverySnapshotWrite")?;
        #[cfg(unix)]
        {
            File::open(directory)
                .and_then(|file| file.sync_all())
                .map_err(|_| "agent.errorRecoverySnapshotWrite")?;
        }
        permit
            .with_valid(|| ())
            .map_err(|error| error.to_string())?;
        Ok(())
    })();
    let _ = fs::remove_file(&temporary);
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Reconciliation, RecoveryDecision};
    fn sample() -> TaskSnapshot {
        TaskSnapshot {
            schema_version: 1,
            request_id: "native-action-1".into(),
            conversation_id: 7,
            project_id: "bkcat-project:00000000000000000000000000000001".into(),
            project_path: "project".into(),
            metadata: serde_json::json!({"languages":{"replyLanguage":"ja"},"modelConfiguration":{"id":"connection-one"}}),
        }
    }
    #[test]
    fn missing_metadata_stays_missing_during_readonly_recovery() {
        let root = tempfile::tempdir().unwrap();
        let owner = SessionOwner::acquire(root.path()).unwrap();
        assert!(
            inspect(&owner, &owner.token(), "old-task")
                .unwrap()
                .is_none()
        );
        assert!(!root.path().join(SNAPSHOTS_DIRECTORY).exists());
        assert!(persist(&owner, &owner.token(), &sample()).is_err());
    }
    #[test]
    fn immutable_metadata_survives_reopen_and_rejects_conflicting_binding() {
        let root = tempfile::tempdir().unwrap();
        let owner = SessionOwner::acquire(root.path()).unwrap();
        let token = owner.token();
        owner
            .confirm_recovery(
                &token,
                Reconciliation {
                    project_identity: "p".into(),
                    project_revision: "1".into(),
                    session_revision: "s".into(),
                },
                RecoveryDecision::ContinueAfterRevalidation,
            )
            .unwrap();
        let first = sample();
        persist(&owner, &token, &first).unwrap();
        persist(&owner, &token, &first).unwrap();
        let mut changed = first.clone();
        changed.metadata["languages"]["replyLanguage"] = serde_json::json!("en");
        assert!(persist(&owner, &token, &changed).is_err());
        assert_eq!(
            inspect(&owner, &token, &first.request_id).unwrap(),
            Some(first.clone())
        );
        owner.revoke();
        owner.finish_handoff(std::time::Duration::ZERO).unwrap();
        drop(owner);
        let next = SessionOwner::acquire(root.path()).unwrap();
        assert_eq!(
            inspect(&next, &next.token(), &first.request_id).unwrap(),
            Some(first)
        );
    }
    #[test]
    fn corrupt_or_misnamed_metadata_is_not_reconstructed_from_current_settings() {
        let root = tempfile::tempdir().unwrap();
        let owner = SessionOwner::acquire(root.path()).unwrap();
        let path = file_path(root.path(), "old-task").unwrap();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, b"{").unwrap();
        assert!(inspect(&owner, &owner.token(), "old-task").is_err());
        fs::write(path, serde_json::to_vec(&sample()).unwrap()).unwrap();
        assert!(inspect(&owner, &owner.token(), "old-task").is_err());
    }
}
