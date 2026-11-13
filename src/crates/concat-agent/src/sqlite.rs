// SPDX-License-Identifier: AGPL-3.0-or-later
//! Native SQLite port for the pinned original Pi `SqliteDatabase` interface.
//!
//! This module is a database driver, not a session store or scheduler. Pi owns
//! its schema and persistence semantics. The host routes an opaque owner handle
//! here only after native reconciliation. No caller supplies a filesystem path.
//! A writable connection retains one operation permit until rollback and close
//! finish; every statement and explicit commit re-enters that permit's gate.
//! The browser adapter queues operations. Native transaction identities also
//! reject an unrelated operation instead of silently joining an active transaction.

use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use fallible_iterator::FallibleIterator;
use rusqlite::config::DbConfig;
use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
use rusqlite::types::{Value as SqlValue, ValueRef};
use rusqlite::{Batch, Connection, OpenFlags, OptionalExtension, params_from_iter};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::{OperationPermit, OwnerToken, SessionOwner};

/// One application-owned file under the canonical ownership directory.
pub const DATABASE_FILE_NAME: &str = "pi-session.sqlite3";
/// Schema version in Pi 1.5.2 at the reviewed, pinned source commit.
pub const PI_SCHEMA_VERSION: u32 = 2;

const MAX_SAFE_INTEGER: i64 = 9_007_199_254_740_991;
const MAX_SQL_BYTES: usize = 1024 * 1024;
const MAX_VALUE_BYTES: usize = 16 * 1024 * 1024;
const MAX_RESULT_ROWS: usize = 100_000;
const MAX_PROGRESS_CALLBACKS: u32 = 10_000;
const REQUIRED_PI_TABLES: [&str; 9] = [
    "durable_schema",
    "durable_metadata",
    "record_ids",
    "conversations",
    "entries",
    "tasks",
    "submissions",
    "documents",
    "document_revisions",
];
static NEXT_DATABASE: AtomicU64 = AtomicU64::new(1);

/// Reviewed methods of the native database port.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, Eq, PartialEq)]
pub enum SqliteMethod {
    /// Open the fixed database after native ownership admission.
    #[serde(rename = "sqlite.open")]
    Open,
    /// Begin a connection transaction and issue its native identity.
    #[serde(rename = "sqlite.begin")]
    Begin,
    /// Execute SQL without bindings, including multiple statements.
    #[serde(rename = "sqlite.exec")]
    Exec,
    /// Execute a single statement with positional bindings.
    #[serde(rename = "sqlite.run")]
    Run,
    /// Execute a single statement and return SQLite's affected-row count.
    #[serde(rename = "sqlite.runCounted")]
    RunCounted,
    /// Return the first result row, or null when absent.
    #[serde(rename = "sqlite.get")]
    Get,
    /// Return all result rows.
    #[serde(rename = "sqlite.all")]
    All,
    /// Commit this connection's matching transaction while still admitted.
    #[serde(rename = "sqlite.commit")]
    Commit,
    /// Roll back the matching transaction, including after revocation.
    #[serde(rename = "sqlite.rollback")]
    Rollback,
    /// Roll back and close the connection, including after revocation.
    #[serde(rename = "sqlite.close")]
    Close,
}

/// The exact frame shared with `agent-runtime/src/sqlite-port.ts`.
/// Unknown fields, including paths, are refused during deserialization.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SqliteRequest {
    /// Native database operation.
    pub method: SqliteMethod,
    /// Opaque handle issued and routed by the trusted native host.
    pub owner_handle: String,
    /// Decimal durable generation, never a JavaScript floating-point number.
    pub generation: String,
    /// Native connection identity, absent only for opening.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub database_id: Option<String>,
    /// Native transaction identity, required for every statement in a transaction.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transaction_id: Option<String>,
    /// Expected pinned Pi schema, present only for opening.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_schema_version: Option<u32>,
    /// Requested initialization; the native grant remains the upper bound.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow_initialize_empty_store: Option<bool>,
    /// SQL text for statement operations only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sql: Option<String>,
    /// Positional values, using tagged 64-bit integers and Base64 blobs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Vec<Value>>,
}

/// Native-reviewed options; a page's flag cannot grant initialization by itself.
#[derive(Clone, Copy, Debug)]
pub struct SqliteOpenOptions {
    /// Must equal the reviewed Pi schema version.
    pub expected_schema_version: u32,
    /// A native decision allowed initializing an empty store.
    pub allow_initialize_empty_store: bool,
}

/// A refused database operation. None of these errors grants a retry or resume.
#[derive(Debug)]
pub enum SqliteError {
    /// Native ownership or recovery admission failed.
    Ownership(crate::Error),
    /// SQLite refused an operation.
    Database(rusqlite::Error),
    /// Inspecting the fixed application-owned files failed.
    Io(std::io::Error),
    /// The frame or a value does not follow the reviewed protocol.
    InvalidRequest(&'static str),
    /// The handle does not route to this connection's native owner.
    WrongOwner,
    /// The frame belongs to another generation.
    StaleGeneration,
    /// The frame names another connection.
    WrongDatabase,
    /// A transaction is absent, foreign, expired, or has already rolled back.
    WrongTransaction,
    /// The connection has already closed.
    Closed,
    /// An empty or absent store needs an explicit native initialization grant.
    InitializationRequired,
    /// An existing store is not the pinned schema; it is never migrated here.
    SchemaMismatch {
        /// Existing schema version, or none for unrecognized metadata.
        found: Option<i64>,
    },
    /// The fixed file or a SQLite sidecar is not an ordinary local file.
    InvalidStorage,
    /// A bounded request, value, result, or identity counter exceeded its limit.
    ResourceLimit,
}

impl SqliteError {
    /// Stable native error code for a structured bridge rejection.
    pub fn code(&self) -> &'static str {
        match self {
            Self::Ownership(_) => "storage_owner_refused",
            Self::Database(_) => "storage_sqlite_error",
            Self::Io(_) => "storage_io_error",
            Self::InvalidRequest(_) => "storage_invalid_request",
            Self::WrongOwner => "storage_wrong_owner",
            Self::StaleGeneration => "storage_stale_generation",
            Self::WrongDatabase => "storage_wrong_database",
            Self::WrongTransaction => "storage_wrong_transaction",
            Self::Closed => "storage_closed",
            Self::InitializationRequired => "storage_initialization_required",
            Self::SchemaMismatch { .. } => "storage_schema_mismatch",
            Self::InvalidStorage => "storage_invalid_file",
            Self::ResourceLimit => "storage_resource_limit",
        }
    }
}

impl fmt::Display for SqliteError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Ownership(error) => write!(formatter, "storage ownership refused: {error}"),
            Self::Database(error) => write!(formatter, "SQLite operation failed: {error}"),
            Self::Io(error) => write!(formatter, "session storage inspection failed: {error}"),
            Self::InvalidRequest(reason) => formatter.write_str(reason),
            Self::WrongOwner => formatter.write_str("SQLite owner handle does not match"),
            Self::StaleGeneration => formatter.write_str("SQLite request belongs to another host generation"),
            Self::WrongDatabase => formatter.write_str("SQLite connection identity does not match"),
            Self::WrongTransaction => formatter.write_str("SQLite transaction identity or state does not match"),
            Self::Closed => formatter.write_str("SQLite connection is closed"),
            Self::InitializationRequired => formatter.write_str("initializing an empty session store requires a native user decision"),
            Self::SchemaMismatch { found } => write!(formatter, "session schema {found:?} does not match pinned schema {PI_SCHEMA_VERSION}; no migration was run"),
            Self::InvalidStorage => formatter.write_str("session database and sidecars must be ordinary files in the owned storage directory"),
            Self::ResourceLimit => formatter.write_str("SQLite request or result exceeds the native port limit"),
        }
    }
}

impl std::error::Error for SqliteError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Ownership(error) => Some(error),
            Self::Database(error) => Some(error),
            Self::Io(error) => Some(error),
            _ => None,
        }
    }
}

impl From<crate::Error> for SqliteError {
    fn from(error: crate::Error) -> Self {
        Self::Ownership(error)
    }
}
impl From<rusqlite::Error> for SqliteError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Database(error)
    }
}
impl From<std::io::Error> for SqliteError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

#[derive(Default)]
struct AuthorizationState {
    control: AtomicBool,
    configuration: AtomicBool,
    alter_table: AtomicBool,
    progress: AtomicU32,
}

impl AuthorizationState {
    fn new_statement(&self) {
        self.alter_table.store(false, Ordering::Relaxed);
        self.progress.store(0, Ordering::Relaxed);
    }
}

/// A single native connection. Keep it on a background worker or behind a mutex;
/// SQLite work must not block the native UI thread.
pub struct NativeSqlite {
    connection: Option<Connection>,
    // This field is explicitly released only after the connection is closed.
    permit: Option<OperationPermit>,
    owner_handle: String,
    generation: String,
    database_id: String,
    active_transaction: Option<String>,
    next_transaction: u64,
    authorization: Arc<AuthorizationState>,
}

impl NativeSqlite {
    /// Validate a page's opening frame against the actual native handle, token,
    /// and initialization decision before constructing a writable connection.
    pub fn open_request(
        owner: &SessionOwner,
        token: &OwnerToken,
        owner_handle: &str,
        native_allows_initialization: bool,
        request: &SqliteRequest,
    ) -> Result<Self, SqliteError> {
        if request.method != SqliteMethod::Open
            || request.database_id.is_some()
            || request.transaction_id.is_some()
            || request.sql.is_some()
            || request.params.is_some()
        {
            return Err(SqliteError::InvalidRequest("invalid SQLite opening frame"));
        }
        check_identity(owner_handle, token.generation(), request)?;
        let requested = request
            .allow_initialize_empty_store
            .ok_or(SqliteError::InvalidRequest(
                "SQLite initialization choice is missing",
            ))?;
        if requested && !native_allows_initialization {
            return Err(SqliteError::InitializationRequired);
        }
        Self::open(
            owner,
            token,
            owner_handle,
            SqliteOpenOptions {
                expected_schema_version: request.expected_schema_version.ok_or(
                    SqliteError::InvalidRequest("SQLite expected schema version is missing"),
                )?,
                allow_initialize_empty_store: requested && native_allows_initialization,
            },
        )
    }

    /// Open only the fixed file inside this owner's canonical directory.
    /// `options` and `owner_handle` come from trusted native admission; page
    /// frames should use `open_request` instead of bypassing that validation.
    pub fn open(
        owner: &SessionOwner,
        token: &OwnerToken,
        owner_handle: &str,
        options: SqliteOpenOptions,
    ) -> Result<Self, SqliteError> {
        if owner_handle.is_empty() || options.expected_schema_version != PI_SCHEMA_VERSION {
            return Err(SqliteError::InvalidRequest(
                "unreviewed SQLite schema or empty native owner handle",
            ));
        }
        let identity = NEXT_DATABASE
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
                value.checked_add(1)
            })
            .map_err(|_| SqliteError::ResourceLimit)?;
        let permit = owner.begin(token)?;
        let authorization = Arc::new(AuthorizationState::default());
        let connection = permit.with_valid(|| {
            open_connection(owner.storage_root(), options, Arc::clone(&authorization))
        })??;
        Ok(Self {
            connection: Some(connection),
            permit: Some(permit),
            owner_handle: owner_handle.to_owned(),
            generation: token.generation().to_string(),
            database_id: format!("sqlite-{}-{identity}", token.generation()),
            active_transaction: None,
            next_transaction: 1,
            authorization,
        })
    }

    /// Native connection identity returned by `sqlite.open`.
    pub fn database_id(&self) -> &str {
        &self.database_id
    }

    /// Process a fully routed frame. No operation is retried after an error.
    pub fn handle(&mut self, request: &SqliteRequest) -> Result<Value, SqliteError> {
        if request.owner_handle != self.owner_handle {
            return Err(SqliteError::WrongOwner);
        }
        if request.generation != self.generation {
            return Err(SqliteError::StaleGeneration);
        }
        if request.database_id.as_deref() != Some(self.database_id.as_str()) {
            return Err(SqliteError::WrongDatabase);
        }
        if request.expected_schema_version.is_some()
            || request.allow_initialize_empty_store.is_some()
        {
            return Err(SqliteError::InvalidRequest(
                "SQLite opening options cannot accompany an existing connection",
            ));
        }
        if request.method == SqliteMethod::Open {
            return Err(SqliteError::InvalidRequest(
                "SQLite connection is already open",
            ));
        }
        let is_statement = matches!(
            request.method,
            SqliteMethod::Exec
                | SqliteMethod::Run
                | SqliteMethod::RunCounted
                | SqliteMethod::Get
                | SqliteMethod::All
        );
        if !is_statement && (request.sql.is_some() || request.params.is_some()) {
            return Err(SqliteError::InvalidRequest(
                "SQLite control frames cannot carry SQL or bindings",
            ));
        }
        match request.method {
            SqliteMethod::Close => {
                if request.transaction_id.is_some() {
                    return Err(SqliteError::WrongTransaction);
                }
                self.close()?;
                Ok(Value::Null)
            }
            SqliteMethod::Rollback => {
                self.rollback(request.transaction_id.as_deref())?;
                Ok(Value::Null)
            }
            SqliteMethod::Begin => {
                if request.transaction_id.is_some() {
                    return Err(SqliteError::WrongTransaction);
                }
                self.begin()
                    .map(|transaction_id| json!({ "transactionId": transaction_id }))
            }
            SqliteMethod::Commit => {
                self.commit(request.transaction_id.as_deref())?;
                Ok(Value::Null)
            }
            _ => self.statement(request),
        }
    }

    /// Roll back unfinished work and close before releasing the lifetime permit.
    /// Cleanup remains legal after revocation and never checkpoints or commits.
    pub fn close(&mut self) -> Result<(), SqliteError> {
        let Some(connection) = self.connection.as_ref() else {
            return Ok(());
        };
        if !connection.is_autocommit() {
            control(connection, &self.authorization, "ROLLBACK")?;
        }
        self.active_transaction = None;
        let connection = self.connection.take().ok_or(SqliteError::Closed)?;
        match connection.close() {
            Ok(()) => {
                drop(self.permit.take());
                Ok(())
            }
            Err((connection, error)) => {
                // A close error must not release ownership while SQLite lives.
                self.connection = Some(connection);
                Err(error.into())
            }
        }
    }

    fn connection(&self) -> Result<&Connection, SqliteError> {
        self.connection.as_ref().ok_or(SqliteError::Closed)
    }

    fn permit(&self) -> Result<&OperationPermit, SqliteError> {
        self.permit.as_ref().ok_or(SqliteError::Closed)
    }

    fn check_transaction(&self, transaction: Option<&str>) -> Result<(), SqliteError> {
        if self.active_transaction.as_deref() != transaction {
            return Err(SqliteError::WrongTransaction);
        }
        // INSERT OR ROLLBACK and some SQLite failures can end a transaction.
        // Do not let a still-live page handle start implicit writes afterward.
        if self.connection()?.is_autocommit() != self.active_transaction.is_none() {
            return Err(SqliteError::WrongTransaction);
        }
        Ok(())
    }

    fn begin(&mut self) -> Result<String, SqliteError> {
        self.check_transaction(None)?;
        let next = self
            .next_transaction
            .checked_add(1)
            .ok_or(SqliteError::ResourceLimit)?;
        let id = format!("{}-tx-{}", self.database_id, self.next_transaction);
        self.permit()?
            .with_valid(|| control(self.connection()?, &self.authorization, "BEGIN IMMEDIATE"))??;
        self.next_transaction = next;
        self.active_transaction = Some(id.clone());
        Ok(id)
    }

    fn commit(&mut self, transaction: Option<&str>) -> Result<(), SqliteError> {
        if transaction.is_none() {
            return Err(SqliteError::WrongTransaction);
        }
        self.check_transaction(transaction)?;
        self.permit()?
            .with_valid(|| control(self.connection()?, &self.authorization, "COMMIT"))??;
        self.active_transaction = None;
        Ok(())
    }

    fn rollback(&mut self, transaction: Option<&str>) -> Result<(), SqliteError> {
        if transaction.is_none() || self.active_transaction.as_deref() != transaction {
            return Err(SqliteError::WrongTransaction);
        }
        let connection = self.connection()?;
        if !connection.is_autocommit() {
            control(connection, &self.authorization, "ROLLBACK")?;
        }
        self.active_transaction = None;
        Ok(())
    }

    fn statement(&self, request: &SqliteRequest) -> Result<Value, SqliteError> {
        self.check_transaction(request.transaction_id.as_deref())?;
        let sql = request
            .sql
            .as_deref()
            .ok_or(SqliteError::InvalidRequest("SQLite statement is missing"))?;
        if sql.trim().is_empty() || sql.as_bytes().contains(&0) {
            return Err(SqliteError::InvalidRequest(
                "SQLite statement is empty or contains a NUL byte",
            ));
        }
        if sql.len() > MAX_SQL_BYTES {
            return Err(SqliteError::ResourceLimit);
        }
        let params = request.params.as_deref().unwrap_or_default();
        if params.len() > 32_766
            || params
                .iter()
                .map(value_size)
                .try_fold(0_usize, |total, size| total.checked_add(size))
                .is_none_or(|total| total > MAX_VALUE_BYTES)
        {
            return Err(SqliteError::ResourceLimit);
        }
        if request.method == SqliteMethod::Exec && !params.is_empty() {
            return Err(SqliteError::InvalidRequest(
                "SQLite exec does not accept bindings",
            ));
        }
        let params = params
            .iter()
            .map(decode_value)
            .collect::<Result<Vec<_>, _>>()?;
        let connection = self.connection()?;
        let permit = self.permit()?;
        if request.method == SqliteMethod::Exec {
            let mut batch = Batch::new(connection, sql);
            loop {
                // Preparing can also invoke the authorizer. Keep preparation,
                // execution, and statement destruction inside the same gate.
                let more = permit.with_valid(|| -> Result<bool, SqliteError> {
                    self.authorization.new_statement();
                    let Some(mut statement) = batch.next()? else {
                        return Ok(false);
                    };
                    let mut rows = statement.query([])?;
                    while rows.next()?.is_some() {}
                    Ok(true)
                })??;
                if !more {
                    break;
                }
            }
            return Ok(Value::Null);
        }
        permit.with_valid(|| -> Result<Value, SqliteError> {
            self.authorization.new_statement();
            // rusqlite 0.42 rejects a second statement in prepare itself;
            // nothing from a multi-statement run/get/all is executed first.
            let mut statement = connection.prepare(sql)?;
            match request.method {
                SqliteMethod::Run | SqliteMethod::RunCounted => {
                    let changes = statement.execute(params_from_iter(params.iter()))?;
                    if request.method == SqliteMethod::Run {
                        Ok(Value::Null)
                    } else {
                        Ok(json!(changes))
                    }
                }
                SqliteMethod::Get | SqliteMethod::All => {
                    let names: Vec<String> = statement.column_names().map(str::to_owned).collect();
                    let mut rows = statement.query(params_from_iter(params.iter()))?;
                    let mut result = Vec::new();
                    let mut size = 0_usize;
                    while let Some(row) = rows.next()? {
                        let mut object = Map::new();
                        for (index, name) in names.iter().enumerate() {
                            let value = encode_value(row.get_ref(index)?)?;
                            size = size
                                .checked_add(name.len() + value_size(&value))
                                .ok_or(SqliteError::ResourceLimit)?;
                            if size > MAX_VALUE_BYTES {
                                return Err(SqliteError::ResourceLimit);
                            }
                            object.insert(name.clone(), value);
                        }
                        let value = Value::Object(object);
                        if request.method == SqliteMethod::Get {
                            return Ok(value);
                        }
                        if result.len() >= MAX_RESULT_ROWS {
                            return Err(SqliteError::ResourceLimit);
                        }
                        result.push(value);
                    }
                    if request.method == SqliteMethod::Get {
                        Ok(Value::Null)
                    } else {
                        Ok(Value::Array(result))
                    }
                }
                _ => Err(SqliteError::InvalidRequest(
                    "unreviewed SQLite statement method",
                )),
            }
        })?
    }
}

impl Drop for NativeSqlite {
    fn drop(&mut self) {
        // No public statement can escape this object, so ordinary close has
        // no outstanding statements. Even on cleanup failure, drop SQLite
        // before its ownership permit; never reverse this order.
        let _ = self.close();
        drop(self.connection.take());
        drop(self.permit.take());
    }
}

fn check_identity(
    handle: &str,
    generation: u64,
    request: &SqliteRequest,
) -> Result<(), SqliteError> {
    if request.owner_handle != handle {
        return Err(SqliteError::WrongOwner);
    }
    if request.generation != generation.to_string() {
        return Err(SqliteError::StaleGeneration);
    }
    Ok(())
}

fn storage_path(root: &Path) -> Result<PathBuf, SqliteError> {
    let path = root.join(DATABASE_FILE_NAME);
    for suffix in ["", "-wal", "-shm", "-journal"] {
        let candidate = root.join(format!("{DATABASE_FILE_NAME}{suffix}"));
        match fs::symlink_metadata(candidate) {
            Ok(metadata) => {
                if !metadata.is_file() || metadata.file_type().is_symlink() {
                    return Err(SqliteError::InvalidStorage);
                }
                #[cfg(unix)]
                {
                    use std::os::unix::fs::MetadataExt;
                    if metadata.nlink() != 1 {
                        return Err(SqliteError::InvalidStorage);
                    }
                }
                #[cfg(windows)]
                {
                    use std::os::windows::fs::MetadataExt;
                    if metadata.file_attributes() & 0x400 != 0 {
                        return Err(SqliteError::InvalidStorage);
                    }
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(path)
}

fn open_connection(
    root: &Path,
    options: SqliteOpenOptions,
    authorization: Arc<AuthorizationState>,
) -> Result<Connection, SqliteError> {
    let path = storage_path(root)?;
    if !path.exists() && !options.allow_initialize_empty_store {
        return Err(SqliteError::InitializationRequired);
    }
    let mut flags = OpenFlags::SQLITE_OPEN_READ_WRITE
        | OpenFlags::SQLITE_OPEN_NO_MUTEX
        | OpenFlags::SQLITE_OPEN_NOFOLLOW
        | OpenFlags::SQLITE_OPEN_PRIVATE_CACHE;
    if options.allow_initialize_empty_store {
        flags |= OpenFlags::SQLITE_OPEN_CREATE;
    }
    let mut connection = Connection::open_with_flags(path, flags)?;
    configure(&connection)?;
    // Existing metadata is untrusted too. Install both the authorizer and
    // execution bound before even the version probe can prepare a statement.
    install_authorization(&mut connection, Arc::clone(&authorization))?;
    let tables = schema_objects(&connection)?;
    if tables.is_empty() {
        if !options.allow_initialize_empty_store {
            return Err(SqliteError::InitializationRequired);
        }
    } else {
        let version = schema_version(&connection, &tables)?;
        if version != Some(i64::from(PI_SCHEMA_VERSION)) || !has_required_tables(&tables) {
            return Err(SqliteError::SchemaMismatch { found: version });
        }
    }
    // These are fixed native configuration, never caller-provided pragmas.
    // FULL makes a successful commit the durability boundary. Closing after
    // revocation does not schedule a checkpoint or implicitly commit a batch.
    configure_journal(&connection, &authorization)?;
    Ok(connection)
}

fn install_authorization(
    connection: &mut Connection,
    authorization: Arc<AuthorizationState>,
) -> Result<(), SqliteError> {
    let state = Arc::clone(&authorization);
    connection.authorizer(Some(move |context: AuthContext<'_>| {
        authorize(context, &state)
    }))?;
    connection.progress_handler(
        1000,
        Some(move || {
            authorization.progress.fetch_add(1, Ordering::Relaxed) >= MAX_PROGRESS_CALLBACKS
        }),
    )?;
    Ok(())
}

fn configure(connection: &Connection) -> Result<(), SqliteError> {
    connection.busy_timeout(Duration::ZERO)?;
    connection.set_db_config(DbConfig::SQLITE_DBCONFIG_DEFENSIVE, true)?;
    connection.set_db_config(DbConfig::SQLITE_DBCONFIG_TRUSTED_SCHEMA, false)?;
    connection.set_db_config(DbConfig::SQLITE_DBCONFIG_NO_CKPT_ON_CLOSE, true)?;
    Ok(())
}

fn configure_journal(
    connection: &Connection,
    state: &AuthorizationState,
) -> Result<(), SqliteError> {
    struct Reset<'a>(&'a AtomicBool);
    impl Drop for Reset<'_> {
        fn drop(&mut self) {
            self.0.store(false, Ordering::Relaxed);
        }
    }
    state.new_statement();
    state.configuration.store(true, Ordering::Relaxed);
    let _reset = Reset(&state.configuration);
    connection.execute_batch(
        "PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; PRAGMA temp_store=MEMORY;",
    )?;
    Ok(())
}

fn authorize(context: AuthContext<'_>, state: &AuthorizationState) -> Authorization {
    use Authorization::{Allow, Deny};
    // Pi's schema contains no triggers or views. Do not execute hidden schema
    // programs from an unexpected pre-existing database.
    if context.accessor.is_some() {
        return Deny;
    }
    match context.action {
        AuthAction::Transaction { .. } => {
            if state.control.load(Ordering::Relaxed) {
                Allow
            } else {
                Deny
            }
        }
        AuthAction::Pragma {
            pragma_name,
            pragma_value,
        } => {
            let native_setting = [
                ("journal_mode", "WAL"),
                ("synchronous", "FULL"),
                ("temp_store", "MEMORY"),
            ]
            .iter()
            .any(|(name, value)| {
                pragma_name.eq_ignore_ascii_case(name)
                    && pragma_value.is_some_and(|actual| actual.eq_ignore_ascii_case(value))
            });
            if state.configuration.load(Ordering::Relaxed) && native_setting {
                Allow
            } else {
                Deny
            }
        }
        AuthAction::Attach { .. } | AuthAction::Detach { .. } | AuthAction::Savepoint { .. } => {
            Deny
        }
        AuthAction::Function { function_name } => {
            let name = function_name.to_ascii_lowercase();
            if matches!(
                name.as_str(),
                "load_extension" | "readfile" | "writefile" | "edit" | "fts3_tokenizer"
            ) || name.starts_with("pragma_")
            {
                Deny
            } else {
                Allow
            }
        }
        AuthAction::Select | AuthAction::Recursive => Allow,
        AuthAction::AlterTable {
            database_name: "main",
            ..
        } => {
            state.alter_table.store(true, Ordering::Relaxed);
            Allow
        }
        // SQLite's own ALTER TABLE implementation reads and rewrites its empty
        // temporary schema while renaming tasks_v2 in Pi migration 2. The flag
        // is reset before each statement; callers cannot enable writable_schema.
        AuthAction::Read { table_name, .. } | AuthAction::Update { table_name, .. }
            if context.database_name == Some("temp")
                && matches!(table_name, "sqlite_temp_master" | "sqlite_temp_schema")
                && state.alter_table.load(Ordering::Relaxed) =>
        {
            Allow
        }
        AuthAction::CreateIndex { .. }
        | AuthAction::CreateTable { .. }
        | AuthAction::DropIndex { .. }
        | AuthAction::DropTable { .. }
        | AuthAction::Insert { .. }
        | AuthAction::Delete { .. }
        | AuthAction::Read { .. }
        | AuthAction::Update { .. }
        | AuthAction::Reindex { .. }
            if context.database_name.is_none() || context.database_name == Some("main") =>
        {
            Allow
        }
        _ => Deny,
    }
}

fn control(
    connection: &Connection,
    state: &AuthorizationState,
    sql: &'static str,
) -> Result<(), SqliteError> {
    struct Reset<'a>(&'a AtomicBool);
    impl Drop for Reset<'_> {
        fn drop(&mut self) {
            self.0.store(false, Ordering::Relaxed);
        }
    }
    state.new_statement();
    state.control.store(true, Ordering::Relaxed);
    let _reset = Reset(&state.control);
    connection.execute_batch(sql)?;
    Ok(())
}

fn decode_value(value: &Value) -> Result<SqlValue, SqliteError> {
    match value {
        Value::Null => Ok(SqlValue::Null),
        Value::String(text) if text.len() <= MAX_VALUE_BYTES => Ok(SqlValue::Text(text.clone())),
        Value::Number(number) => {
            if let Some(integer) = number.as_i64() {
                if !(-MAX_SAFE_INTEGER..=MAX_SAFE_INTEGER).contains(&integer) {
                    return Err(SqliteError::InvalidRequest(
                        "large SQLite integers require a tagged decimal value",
                    ));
                }
                return Ok(SqlValue::Integer(integer));
            }
            let number = number
                .as_f64()
                .ok_or(SqliteError::InvalidRequest("invalid SQLite number"))?;
            if !number.is_finite()
                || (number.fract() == 0.0 && number.abs() > MAX_SAFE_INTEGER as f64)
            {
                return Err(SqliteError::InvalidRequest(
                    "SQLite numeric binding loses precision",
                ));
            }
            Ok(SqlValue::Real(number))
        }
        Value::Object(tagged)
            if tagged.len() == 2
                && tagged.get("type").and_then(Value::as_str) == Some("integer") =>
        {
            let text = tagged
                .get("value")
                .and_then(Value::as_str)
                .ok_or(SqliteError::InvalidRequest("invalid SQLite integer tag"))?;
            let integer = text.parse::<i64>().map_err(|_| {
                SqliteError::InvalidRequest("SQLite integer is outside signed 64-bit range")
            })?;
            if integer.to_string() != text {
                return Err(SqliteError::InvalidRequest("noncanonical SQLite integer"));
            }
            Ok(SqlValue::Integer(integer))
        }
        Value::Object(tagged)
            if tagged.len() == 2 && tagged.get("type").and_then(Value::as_str) == Some("blob") =>
        {
            let text = tagged
                .get("base64")
                .and_then(Value::as_str)
                .ok_or(SqliteError::InvalidRequest("invalid SQLite blob tag"))?;
            if text.len() > MAX_VALUE_BYTES {
                return Err(SqliteError::ResourceLimit);
            }
            let bytes = BASE64
                .decode(text)
                .map_err(|_| SqliteError::InvalidRequest("invalid SQLite Base64 blob"))?;
            if BASE64.encode(&bytes) != text {
                return Err(SqliteError::InvalidRequest(
                    "noncanonical SQLite Base64 blob",
                ));
            }
            Ok(SqlValue::Blob(bytes))
        }
        _ => Err(SqliteError::InvalidRequest("unsupported SQLite wire value")),
    }
}

fn encode_value(value: ValueRef<'_>) -> Result<Value, SqliteError> {
    match value {
        ValueRef::Null => Ok(Value::Null),
        ValueRef::Integer(integer) if (-MAX_SAFE_INTEGER..=MAX_SAFE_INTEGER).contains(&integer) => {
            Ok(json!(integer))
        }
        ValueRef::Integer(integer) => {
            Ok(json!({ "type": "integer", "value": integer.to_string() }))
        }
        ValueRef::Real(number) => {
            if number.fract() == 0.0 && number.abs() > MAX_SAFE_INTEGER as f64 {
                return Err(SqliteError::InvalidRequest(
                    "SQLite real result is outside the safe wire integer range",
                ));
            }
            serde_json::Number::from_f64(number)
                .map(Value::Number)
                .ok_or(SqliteError::InvalidRequest(
                    "SQLite returned a non-finite number",
                ))
        }
        ValueRef::Text(text) if text.len() <= MAX_VALUE_BYTES => Ok(Value::String(
            std::str::from_utf8(text)
                .map_err(|_| SqliteError::InvalidRequest("SQLite returned non-UTF-8 text"))?
                .to_owned(),
        )),
        ValueRef::Blob(bytes) if bytes.len() <= MAX_VALUE_BYTES => {
            Ok(json!({ "type": "blob", "base64": BASE64.encode(bytes) }))
        }
        _ => Err(SqliteError::ResourceLimit),
    }
}

fn value_size(value: &Value) -> usize {
    match value {
        Value::String(text) => text.len(),
        Value::Object(object) => object.values().map(value_size).sum(),
        _ => 24,
    }
}

/// Whether the fixed session file was present during a read-only review.
#[derive(Clone, Copy, Debug, Serialize, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum StorageState {
    /// No database file exists. Inspection has not created one.
    Empty,
    /// A database file exists; compatibility is reported separately.
    Existing,
}

/// Content-free record counts used by native recovery review.
#[derive(Clone, Debug, Serialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct StorageCounts {
    /// Number of durable conversations.
    pub conversations: u64,
    /// Number of durable entries.
    pub entries: u64,
    /// Number of versioned documents.
    pub documents: u64,
    /// Task counts by persisted state; no task is resumed.
    pub tasks: BTreeMap<String, u64>,
    /// Submission counts by persisted state.
    pub submissions: BTreeMap<String, u64>,
}

/// Read-only recovery evidence, never a permission to resume work.
#[derive(Clone, Debug, Serialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct StorageInspection {
    /// Whether the fixed database existed.
    pub state: StorageState,
    /// Persisted Pi schema version, absent when metadata is unrecognized.
    pub schema_version: Option<i64>,
    /// Exact pinned version and required tables were found.
    pub compatible: bool,
    /// Available only for a compatible store; absence is not a count of zero.
    pub counts: Option<StorageCounts>,
}

/// Inspect the fixed store in `RecoveryRequired` without initialization,
/// migration, writable storage, or scheduler entry. The read-only connection
/// opens and closes synchronously inside `with_owned`; it never escapes that
/// ownership gate and therefore needs no retained asynchronous operation permit.
pub fn inspect_storage(
    owner: &SessionOwner,
    token: &OwnerToken,
) -> Result<StorageInspection, SqliteError> {
    owner.with_owned(token, || inspect_owned_storage(owner.storage_root()))?
}

fn inspect_owned_storage(root: &Path) -> Result<StorageInspection, SqliteError> {
    let path = storage_path(root)?;
    if !path.exists() {
        return Ok(StorageInspection {
            state: StorageState::Empty,
            schema_version: None,
            compatible: false,
            counts: None,
        });
    }
    let mut connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_NO_MUTEX
            | OpenFlags::SQLITE_OPEN_NOFOLLOW
            | OpenFlags::SQLITE_OPEN_PRIVATE_CACHE,
    )?;
    configure(&connection)?;
    install_authorization(&mut connection, Arc::new(AuthorizationState::default()))?;
    let tables = schema_objects(&connection)?;
    let schema_version = schema_version(&connection, &tables)?;
    let compatible =
        schema_version == Some(i64::from(PI_SCHEMA_VERSION)) && has_required_tables(&tables);
    let counts = if compatible {
        Some(StorageCounts {
            conversations: count(&connection, "SELECT count(*) FROM conversations")?,
            entries: count(&connection, "SELECT count(*) FROM entries")?,
            documents: count(&connection, "SELECT count(*) FROM documents")?,
            tasks: states(
                &connection,
                "SELECT status, count(*) FROM tasks GROUP BY status",
            )?,
            submissions: states(
                &connection,
                "SELECT status, count(*) FROM submissions GROUP BY status",
            )?,
        })
    } else {
        None
    };
    connection
        .close()
        .map_err(|(_, error)| SqliteError::Database(error))?;
    Ok(StorageInspection {
        state: StorageState::Existing,
        schema_version,
        compatible,
        counts,
    })
}

fn schema_objects(connection: &Connection) -> Result<BTreeMap<String, String>, SqliteError> {
    let mut statement =
        connection.prepare("SELECT name,type FROM sqlite_schema WHERE name NOT LIKE 'sqlite_%'")?;
    let rows = statement.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

fn has_required_tables(tables: &BTreeMap<String, String>) -> bool {
    REQUIRED_PI_TABLES
        .iter()
        .all(|name| tables.get(*name).map(String::as_str) == Some("table"))
}

fn schema_version(
    connection: &Connection,
    tables: &BTreeMap<String, String>,
) -> Result<Option<i64>, SqliteError> {
    // Metadata and every required persistent object must be ordinary tables.
    // The profile cannot evaluate a same-named view while probing a version.
    if tables.get("durable_schema").map(String::as_str) != Some("table") {
        return Ok(None);
    }
    Ok(connection
        .query_row(
            "SELECT version FROM durable_schema WHERE singleton=1",
            [],
            |row| row.get(0),
        )
        .optional()?)
}

fn count(connection: &Connection, sql: &str) -> Result<u64, SqliteError> {
    let value: i64 = connection.query_row(sql, [], |row| row.get(0))?;
    if !(0..=MAX_SAFE_INTEGER).contains(&value) {
        return Err(SqliteError::ResourceLimit);
    }
    Ok(value as u64)
}

fn states(connection: &Connection, sql: &str) -> Result<BTreeMap<String, u64>, SqliteError> {
    let mut statement = connection.prepare(sql)?;
    let mut rows = statement.query([])?;
    let mut result = BTreeMap::new();
    while let Some(row) = rows.next()? {
        let state: String = row.get(0)?;
        let value: i64 = row.get(1)?;
        if !(0..=MAX_SAFE_INTEGER).contains(&value) || result.len() >= 100 {
            return Err(SqliteError::ResourceLimit);
        }
        result.insert(state, value as u64);
    }
    Ok(result)
}
