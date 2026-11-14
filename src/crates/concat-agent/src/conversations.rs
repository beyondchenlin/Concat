// SPDX-License-Identifier: AGPL-3.0-or-later
//! Native project bindings for original Pi conversation identities.
//!
//! This table contains no messages, task state, credentials or scheduling
//! logic. Pi remains their only owner. A recovered binding only identifies a
//! conversation to inspect; it never authorizes resuming that conversation.

use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use rusqlite::{Connection, OpenFlags, OptionalExtension, TransactionBehavior, params};

use crate::{OperationPermit, OwnerToken, SessionOwner};

/// Fixed file within the application's exclusively owned session directory.
pub const CONVERSATIONS_FILE_NAME: &str = "native-conversations.sqlite3";
const APPLICATION_ID: i32 = 0x424b4342;
const SCHEMA_VERSION: i32 = 1;
const MAX_CONVERSATION_ID: u64 = 9_007_199_254_740_991;

const SCHEMA: &[(&str, &str)] = &[
    ("conversation_bindings", "CREATE TABLE conversation_bindings (
        sequence INTEGER PRIMARY KEY AUTOINCREMENT,
        conversation_id INTEGER UNIQUE NOT NULL CHECK (conversation_id >= 0 AND conversation_id <= 9007199254740991),
        project_id TEXT NOT NULL,
        mode TEXT NOT NULL CHECK (mode IN ('chat','script','edit'))
    )"),
    ("conversation_bindings_target", "CREATE INDEX conversation_bindings_target ON conversation_bindings(project_id, mode, sequence)"),
    ("immutable_conversation_bindings_update", "CREATE TRIGGER immutable_conversation_bindings_update BEFORE UPDATE ON conversation_bindings
        BEGIN SELECT RAISE(ABORT, 'conversation bindings are immutable'); END"),
    ("immutable_conversation_bindings_delete", "CREATE TRIGGER immutable_conversation_bindings_delete BEFORE DELETE ON conversation_bindings
        BEGIN SELECT RAISE(ABORT, 'conversation bindings are immutable'); END"),
];

/// A refused binding never changes another project's conversation identity.
#[derive(Debug)]
pub enum BindingError {
    /// The owner or original generation is not authorized.
    Owner(crate::Error),
    /// A native file operation failed.
    Io(io::Error),
    /// A database operation failed.
    Database(rusqlite::Error),
    /// The file, exact schema, or immutable rows are not the reviewed format.
    InvalidStorage,
    /// Only a durable native project identity and reviewed task mode are valid.
    InvalidInput,
    /// This Pi identity was already assigned to a different project or mode.
    ConflictingBinding,
    /// The local connection lock failed.
    Poisoned,
}

impl fmt::Display for BindingError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Owner(_) => "会话归属操作已被原生所有权拒绝。",
            Self::Io(_) => "会话归属文件无法读取或保存。",
            Self::Database(_) => "会话归属记录未能完成存储操作。",
            Self::InvalidStorage => "会话归属存储格式或文件身份不一致，原记录保持不变。",
            Self::InvalidInput => "会话归属参数无效。",
            Self::ConflictingBinding => "该会话已属于另一工程或任务类型，不能改绑。",
            Self::Poisoned => "会话归属存储连接已中断。",
        })
    }
}

impl std::error::Error for BindingError {}
impl From<crate::Error> for BindingError {
    fn from(value: crate::Error) -> Self {
        Self::Owner(value)
    }
}
impl From<io::Error> for BindingError {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}
impl From<rusqlite::Error> for BindingError {
    fn from(value: rusqlite::Error) -> Self {
        Self::Database(value)
    }
}

/// One admitted connection. Connection and file guard close before its permit,
/// so a retired component cannot release ownership while storage remains live.
pub struct ConversationBindings {
    connection: Mutex<Connection>,
    file: File,
    path: PathBuf,
    permit: OperationPermit,
}

impl ConversationBindings {
    /// Open only after explicit native recovery approval. First creation is
    /// prepared under a private name and installed without replacing any file.
    pub fn open(owner: &SessionOwner, token: &OwnerToken) -> Result<Self, BindingError> {
        let permit = owner.begin(token)?;
        let path = binding_path(owner.storage_root())?;
        let (mut connection, file) = permit.with_valid(|| {
            if !path.try_exists()? {
                initialize(&path)?;
            }
            let file = guarded_file(&path, true)?;
            let connection = open_existing(&path, false)?;
            check_file_identity(&path, &file)?;
            Ok::<_, BindingError>((connection, file))
        })??;
        let stop = permit.stop_signal();
        connection.progress_handler(1_000, Some(move || stop.requested()))?;
        Ok(Self {
            connection: Mutex::new(connection),
            file,
            path,
            permit,
        })
    }

    /// A repeated identical binding is idempotent. It neither changes the
    /// original row nor moves an older conversation to the latest position.
    pub fn bind(
        &self,
        project_id: &str,
        mode: &str,
        conversation_id: u64,
    ) -> Result<(), BindingError> {
        validate_target(project_id, mode)?;
        if conversation_id > MAX_CONVERSATION_ID {
            return Err(BindingError::InvalidInput);
        }
        let conversation_id =
            i64::try_from(conversation_id).map_err(|_| BindingError::InvalidInput)?;
        let mut connection = self.connection.lock().map_err(|_| BindingError::Poisoned)?;
        self.permit.with_valid(|| {
            check_file_identity(&self.path, &self.file)?;
            let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let prior: Option<(String, String)> = transaction.query_row(
                "SELECT project_id, mode FROM conversation_bindings WHERE conversation_id=?1",
                [conversation_id], |row| Ok((row.get(0)?, row.get(1)?)),
            ).optional()?;
            match prior {
                Some((project, kind)) if project == project_id && kind == mode => {},
                Some(_) => return Err(BindingError::ConflictingBinding),
                None => {
                    transaction.execute(
                        "INSERT INTO conversation_bindings(conversation_id, project_id, mode) VALUES (?1, ?2, ?3)",
                        params![conversation_id, project_id, mode],
                    )?;
                }
            }
            transaction.commit()?;
            Ok(())
        })?
    }

    /// Read the latest original Pi conversation bound to this exact target.
    /// Reading never calls the original scheduler or restores a network grant.
    pub fn latest(&self, project_id: &str, mode: &str) -> Result<Option<u64>, BindingError> {
        validate_target(project_id, mode)?;
        let connection = self.connection.lock().map_err(|_| BindingError::Poisoned)?;
        self.permit.with_valid(|| {
            check_file_identity(&self.path, &self.file)?;
            read_latest(&connection, project_id, mode)
        })?
    }
}

/// Recovery-only inspection. Missing storage stays missing; existing storage
/// is opened read-only with no schema creation, migration or journal change.
pub fn inspect_latest(
    owner: &SessionOwner,
    token: &OwnerToken,
    project_id: &str,
    mode: &str,
) -> Result<Option<u64>, BindingError> {
    validate_target(project_id, mode)?;
    owner.with_owned(token, || ())?;
    let result = (|| {
        let path = binding_path(owner.storage_root())?;
        if !path.try_exists()? {
            return Ok(None);
        }
        let file = guarded_file(&path, false)?;
        let connection = open_existing(&path, true)?;
        let value = read_latest(&connection, project_id, mode)?;
        check_file_identity(&path, &file)?;
        Ok(value)
    })();
    owner.with_owned(token, || ())?;
    result
}

fn validate_target(project_id: &str, mode: &str) -> Result<(), BindingError> {
    let native = project_id.strip_prefix("bkcat-project:");
    if !native.is_some_and(|id| {
        id.len() == 32
            && id
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    }) || !matches!(mode, "chat" | "script" | "edit")
    {
        return Err(BindingError::InvalidInput);
    }
    Ok(())
}

fn read_latest(
    connection: &Connection,
    project_id: &str,
    mode: &str,
) -> Result<Option<u64>, BindingError> {
    let value: Option<i64> = connection.query_row(
        "SELECT conversation_id FROM conversation_bindings WHERE project_id=?1 AND mode=?2 ORDER BY sequence DESC LIMIT 1",
        params![project_id, mode], |row| row.get(0),
    ).optional()?;
    value
        .map(|value| {
            u64::try_from(value)
                .ok()
                .filter(|value| *value <= MAX_CONVERSATION_ID)
                .ok_or(BindingError::InvalidStorage)
        })
        .transpose()
}

fn binding_path(root: &Path) -> Result<PathBuf, BindingError> {
    let metadata = fs::symlink_metadata(root)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() || root.canonicalize()? != root {
        return Err(BindingError::InvalidStorage);
    }
    let path = root.join(CONVERSATIONS_FILE_NAME);
    validate_files(&path)?;
    Ok(path)
}

fn validate_files(path: &Path) -> Result<(), BindingError> {
    for suffix in ["", "-journal", "-wal", "-shm"] {
        let mut name = path.as_os_str().to_os_string();
        name.push(suffix);
        match fs::symlink_metadata(PathBuf::from(name)) {
            Ok(metadata) => {
                if !metadata.is_file() || metadata.file_type().is_symlink() {
                    return Err(BindingError::InvalidStorage);
                }
                #[cfg(unix)]
                {
                    use std::os::unix::fs::MetadataExt;
                    if metadata.nlink() != 1 {
                        return Err(BindingError::InvalidStorage);
                    }
                }
                #[cfg(windows)]
                {
                    use std::os::windows::fs::MetadataExt;
                    if metadata.file_attributes() & 0x400 != 0 {
                        return Err(BindingError::InvalidStorage);
                    }
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

fn guarded_file(path: &Path, writable: bool) -> Result<File, BindingError> {
    validate_files(path)?;
    let mut options = OpenOptions::new();
    options.read(true).write(writable);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        // Deny rename/delete until the SQLite connection has actually closed.
        options.share_mode(0x0000_0001 | 0x0000_0002);
    }
    let file = options.open(path)?;
    check_file_identity(path, &file)?;
    Ok(file)
}

fn check_file_identity(path: &Path, file: &File) -> Result<(), BindingError> {
    validate_files(path)?;
    let actual = fs::symlink_metadata(path)?;
    let opened = file.metadata()?;
    if !opened.is_file() || !actual.is_file() {
        return Err(BindingError::InvalidStorage);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if actual.dev() != opened.dev() || actual.ino() != opened.ino() {
            return Err(BindingError::InvalidStorage);
        }
    }
    Ok(())
}

fn open_existing(path: &Path, read_only: bool) -> Result<Connection, BindingError> {
    let mode = if read_only {
        OpenFlags::SQLITE_OPEN_READ_ONLY
    } else {
        OpenFlags::SQLITE_OPEN_READ_WRITE
    };
    let connection = Connection::open_with_flags(
        path,
        mode | OpenFlags::SQLITE_OPEN_NOFOLLOW | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    connection.busy_timeout(Duration::from_millis(250))?;
    connection.set_db_config(rusqlite::config::DbConfig::SQLITE_DBCONFIG_DEFENSIVE, true)?;
    connection.set_db_config(
        rusqlite::config::DbConfig::SQLITE_DBCONFIG_TRUSTED_SCHEMA,
        false,
    )?;
    validate_schema(&connection)?;
    if !read_only {
        connection.pragma_update(None, "synchronous", "FULL")?;
    }
    Ok(connection)
}

fn initialize(path: &Path) -> Result<(), BindingError> {
    let root = path.parent().ok_or(BindingError::InvalidStorage)?;
    let prepared = tempfile::NamedTempFile::new_in(root)?;
    {
        let mut connection = Connection::open_with_flags(
            prepared.path(),
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_NOFOLLOW
                | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        connection.pragma_update(None, "journal_mode", "DELETE")?;
        connection.pragma_update(None, "synchronous", "FULL")?;
        connection.pragma_update(None, "trusted_schema", false)?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        for (_, sql) in SCHEMA {
            transaction.execute_batch(sql)?;
        }
        transaction.pragma_update(None, "application_id", APPLICATION_ID)?;
        transaction.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        transaction.commit()?;
        validate_schema(&connection)?;
    }
    prepared.as_file().sync_all()?;
    let published = prepared
        .persist_noclobber(path)
        .map_err(|error| BindingError::Io(error.error))?;
    published.sync_all()?;
    #[cfg(unix)]
    File::open(root)?.sync_all()?;
    Ok(())
}

fn validate_schema(connection: &Connection) -> Result<(), BindingError> {
    let application: i32 =
        connection.pragma_query_value(None, "application_id", |row| row.get(0))?;
    let version: i32 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
    let journal: String = connection.pragma_query_value(None, "journal_mode", |row| row.get(0))?;
    if application != APPLICATION_ID || version != SCHEMA_VERSION || journal != "delete" {
        return Err(BindingError::InvalidStorage);
    }
    let count: i64 = connection.query_row(
        "SELECT count(*) FROM sqlite_schema WHERE name NOT GLOB 'sqlite_*'",
        [],
        |row| row.get(0),
    )?;
    if count != SCHEMA.len() as i64 {
        return Err(BindingError::InvalidStorage);
    }
    for (name, sql) in SCHEMA {
        let actual: Option<String> = connection
            .query_row(
                "SELECT sql FROM sqlite_schema WHERE name=?1",
                [name],
                |row| row.get(0),
            )
            .optional()?;
        if actual.as_deref() != Some(*sql) {
            return Err(BindingError::InvalidStorage);
        }
    }
    let integrity: String = connection.query_row("PRAGMA quick_check", [], |row| row.get(0))?;
    if integrity != "ok" {
        return Err(BindingError::InvalidStorage);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Phase, Reconciliation, RecoveryDecision};

    const PROJECT: &str = "bkcat-project:11111111111111111111111111111111";
    const OTHER: &str = "bkcat-project:22222222222222222222222222222222";

    fn approve(owner: &SessionOwner) {
        owner
            .confirm_recovery(
                &owner.token(),
                Reconciliation {
                    project_identity: PROJECT.into(),
                    project_revision: "1".into(),
                    session_revision: "1".into(),
                },
                RecoveryDecision::DiscardInterrupted,
            )
            .unwrap();
    }

    #[test]
    fn recovery_inspection_leaves_an_empty_store_empty_and_does_not_admit_writes() {
        let root = tempfile::tempdir().unwrap();
        let owner = SessionOwner::acquire(root.path()).unwrap();
        assert_eq!(
            inspect_latest(&owner, &owner.token(), PROJECT, "chat").unwrap(),
            None
        );
        assert!(!root.path().join(CONVERSATIONS_FILE_NAME).exists());
        assert!(matches!(
            ConversationBindings::open(&owner, &owner.token()),
            Err(BindingError::Owner(crate::Error::RecoveryRequired))
        ));
        assert_eq!(owner.status().unwrap().phase, Phase::RecoveryRequired);
        assert_eq!(owner.status().unwrap().active_operations, 0);
    }

    #[test]
    fn original_conversation_is_immutable_and_duplicate_binding_does_not_reorder() {
        let root = tempfile::tempdir().unwrap();
        let owner = SessionOwner::acquire(root.path()).unwrap();
        approve(&owner);
        let bindings = ConversationBindings::open(&owner, &owner.token()).unwrap();
        bindings.bind(PROJECT, "chat", 0).unwrap();
        bindings.bind(PROJECT, "chat", 8).unwrap();
        bindings.bind(PROJECT, "script", 9).unwrap();
        bindings.bind(PROJECT, "chat", 0).unwrap();
        assert_eq!(bindings.latest(PROJECT, "chat").unwrap(), Some(8));
        assert_eq!(bindings.latest(PROJECT, "script").unwrap(), Some(9));
        assert_eq!(bindings.latest(PROJECT, "edit").unwrap(), None);
        for (project, mode) in [(OTHER, "chat"), (PROJECT, "script")] {
            assert!(matches!(
                bindings.bind(project, mode, 0),
                Err(BindingError::ConflictingBinding)
            ));
        }
        assert_eq!(bindings.latest(OTHER, "chat").unwrap(), None);
    }

    #[test]
    fn reopens_bindings_without_messages_or_automatic_recovery_and_keeps_owner_until_closed() {
        let root = tempfile::tempdir().unwrap();
        let owner = SessionOwner::acquire(root.path()).unwrap();
        approve(&owner);
        let bindings = ConversationBindings::open(&owner, &owner.token()).unwrap();
        bindings.bind(PROJECT, "chat", 7).unwrap();
        owner.request_stop();
        assert!(bindings.bind(PROJECT, "chat", 8).is_err());
        assert!(bindings.latest(PROJECT, "chat").is_err());
        assert!(matches!(
            owner.finish_handoff(Duration::ZERO),
            Err(crate::Error::DrainTimeout {
                active_operations: 1
            })
        ));
        assert!(matches!(
            SessionOwner::acquire(root.path()),
            Err(crate::Error::Busy)
        ));
        drop(bindings);
        owner.finish_handoff(Duration::ZERO).unwrap();
        let next = SessionOwner::acquire(root.path()).unwrap();
        let before = fs::read(root.path().join(CONVERSATIONS_FILE_NAME)).unwrap();
        assert_eq!(
            inspect_latest(&next, &next.token(), PROJECT, "chat").unwrap(),
            Some(7)
        );
        assert_eq!(
            fs::read(root.path().join(CONVERSATIONS_FILE_NAME)).unwrap(),
            before
        );
        assert_eq!(next.status().unwrap().phase, Phase::RecoveryRequired);
        assert_eq!(next.status().unwrap().active_operations, 0);
        assert!(inspect_latest(&next, &owner.token(), PROJECT, "chat").is_err());
    }

    #[test]
    fn unexpected_schema_and_existing_empty_file_are_never_initialized_or_replaced() {
        for contents in [b"".as_slice(), b"not a database".as_slice()] {
            let root = tempfile::tempdir().unwrap();
            let path = root.path().join(CONVERSATIONS_FILE_NAME);
            fs::write(&path, contents).unwrap();
            let owner = SessionOwner::acquire(root.path()).unwrap();
            assert!(inspect_latest(&owner, &owner.token(), PROJECT, "chat").is_err());
            approve(&owner);
            assert!(ConversationBindings::open(&owner, &owner.token()).is_err());
            assert_eq!(fs::read(path).unwrap(), contents);
        }
        let root = tempfile::tempdir().unwrap();
        let owner = SessionOwner::acquire(root.path()).unwrap();
        approve(&owner);
        drop(ConversationBindings::open(&owner, &owner.token()).unwrap());
        let path = root.path().join(CONVERSATIONS_FILE_NAME);
        let connection = Connection::open(&path).unwrap();
        connection
            .execute_batch("CREATE TABLE unreviewed(data TEXT)")
            .unwrap();
        drop(connection);
        let before = fs::read(&path).unwrap();
        assert!(ConversationBindings::open(&owner, &owner.token()).is_err());
        assert_eq!(fs::read(path).unwrap(), before);
    }

    #[test]
    fn invalid_targets_and_ids_do_not_create_bindings() {
        let root = tempfile::tempdir().unwrap();
        let owner = SessionOwner::acquire(root.path()).unwrap();
        approve(&owner);
        let bindings = ConversationBindings::open(&owner, &owner.token()).unwrap();
        for (project, mode, id) in [
            ("path/project", "chat", 1),
            (PROJECT, "admin", 2),
            (PROJECT, "chat", MAX_CONVERSATION_ID + 1),
        ] {
            assert!(matches!(
                bindings.bind(project, mode, id),
                Err(BindingError::InvalidInput)
            ));
        }
        assert_eq!(bindings.latest(PROJECT, "chat").unwrap(), None);
        // The largest original Pi safe integer must round-trip through
        // SQLite's signed storage without truncation or narrowing.
        bindings.bind(PROJECT, "chat", MAX_CONVERSATION_ID).unwrap();
        assert_eq!(
            bindings.latest(PROJECT, "chat").unwrap(),
            Some(MAX_CONVERSATION_ID)
        );
        drop(bindings);
        let reopened = ConversationBindings::open(&owner, &owner.token()).unwrap();
        assert_eq!(
            reopened.latest(PROJECT, "chat").unwrap(),
            Some(MAX_CONVERSATION_ID)
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlink_alias_and_replaced_live_database_are_refused() {
        use std::os::unix::fs::symlink;
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join(CONVERSATIONS_FILE_NAME);
        let foreign = root.path().join("foreign");
        fs::write(&foreign, b"foreign data").unwrap();
        symlink(&foreign, &path).unwrap();
        let owner = SessionOwner::acquire(root.path()).unwrap();
        assert!(inspect_latest(&owner, &owner.token(), PROJECT, "chat").is_err());
        approve(&owner);
        assert!(ConversationBindings::open(&owner, &owner.token()).is_err());
        assert_eq!(fs::read(&foreign).unwrap(), b"foreign data");
        fs::remove_file(&path).unwrap();
        let bindings = ConversationBindings::open(&owner, &owner.token()).unwrap();
        bindings.bind(PROJECT, "chat", 1).unwrap();
        fs::rename(&path, root.path().join("original.sqlite3")).unwrap();
        fs::write(&path, b"replacement").unwrap();
        assert!(bindings.bind(PROJECT, "chat", 2).is_err());
        assert!(bindings.latest(PROJECT, "chat").is_err());
        assert_eq!(fs::read(path).unwrap(), b"replacement");
    }
}
