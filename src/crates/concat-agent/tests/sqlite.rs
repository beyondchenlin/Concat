// SPDX-License-Identifier: AGPL-3.0-or-later
//! Native SQLite contract and failure tests using isolated real disk files.
//! These tests are not Android lifecycle tests or product acceptance.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use concat_agent::sqlite::{
    DATABASE_FILE_NAME, NativeSqlite, PI_SCHEMA_VERSION, SqliteError, SqliteMethod,
    SqliteOpenOptions, SqliteRequest, StorageState, inspect_storage,
};
use concat_agent::{Reconciliation, RecoveryDecision, SessionOwner};
use rusqlite::Connection;
use serde_json::{Value, json};

const HANDLE: &str = "native-test-owner";
static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(1);

struct Directory(PathBuf);
impl Directory {
    fn new() -> Self {
        let sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let time = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "bk-cat-sqlite-{}-{time}-{sequence}",
            std::process::id()
        ));
        fs::create_dir(&path).expect("isolated directory");
        Self(path)
    }
    fn path(&self) -> &Path {
        &self.0
    }
    fn database(&self) -> PathBuf {
        self.0.join(DATABASE_FILE_NAME)
    }
}
impl Drop for Directory {
    fn drop(&mut self) {
        if let Err(error) = fs::remove_dir_all(&self.0) {
            eprintln!("could not remove this test's database directory: {error}");
        }
    }
}

fn approve(owner: &SessionOwner) {
    owner
        .confirm_recovery(
            &owner.token(),
            Reconciliation {
                project_identity: "isolated-project".into(),
                project_revision: "1".into(),
                session_revision: "1".into(),
            },
            RecoveryDecision::DiscardInterrupted,
        )
        .expect("native user decision");
}

fn options(initialize: bool) -> SqliteOpenOptions {
    SqliteOpenOptions {
        expected_schema_version: PI_SCHEMA_VERSION,
        allow_initialize_empty_store: initialize,
    }
}

fn open(directory: &Directory) -> (SessionOwner, NativeSqlite) {
    let owner = SessionOwner::acquire(directory.path()).expect("owner");
    approve(&owner);
    let database = NativeSqlite::open(&owner, &owner.token(), HANDLE, options(true))
        .expect("real SQLite file");
    (owner, database)
}

fn frame(database: &NativeSqlite, generation: u64, method: SqliteMethod) -> SqliteRequest {
    SqliteRequest {
        method,
        owner_handle: HANDLE.into(),
        generation: generation.to_string(),
        database_id: Some(database.database_id().into()),
        transaction_id: None,
        expected_schema_version: None,
        allow_initialize_empty_store: None,
        sql: None,
        params: None,
    }
}

fn statement(
    database: &mut NativeSqlite,
    generation: u64,
    method: SqliteMethod,
    sql: &str,
    transaction: Option<&str>,
    params: Vec<Value>,
) -> Result<Value, SqliteError> {
    let mut request = frame(database, generation, method);
    request.sql = Some(sql.into());
    request.transaction_id = transaction.map(str::to_owned);
    request.params = Some(params);
    database.handle(&request)
}

fn control(
    database: &mut NativeSqlite,
    generation: u64,
    method: SqliteMethod,
    transaction: Option<&str>,
) -> Result<Value, SqliteError> {
    let mut request = frame(database, generation, method);
    request.transaction_id = transaction.map(str::to_owned);
    database.handle(&request)
}

fn begin(database: &mut NativeSqlite, generation: u64) -> String {
    control(database, generation, SqliteMethod::Begin, None).expect("begins transaction")
        ["transactionId"].as_str().expect("native transaction identity").to_owned()
}

fn initialize_pi(database: &mut NativeSqlite, generation: u64) {
    let transaction = begin(database, generation);
    statement(
        database,
        generation,
        SqliteMethod::Exec,
        include_str!("fixtures/pi-schema-1.5.2.sql"),
        Some(&transaction),
        vec![],
    )
    .expect("original pinned Pi schema statements");
    control(
        database,
        generation,
        SqliteMethod::Commit,
        Some(&transaction),
    )
    .expect("durable schema commit");
}

#[test]
fn recovery_inspection_never_initializes_an_absent_database() {
    let directory = Directory::new();
    let owner = SessionOwner::acquire(directory.path()).expect("owner");
    let inspected = inspect_storage(&owner, &owner.token()).expect("read-only recovery inspection");
    assert_eq!(inspected.state, StorageState::Empty);
    assert!(inspected.schema_version.is_none());
    assert!(inspected.counts.is_none());
    assert!(!directory.database().exists());
    assert!(matches!(
        NativeSqlite::open(&owner, &owner.token(), HANDLE, options(true)),
        Err(SqliteError::Ownership(
            concat_agent::Error::RecoveryRequired
        ))
    ));
    assert!(!directory.database().exists());
    approve(&owner);
    assert!(matches!(
        NativeSqlite::open(&owner, &owner.token(), HANDLE, options(false)),
        Err(SqliteError::InitializationRequired)
    ));
    assert!(!directory.database().exists());
    owner.revoke();
    assert!(matches!(
        inspect_storage(&owner, &owner.token()),
        Err(SqliteError::Ownership(concat_agent::Error::Revoked))
    ));
}

#[test]
fn opening_frames_cannot_grant_themselves_initialization_or_a_path() {
    let directory = Directory::new();
    let owner = SessionOwner::acquire(directory.path()).expect("owner");
    approve(&owner);
    let mut request: SqliteRequest = serde_json::from_value(json!({
        "method": "sqlite.open", "ownerHandle": HANDLE, "generation": owner.token().generation().to_string(),
        "expectedSchemaVersion": 2, "allowInitializeEmptyStore": true,
    })).expect("opening frame");
    assert!(matches!(
        NativeSqlite::open_request(&owner, &owner.token(), HANDLE, false, &request),
        Err(SqliteError::InitializationRequired)
    ));
    assert!(!directory.database().exists());
    request.generation = "0001".into();
    assert!(matches!(
        NativeSqlite::open_request(&owner, &owner.token(), HANDLE, true, &request),
        Err(SqliteError::StaleGeneration)
    ));
    let mut with_path = serde_json::to_value(&request).expect("serializes");
    with_path["path"] = json!(directory.path().join("other.sqlite3"));
    assert!(serde_json::from_value::<SqliteRequest>(with_path).is_err());
    request.generation = owner.token().generation().to_string();
    let mut database = NativeSqlite::open_request(&owner, &owner.token(), HANDLE, true, &request)
        .expect("native grant permits empty initialization");
    database.close().expect("closes");
}

#[test]
fn pinned_pi_schema_initializes_and_real_data_survives_handoff() {
    let directory = Directory::new();
    let (owner, mut database) = open(&directory);
    let generation = owner.token().generation();
    initialize_pi(&mut database, generation);
    statement(
        &mut database,
        generation,
        SqliteMethod::Run,
        "INSERT INTO conversations (id, record) VALUES (?, ?)",
        None,
        vec![json!(7), json!("{}")],
    )
    .expect("writes bound Pi record");
    statement(&mut database, generation, SqliteMethod::Run,
        "INSERT INTO tasks (id,conversation_id,kind,status,abort_requested,background,record) VALUES (8,7,'test','sleeping',0,0,'{}')",
        None, vec![]).expect("Pi version 2 sleeping status");
    database
        .close()
        .expect("SQLite closes before ownership release");
    assert!(
        directory
            .database()
            .metadata()
            .expect("real database")
            .len()
            > 0
    );
    owner
        .finish_handoff(Duration::ZERO)
        .expect("no storage permit remains");
    let next = SessionOwner::acquire(directory.path()).expect("next generation");
    let snapshot = inspect_storage(&next, &next.token())
        .expect("read-only inspection before recovery decision");
    assert_eq!(snapshot.state, StorageState::Existing);
    assert_eq!(snapshot.schema_version, Some(2));
    assert!(snapshot.compatible);
    let counts = snapshot.counts.expect("compatible counts");
    assert_eq!(counts.conversations, 1);
    assert_eq!(counts.tasks.get("sleeping"), Some(&1));
    assert_eq!(next.status().expect("status").active_operations, 0);
    assert_eq!(
        next.status().expect("status").phase,
        concat_agent::Phase::RecoveryRequired
    );
    approve(&next);
    let mut reopened = NativeSqlite::open(&next, &next.token(), HANDLE, options(false))
        .expect("no migration needed");
    let row = statement(
        &mut reopened,
        next.token().generation(),
        SqliteMethod::Get,
        "SELECT record FROM conversations WHERE id=?",
        None,
        vec![json!(7)],
    )
    .expect("reads persisted record");
    assert_eq!(row, json!({ "record": "{}" }));
}

#[test]
fn existing_unsupported_schema_is_read_only_inspected_and_not_migrated() {
    let directory = Directory::new();
    let connection = Connection::open(directory.database()).expect("isolated old file");
    connection.execute_batch("CREATE TABLE durable_schema(singleton INTEGER PRIMARY KEY, version INTEGER); INSERT INTO durable_schema VALUES(1,1); CREATE TABLE sentinel(value TEXT); INSERT INTO sentinel VALUES('keep');").expect("old schema fixture");
    connection.close().expect("closes fixture");
    let before = fs::read(directory.database()).expect("snapshot");
    let owner = SessionOwner::acquire(directory.path()).expect("owner");
    let snapshot = inspect_storage(&owner, &owner.token()).expect("read-only version check");
    assert_eq!(snapshot.schema_version, Some(1));
    assert!(!snapshot.compatible);
    assert!(snapshot.counts.is_none());
    assert_eq!(
        fs::read(directory.database()).expect("unchanged database"),
        before
    );
    approve(&owner);
    assert!(matches!(
        NativeSqlite::open(&owner, &owner.token(), HANDLE, options(true)),
        Err(SqliteError::SchemaMismatch { found: Some(1) })
    ));
    assert_eq!(
        fs::read(directory.database()).expect("still unchanged database"),
        before
    );
}

#[test]
fn existing_same_named_metadata_views_are_never_executed_or_replaced() {
    for view in [
        "CREATE VIEW durable_schema AS SELECT 1 AS singleton, 2 AS version",
        "CREATE VIEW durable_schema AS WITH RECURSIVE forever(n) AS (VALUES(1) UNION ALL SELECT n+1 FROM forever) SELECT 1 AS singleton, sum(n) AS version FROM forever",
    ] {
        let directory = Directory::new();
        let connection = Connection::open(directory.database()).expect("isolated existing file");
        connection
            .execute_batch(view)
            .expect("malformed metadata fixture");
        connection.close().expect("closes fixture");
        let before = fs::read(directory.database()).expect("original database bytes");
        let owner = SessionOwner::acquire(directory.path()).expect("owner");
        let snapshot = inspect_storage(&owner, &owner.token()).expect("never evaluates the view");
        assert_eq!(snapshot.state, StorageState::Existing);
        assert!(snapshot.schema_version.is_none());
        assert!(!snapshot.compatible);
        assert!(snapshot.counts.is_none());
        approve(&owner);
        assert!(matches!(
            NativeSqlite::open(&owner, &owner.token(), HANDLE, options(true)),
            Err(SqliteError::SchemaMismatch { found: None })
        ));
        assert_eq!(
            fs::read(directory.database()).expect("unchanged existing database"),
            before
        );
    }
}

#[test]
fn a_version_two_marker_without_the_other_real_tables_is_never_writable() {
    let directory = Directory::new();
    let connection = Connection::open(directory.database()).expect("isolated existing file");
    connection.execute_batch("CREATE TABLE durable_schema(singleton INTEGER PRIMARY KEY, version INTEGER); INSERT INTO durable_schema VALUES(1,2);").expect("misleading version marker");
    connection.close().expect("closes fixture");
    let before = fs::read(directory.database()).expect("original bytes");
    let owner = SessionOwner::acquire(directory.path()).expect("owner");
    let snapshot = inspect_storage(&owner, &owner.token()).expect("read-only evidence");
    assert_eq!(snapshot.schema_version, Some(2));
    assert!(
        !snapshot.compatible,
        "a version marker cannot replace real tables"
    );
    approve(&owner);
    assert!(matches!(
        NativeSqlite::open(&owner, &owner.token(), HANDLE, options(true)),
        Err(SqliteError::SchemaMismatch { found: Some(2) })
    ));
    assert_eq!(fs::read(directory.database()).expect("same bytes"), before);
}

#[test]
fn a_version_two_store_with_a_fake_tasks_view_is_never_writable() {
    let directory = Directory::new();
    let connection = Connection::open(directory.database()).expect("isolated original schema");
    connection
        .execute_batch(include_str!("fixtures/pi-schema-1.5.2.sql"))
        .expect("fixed original schema");
    connection
        .execute_batch(
            "DROP TABLE tasks; CREATE VIEW tasks AS SELECT 9 AS id, 'running' AS status;",
        )
        .expect("replaces a required table with a view");
    connection.close().expect("closes fixture");
    let before = fs::read(directory.database()).expect("original bytes");
    let owner = SessionOwner::acquire(directory.path()).expect("owner");
    let snapshot = inspect_storage(&owner, &owner.token()).expect("read-only evidence");
    assert_eq!(snapshot.schema_version, Some(2));
    assert!(!snapshot.compatible);
    assert!(snapshot.counts.is_none());
    approve(&owner);
    assert!(matches!(
        NativeSqlite::open(&owner, &owner.token(), HANDLE, options(true)),
        Err(SqliteError::SchemaMismatch { found: Some(2) })
    ));
    assert_eq!(fs::read(directory.database()).expect("same bytes"), before);
}

#[test]
fn unrelated_or_expired_transaction_handles_never_join_active_work() {
    let directory = Directory::new();
    let (owner, mut database) = open(&directory);
    let generation = owner.token().generation();
    statement(
        &mut database,
        generation,
        SqliteMethod::Exec,
        "CREATE TABLE value(n INTEGER)",
        None,
        vec![],
    )
    .expect("table");
    let transaction = begin(&mut database, generation);
    statement(
        &mut database,
        generation,
        SqliteMethod::Run,
        "INSERT INTO value VALUES(1)",
        Some(&transaction),
        vec![],
    )
    .expect("transaction write");
    for handle in [None, Some("wrong-transaction")] {
        assert!(matches!(
            statement(
                &mut database,
                generation,
                SqliteMethod::Run,
                "INSERT INTO value VALUES(2)",
                handle,
                vec![]
            ),
            Err(SqliteError::WrongTransaction)
        ));
    }
    assert!(matches!(
        control(&mut database, generation, SqliteMethod::Begin, None),
        Err(SqliteError::WrongTransaction)
    ));
    assert!(matches!(
        control(
            &mut database,
            generation,
            SqliteMethod::Commit,
            Some("wrong")
        ),
        Err(SqliteError::WrongTransaction)
    ));
    control(
        &mut database,
        generation,
        SqliteMethod::Commit,
        Some(&transaction),
    )
    .expect("matching commit");
    assert!(matches!(
        statement(
            &mut database,
            generation,
            SqliteMethod::Get,
            "SELECT n FROM value",
            Some(&transaction),
            vec![]
        ),
        Err(SqliteError::WrongTransaction)
    ));
    let row = statement(
        &mut database,
        generation,
        SqliteMethod::Get,
        "SELECT sum(n) AS n FROM value",
        None,
        vec![],
    )
    .expect("outside read");
    assert_eq!(row, json!({ "n": 1 }));
}

#[test]
fn revocation_refuses_queries_and_commit_but_allows_matching_rollback_and_close() {
    let directory = Directory::new();
    let (owner, mut database) = open(&directory);
    let generation = owner.token().generation();
    initialize_pi(&mut database, generation);
    let transaction = begin(&mut database, generation);
    statement(
        &mut database,
        generation,
        SqliteMethod::Run,
        "INSERT INTO conversations(id,record) VALUES(3,'{}')",
        Some(&transaction),
        vec![],
    )
    .expect("uncommitted write");
    owner.revoke();
    assert!(matches!(
        control(
            &mut database,
            generation,
            SqliteMethod::Commit,
            Some(&transaction)
        ),
        Err(SqliteError::Ownership(concat_agent::Error::Revoked))
    ));
    assert!(matches!(
        statement(
            &mut database,
            generation,
            SqliteMethod::Get,
            "SELECT id FROM conversations",
            Some(&transaction),
            vec![]
        ),
        Err(SqliteError::Ownership(concat_agent::Error::Revoked))
    ));
    assert!(matches!(
        owner.finish_handoff(Duration::ZERO),
        Err(concat_agent::Error::DrainTimeout {
            active_operations: 1
        })
    ));
    assert!(matches!(
        SessionOwner::acquire(directory.path()),
        Err(concat_agent::Error::Busy)
    ));
    assert!(matches!(
        control(
            &mut database,
            generation,
            SqliteMethod::Rollback,
            Some("wrong")
        ),
        Err(SqliteError::WrongTransaction)
    ));
    control(
        &mut database,
        generation,
        SqliteMethod::Rollback,
        Some(&transaction),
    )
    .expect("cleanup after revocation");
    database.close().expect("closes after revocation");
    owner.finish_handoff(Duration::ZERO).expect("drained");
    let next = SessionOwner::acquire(directory.path()).expect("new generation");
    let snapshot = inspect_storage(&next, &next.token()).expect("read-only actual database");
    assert_eq!(
        snapshot.counts.expect("counts").conversations,
        0,
        "uncommitted data was rolled back"
    );
}

#[test]
fn dropping_a_revoked_connection_rolls_back_before_releasing_ownership() {
    let directory = Directory::new();
    let (owner, mut database) = open(&directory);
    let generation = owner.token().generation();
    initialize_pi(&mut database, generation);
    let transaction = begin(&mut database, generation);
    statement(
        &mut database,
        generation,
        SqliteMethod::Run,
        "INSERT INTO conversations(id,record) VALUES(5,'{}')",
        Some(&transaction),
        vec![],
    )
    .expect("pending write");
    drop(owner);
    assert!(matches!(
        SessionOwner::acquire(directory.path()),
        Err(concat_agent::Error::Busy)
    ));
    drop(database);
    let next = SessionOwner::acquire(directory.path()).expect("connection drained first");
    assert_eq!(
        inspect_storage(&next, &next.token())
            .expect("actual file")
            .counts
            .expect("counts")
            .conversations,
        0
    );
}

#[test]
fn authorizer_rejects_external_files_pragmas_and_sql_transaction_bypasses() {
    let directory = Directory::new();
    let (owner, mut database) = open(&directory);
    let generation = owner.token().generation();
    let escaped = directory
        .path()
        .join("not-allowed.sqlite3")
        .to_string_lossy()
        .replace('\'', "''");
    for sql in [
        format!("ATTACH DATABASE '{escaped}' AS outside"),
        format!("VACUUM INTO '{escaped}'"),
        format!("PRAGMA temp_store_directory='{escaped}'"),
        format!("PRAGMA data_store_directory='{escaped}'"),
        "PRAGMA writable_schema=ON".into(),
        "PRAGMA journal_mode=OFF".into(),
        "BEGIN".into(),
        "COMMIT".into(),
        "ROLLBACK".into(),
        "END".into(),
        "SAVEPOINT stray".into(),
        "SELECT load_extension('not-allowed')".into(),
        "SELECT writefile('not-allowed','data')".into(),
        "CREATE VIRTUAL TABLE outside USING fts5(value)".into(),
    ] {
        assert!(
            statement(
                &mut database,
                generation,
                SqliteMethod::Exec,
                &sql,
                None,
                vec![]
            )
            .is_err(),
            "accepted forbidden SQL: {sql}"
        );
    }
    assert!(!directory.path().join("not-allowed.sqlite3").exists());
    let row = statement(
        &mut database,
        generation,
        SqliteMethod::Get,
        "SELECT 1 AS alive",
        None,
        vec![],
    )
    .expect("connection still intact");
    assert_eq!(row, json!({ "alive": 1 }));
}

#[test]
fn single_statement_methods_refuse_a_second_statement_before_the_first_write() {
    let directory = Directory::new();
    let (owner, mut database) = open(&directory);
    let generation = owner.token().generation();
    statement(
        &mut database,
        generation,
        SqliteMethod::Exec,
        "CREATE TABLE value(n TEXT); INSERT INTO value VALUES('quoted; semicolon')",
        None,
        vec![],
    )
    .expect("real batch parser");
    assert!(
        statement(
            &mut database,
            generation,
            SqliteMethod::Run,
            "INSERT INTO value VALUES('first'); INSERT INTO value VALUES('second')",
            None,
            vec![]
        )
        .is_err()
    );
    assert_eq!(
        statement(
            &mut database,
            generation,
            SqliteMethod::Get,
            "SELECT count(*) AS n FROM value",
            None,
            vec![]
        )
        .expect("count"),
        json!({ "n": 1 })
    );
}

#[test]
fn all_portable_value_types_round_trip_without_integer_precision_loss() {
    let directory = Directory::new();
    let (owner, mut database) = open(&directory);
    let generation = owner.token().generation();
    statement(
        &mut database,
        generation,
        SqliteMethod::Exec,
        "CREATE TABLE value(a,b,c,d,e,f)",
        None,
        vec![],
    )
    .expect("table");
    let parameters = vec![
        Value::Null,
        json!(1.25),
        json!(9_007_199_254_740_991_i64),
        json!({"type":"integer","value":"9223372036854775807"}),
        json!("字幕;\u{0000}text"),
        json!({"type":"blob","base64":"AP8BAg=="}),
    ];
    assert_eq!(
        statement(
            &mut database,
            generation,
            SqliteMethod::RunCounted,
            "INSERT INTO value VALUES(?,?,?,?,?,?)",
            None,
            parameters
        )
        .expect("bound values"),
        json!(1)
    );
    let expected = json!({"a":null,"b":1.25,"c":9_007_199_254_740_991_i64,
        "d":{"type":"integer","value":"9223372036854775807"},"e":"字幕;\u{0000}text","f":{"type":"blob","base64":"AP8BAg=="}});
    assert_eq!(
        statement(
            &mut database,
            generation,
            SqliteMethod::Get,
            "SELECT * FROM value",
            None,
            vec![]
        )
        .expect("single row"),
        expected
    );
    assert_eq!(
        statement(
            &mut database,
            generation,
            SqliteMethod::All,
            "SELECT * FROM value",
            None,
            vec![]
        )
        .expect("rows"),
        json!([expected])
    );
    assert_eq!(
        statement(
            &mut database,
            generation,
            SqliteMethod::Get,
            "SELECT * FROM value WHERE 0",
            None,
            vec![]
        )
        .expect("absent row"),
        Value::Null
    );
    for parameter in [
        json!(true),
        json!(9_007_199_254_740_992_i64),
        json!({"type":"integer","value":"9223372036854775808"}),
        json!({"type":"blob","base64":"AP8BAg"}),
    ] {
        assert!(
            statement(
                &mut database,
                generation,
                SqliteMethod::Get,
                "SELECT ? AS rejected",
                None,
                vec![parameter]
            )
            .is_err()
        );
    }
}

#[test]
fn identities_are_checked_on_reads_writes_and_cleanup() {
    let directory = Directory::new();
    let (owner, mut database) = open(&directory);
    let generation = owner.token().generation();
    let mut request = frame(&database, generation, SqliteMethod::Close);
    request.owner_handle = "foreign".into();
    assert!(matches!(
        database.handle(&request),
        Err(SqliteError::WrongOwner)
    ));
    request.owner_handle = HANDLE.into();
    request.generation = (generation + 1).to_string();
    assert!(matches!(
        database.handle(&request),
        Err(SqliteError::StaleGeneration)
    ));
    request.generation = generation.to_string();
    request.database_id = Some("foreign".into());
    assert!(matches!(
        database.handle(&request),
        Err(SqliteError::WrongDatabase)
    ));
    assert_eq!(
        owner.status().expect("status").active_operations,
        1,
        "foreign cleanup did not close current storage"
    );
    database.close().expect("native cleanup");
    assert_eq!(owner.status().expect("status").active_operations, 0);
}

#[test]
fn sqlite_automatic_rollback_cannot_turn_an_old_transaction_into_implicit_writes() {
    let directory = Directory::new();
    let (owner, mut database) = open(&directory);
    let generation = owner.token().generation();
    statement(
        &mut database,
        generation,
        SqliteMethod::Exec,
        "CREATE TABLE value(n INTEGER PRIMARY KEY); INSERT INTO value VALUES(1)",
        None,
        vec![],
    )
    .expect("fixture");
    let transaction = begin(&mut database, generation);
    assert!(
        statement(
            &mut database,
            generation,
            SqliteMethod::Run,
            "INSERT OR ROLLBACK INTO value VALUES(1)",
            Some(&transaction),
            vec![]
        )
        .is_err()
    );
    assert!(matches!(
        statement(
            &mut database,
            generation,
            SqliteMethod::Run,
            "INSERT INTO value VALUES(2)",
            Some(&transaction),
            vec![]
        ),
        Err(SqliteError::WrongTransaction)
    ));
    control(
        &mut database,
        generation,
        SqliteMethod::Rollback,
        Some(&transaction),
    )
    .expect("already rolled back cleanup");
    assert_eq!(
        statement(
            &mut database,
            generation,
            SqliteMethod::Get,
            "SELECT count(*) AS n FROM value",
            None,
            vec![]
        )
        .expect("actual file state"),
        json!({ "n": 1 })
    );
}

#[test]
fn a_nonterminating_query_is_bounded_without_losing_the_connection() {
    let directory = Directory::new();
    let (owner, mut database) = open(&directory);
    let generation = owner.token().generation();
    assert!(
        statement(
            &mut database,
            generation,
            SqliteMethod::Get,
            "WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n) SELECT sum(x) FROM n",
            None,
            vec![]
        )
        .is_err()
    );
    assert_eq!(
        statement(
            &mut database,
            generation,
            SqliteMethod::Get,
            "SELECT 1 AS n",
            None,
            vec![]
        )
        .expect("next valid statement"),
        json!({ "n": 1 })
    );
}

#[cfg(unix)]
#[test]
fn database_and_sidecar_links_are_refused_without_touching_the_target() {
    use std::os::unix::fs::symlink;
    let target = Directory::new();
    let sentinel = target.path().join("untouched");
    fs::write(&sentinel, b"do not change").expect("isolated sentinel");
    for suffix in ["", "-wal", "-shm", "-journal"] {
        let directory = Directory::new();
        symlink(
            &sentinel,
            directory
                .path()
                .join(format!("{DATABASE_FILE_NAME}{suffix}")),
        )
        .expect("test alias");
        let owner = SessionOwner::acquire(directory.path()).expect("owner");
        approve(&owner);
        assert!(matches!(
            NativeSqlite::open(&owner, &owner.token(), HANDLE, options(true)),
            Err(SqliteError::InvalidStorage)
        ));
        assert_eq!(fs::read(&sentinel).expect("sentinel"), b"do not change");
    }
}
