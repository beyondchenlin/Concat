// SPDX-License-Identifier: AGPL-3.0-or-later
//! Deterministic native ownership tests. The mobile-overlap test is a host
//! lifecycle simulation using a real OS lock; it is not an Android device test.

use concat_agent::{Error, OWNER_FILE_NAME, Phase, Reconciliation, RecoveryDecision, SessionOwner};
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Barrier, mpsc};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(1);

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new() -> Self {
        let sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let instant = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "bk-cat-owner-{}-{instant}-{sequence}",
            std::process::id()
        ));
        fs::create_dir(&root).expect("isolated test directory");
        Self(root)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        if let Err(error) = fs::remove_dir_all(&self.0) {
            eprintln!("could not remove this test's temporary directory: {error}");
        }
    }
}

fn review() -> Reconciliation {
    Reconciliation {
        project_identity: "isolated-project".into(),
        project_revision: "revision-4".into(),
        session_revision: "session-7".into(),
    }
}

fn approve(owner: &SessionOwner) {
    owner
        .confirm_recovery(
            &owner.token(),
            review(),
            RecoveryDecision::DiscardInterrupted,
        )
        .expect("explicit test user decision after reconciliation");
}

#[test]
fn a_new_owner_cannot_start_work_until_reviewed() {
    let directory = TestDirectory::new();
    let owner = SessionOwner::acquire(directory.path()).expect("owner");
    let token = owner.token();
    assert_eq!(
        owner.status().expect("status").phase,
        Phase::RecoveryRequired
    );
    assert!(matches!(owner.begin(&token), Err(Error::RecoveryRequired)));
    assert!(owner.reconciliation().expect("review").is_none());
    approve(&owner);
    let operation = owner.begin(&token).expect("admitted after user decision");
    assert_eq!(operation.with_valid(|| 42).expect("valid operation"), 42);
    assert_eq!(
        owner.reconciliation().expect("review"),
        Some((review(), RecoveryDecision::DiscardInterrupted))
    );
}

#[test]
fn recovery_reads_are_allowed_but_revocation_refuses_even_read_callbacks() {
    let directory = TestDirectory::new();
    let owner = SessionOwner::acquire(directory.path()).expect("owner");
    let token = owner.token();
    assert_eq!(
        owner
            .with_owned(&token, || 7)
            .expect("read-only recovery view"),
        7
    );
    assert!(matches!(owner.begin(&token), Err(Error::RecoveryRequired)));
    owner.revoke();
    assert!(matches!(
        owner.with_owned(&token, || panic!("revoked read")),
        Err(Error::Revoked)
    ));
}

#[test]
fn incomplete_or_repeated_review_does_not_approve_again() {
    let directory = TestDirectory::new();
    let owner = SessionOwner::acquire(directory.path()).expect("owner");
    let mut incomplete = review();
    incomplete.session_revision.clear();
    assert!(matches!(
        owner.confirm_recovery(
            &owner.token(),
            incomplete,
            RecoveryDecision::DiscardInterrupted
        ),
        Err(Error::IncompleteReconciliation)
    ));
    assert!(matches!(
        owner.begin(&owner.token()),
        Err(Error::RecoveryRequired)
    ));
    approve(&owner);
    assert!(matches!(
        owner.confirm_recovery(
            &owner.token(),
            review(),
            RecoveryDecision::ContinueAfterRevalidation
        ),
        Err(Error::RecoveryAlreadyCompleted)
    ));
}

#[test]
fn same_store_and_canonical_path_alias_share_one_real_lock() {
    let directory = TestDirectory::new();
    fs::create_dir(directory.path().join("child")).expect("alias child directory");
    let owner = SessionOwner::acquire(directory.path()).expect("first owner");
    assert!(matches!(
        SessionOwner::acquire(directory.path()),
        Err(Error::Busy)
    ));
    assert!(matches!(
        SessionOwner::acquire(directory.path().join("child").join("..")),
        Err(Error::Busy)
    ));
    assert_eq!(
        owner.storage_root(),
        fs::canonicalize(directory.path()).expect("canonical root")
    );
}

#[cfg(unix)]
#[test]
fn a_symlink_to_the_storage_root_cannot_create_a_second_owner() {
    use std::os::unix::fs::symlink;
    let directory = TestDirectory::new();
    let alias = TestDirectory::new();
    symlink(directory.path(), alias.path().join("store")).expect("storage alias");
    let _owner = SessionOwner::acquire(directory.path()).expect("first owner");
    assert!(matches!(
        SessionOwner::acquire(alias.path().join("store")),
        Err(Error::Busy)
    ));
}

#[test]
fn different_stores_cannot_exchange_tokens_even_when_generations_match() {
    let first_directory = TestDirectory::new();
    let second_directory = TestDirectory::new();
    let first = SessionOwner::acquire(first_directory.path()).expect("first store");
    let second = SessionOwner::acquire(second_directory.path()).expect("second store");
    assert_eq!(first.token().generation(), second.token().generation());
    approve(&first);
    approve(&second);
    assert!(matches!(
        second.begin(&first.token()),
        Err(Error::TokenMismatch)
    ));
}

#[test]
fn persistent_generation_increases_without_removing_the_lock_file() {
    let directory = TestDirectory::new();
    let lock = directory.path().join(OWNER_FILE_NAME);
    let first = SessionOwner::acquire(directory.path()).expect("first owner");
    let old_token = first.token();
    let initial = fs::metadata(&lock).expect("permanent journal").len();
    first
        .finish_handoff(Duration::ZERO)
        .expect("no active operations");
    assert!(lock.exists());
    let second = SessionOwner::acquire(directory.path()).expect("next owner");
    assert_eq!(second.token().generation(), old_token.generation() + 1);
    assert_eq!(
        fs::metadata(&lock).expect("journal remains").len(),
        initial * 2
    );
    assert_eq!(
        second.status().expect("status").phase,
        Phase::RecoveryRequired
    );
    approve(&second);
    assert!(matches!(
        second.begin(&old_token),
        Err(Error::TokenMismatch)
    ));
}

#[test]
fn revocation_retains_ownership_until_safe_handoff_is_requested() {
    let directory = TestDirectory::new();
    let owner = SessionOwner::acquire(directory.path()).expect("owner");
    approve(&owner);
    owner.revoke();
    assert_eq!(owner.status().expect("status").phase, Phase::Draining);
    assert!(matches!(owner.begin(&owner.token()), Err(Error::Revoked)));
    assert!(matches!(
        SessionOwner::acquire(directory.path()),
        Err(Error::Busy)
    ));
    owner
        .finish_handoff(Duration::ZERO)
        .expect("integration closed the old runtime");
    assert_eq!(owner.status().expect("status").phase, Phase::Released);
    let _next = SessionOwner::acquire(directory.path()).expect("only after explicit safe handoff");
}

#[test]
fn a_timed_out_handoff_refuses_new_owners_and_late_callbacks() {
    let directory = TestDirectory::new();
    let owner = SessionOwner::acquire(directory.path()).expect("owner");
    approve(&owner);
    let operation = owner
        .begin(&owner.token())
        .expect("in-flight native operation");
    assert!(matches!(
        owner.finish_handoff(Duration::ZERO),
        Err(Error::DrainTimeout {
            active_operations: 1
        })
    ));
    assert!(matches!(
        SessionOwner::acquire(directory.path()),
        Err(Error::Busy)
    ));
    let mut wrote = false;
    assert!(matches!(
        operation.with_valid(|| wrote = true),
        Err(Error::Revoked)
    ));
    assert!(!wrote, "a late callback never entered its write closure");
    drop(operation);
    owner
        .finish_handoff(Duration::from_secs(1))
        .expect("drained");
    let _next = SessionOwner::acquire(directory.path()).expect("next owner after drain");
}

#[test]
fn dropping_the_host_revokes_but_keeps_the_kernel_lock_until_callbacks_drain() {
    let directory = TestDirectory::new();
    let owner = SessionOwner::acquire(directory.path()).expect("owner");
    approve(&owner);
    let operation = owner.begin(&owner.token()).expect("in-flight operation");
    drop(owner);
    assert!(matches!(
        operation.with_valid(|| panic!("must not execute")),
        Err(Error::Revoked)
    ));
    assert!(matches!(
        SessionOwner::acquire(directory.path()),
        Err(Error::Busy)
    ));
    drop(operation);
    let _next = SessionOwner::acquire(directory.path()).expect("callbacks drained");
}

#[test]
fn native_revocation_and_a_write_are_one_ordered_boundary() {
    let directory = TestDirectory::new();
    let owner = SessionOwner::acquire(directory.path()).expect("owner");
    approve(&owner);
    let operation = owner.begin(&owner.token()).expect("operation");
    let (entered, receive_entered) = mpsc::channel();
    let (release, receive_release) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        operation
            .with_valid(|| {
                entered.send(()).expect("signal critical section");
                receive_release
                    .recv()
                    .expect("finish synchronous side effect");
            })
            .expect("admitted before revocation");
        operation
    });
    receive_entered
        .recv_timeout(Duration::from_secs(5))
        .expect("callback started");
    let (revoked, receive_revoked) = mpsc::channel();
    let shutdown = std::thread::spawn(move || {
        owner.revoke();
        revoked.send(()).expect("revocation complete");
        owner
    });
    assert!(
        receive_revoked
            .recv_timeout(Duration::from_millis(30))
            .is_err(),
        "revocation cannot split the check from the running write"
    );
    release.send(()).expect("complete write");
    let operation = worker.join().expect("write worker");
    receive_revoked
        .recv_timeout(Duration::from_secs(5))
        .expect("revoked after write");
    let owner = shutdown.join().expect("shutdown thread");
    assert!(matches!(
        operation.with_valid(|| panic!("late write")),
        Err(Error::Revoked)
    ));
    drop(operation);
    owner.finish_handoff(Duration::ZERO).expect("fully drained");
}

#[test]
fn mobile_runtime_replacement_overlap_is_fenced_by_native_generation() {
    // Host lifecycle simulation, not an Android hardware acceptance test.
    let directory = TestDirectory::new();
    let old_runtime = SessionOwner::acquire(directory.path()).expect("old runtime");
    approve(&old_runtime);
    let old_token = old_runtime.token();
    let queued_callback = old_runtime
        .begin(&old_token)
        .expect("old component callback");
    assert!(matches!(
        SessionOwner::acquire(directory.path()),
        Err(Error::Busy)
    ));
    old_runtime.revoke();
    assert!(matches!(
        queued_callback.with_valid(|| panic!("must not issue a model request")),
        Err(Error::Revoked)
    ));
    drop(queued_callback);
    assert!(
        matches!(SessionOwner::acquire(directory.path()), Err(Error::Busy)),
        "a component with no callbacks still must finish closing"
    );
    old_runtime
        .finish_handoff(Duration::ZERO)
        .expect("old component has shut down safely");
    let new_runtime = SessionOwner::acquire(directory.path()).expect("replacement runtime");
    assert!(new_runtime.token().generation() > old_token.generation());
    assert!(matches!(
        new_runtime.begin(&new_runtime.token()),
        Err(Error::RecoveryRequired)
    ));
    approve(&new_runtime);
    assert!(matches!(
        new_runtime.begin(&old_token),
        Err(Error::TokenMismatch)
    ));
}

#[test]
fn damaged_generation_journal_refuses_ownership_without_repairing_it() {
    let directory = TestDirectory::new();
    let owner = SessionOwner::acquire(directory.path()).expect("owner");
    drop(owner);
    let lock = directory.path().join(OWNER_FILE_NAME);
    let mut bytes = fs::read(&lock).expect("journal");
    bytes.push(1); // A process interrupted while appending, not a graceful stop.
    fs::write(&lock, &bytes).expect("inject fault only into isolated storage");
    assert!(matches!(
        SessionOwner::acquire(directory.path()),
        Err(Error::InvalidJournal)
    ));
    assert_eq!(fs::read(lock).expect("unchanged journal"), bytes);
}

#[test]
fn an_existing_empty_journal_cannot_reuse_generation_one() {
    let directory = TestDirectory::new();
    let owner = SessionOwner::acquire(directory.path()).expect("first owner");
    assert_eq!(owner.token().generation(), 1);
    drop(owner);
    let lock = directory.path().join(OWNER_FILE_NAME);
    fs::write(&lock, []).expect("inject truncation only into isolated storage");
    assert!(matches!(
        SessionOwner::acquire(directory.path()),
        Err(Error::InvalidJournal)
    ));
    assert_eq!(fs::metadata(lock).expect("journal unchanged").len(), 0);
}

#[test]
fn malformed_complete_records_are_not_treated_as_a_fresh_store() {
    for index in [0, 8, 16] {
        let directory = TestDirectory::new();
        let owner = SessionOwner::acquire(directory.path()).expect("owner");
        drop(owner);
        let lock = directory.path().join(OWNER_FILE_NAME);
        let mut bytes = fs::read(&lock).expect("journal");
        bytes[index] ^= 0x80; // Magic, generation, and complement respectively.
        fs::write(&lock, &bytes).expect("inject isolated record corruption");
        assert!(matches!(
            SessionOwner::acquire(directory.path()),
            Err(Error::InvalidJournal)
        ));
        assert_eq!(fs::read(lock).expect("no automatic repair"), bytes);
    }
}

#[test]
fn concurrent_first_acquisition_has_one_owner_and_does_not_strand_an_empty_journal() {
    let directory = TestDirectory::new();
    let barrier = Arc::new(Barrier::new(8));
    let workers: Vec<_> = (0..8)
        .map(|_| {
            let path = directory.path().to_owned();
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                barrier.wait();
                SessionOwner::acquire(path)
            })
        })
        .collect();
    let mut owners = Vec::new();
    for worker in workers {
        match worker.join().expect("startup worker") {
            Ok(owner) => owners.push(owner),
            Err(Error::Busy | Error::InvalidJournal) => {}
            Err(error) => panic!("unexpected startup error: {error}"),
        }
    }
    assert_eq!(
        owners.len(),
        1,
        "the atomic creator completes while every other instance is refused"
    );
    assert_eq!(owners[0].token().generation(), 1);
    drop(owners);
    let next =
        SessionOwner::acquire(directory.path()).expect("first initialization was not stranded");
    assert_eq!(next.token().generation(), 2);
}

#[test]
fn callback_panic_fails_closed_until_the_host_is_closed() {
    let directory = TestDirectory::new();
    let owner = SessionOwner::acquire(directory.path()).expect("owner");
    approve(&owner);
    let operation = owner.begin(&owner.token()).expect("operation");
    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = operation.with_valid(|| panic!("injected callback failure"));
    }));
    assert!(panic.is_err());
    assert!(matches!(owner.begin(&owner.token()), Err(Error::Poisoned)));
    assert!(matches!(
        operation.with_valid(|| panic!("must not run again")),
        Err(Error::Poisoned)
    ));
    drop(operation);
    assert!(matches!(
        SessionOwner::acquire(directory.path()),
        Err(Error::Busy)
    ));
    owner
        .finish_handoff(Duration::ZERO)
        .expect("poisoned host can close safely");
    let _next = SessionOwner::acquire(directory.path()).expect("new owner still requires recovery");
}

struct RunningChild {
    child: Child,
    generation: u64,
}

impl RunningChild {
    fn start(root: &Path) -> Self {
        let mut child = Command::new(std::env::current_exe().expect("current test binary"))
            .args(["--ignored", "--exact", "owner_child_process", "--nocapture"])
            .env("BK_CAT_OWNER_CHILD_ROOT", root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("spawn actual second process");
        let output = child.stdout.take().expect("child output");
        let (ready, receive_ready) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(output).lines() {
                let Ok(line) = line else { break };
                if let Some((_, generation)) = line.split_once("OWNER_READY:") {
                    let _ = ready.send(generation.trim().parse::<u64>().expect("child generation"));
                    break;
                }
            }
        });
        match receive_ready.recv_timeout(Duration::from_secs(15)) {
            Ok(generation) => Self { child, generation },
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                panic!("child never acquired ownership: {error}");
            }
        }
    }

    fn crash(&mut self) {
        self.child
            .kill()
            .expect("terminate without running Rust destructors");
        let status = self
            .child
            .wait()
            .expect("wait for actual process termination");
        assert!(
            !status.success(),
            "the fault is an abnormal exit, not a graceful stop"
        );
    }
}

impl Drop for RunningChild {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

#[test]
fn desktop_second_process_is_refused_by_the_operating_system_lock() {
    let directory = TestDirectory::new();
    let _child = RunningChild::start(directory.path());
    assert!(matches!(
        SessionOwner::acquire(directory.path()),
        Err(Error::Busy)
    ));
}

#[test]
fn killed_owner_can_be_taken_over_but_never_automatically_resumes() {
    let directory = TestDirectory::new();
    let mut child = RunningChild::start(directory.path());
    let old_generation = child.generation;
    child.crash();
    let next =
        SessionOwner::acquire(directory.path()).expect("kernel releases a killed process's lock");
    assert_eq!(next.token().generation(), old_generation + 1);
    assert_eq!(
        next.status().expect("status").phase,
        Phase::RecoveryRequired
    );
    assert!(matches!(
        next.begin(&next.token()),
        Err(Error::RecoveryRequired)
    ));
    approve(&next);
    let operation = next
        .begin(&next.token())
        .expect("user explicitly continued");
    operation
        .with_valid(|| ())
        .expect("new-generation work only");
}

#[test]
#[ignore = "subprocess helper; invoked explicitly by the real multiprocess tests"]
fn owner_child_process() {
    let Some(root) = std::env::var_os("BK_CAT_OWNER_CHILD_ROOT") else {
        return;
    };
    let owner = SessionOwner::acquire(root).expect("child owner");
    approve(&owner);
    println!("OWNER_READY:{}", owner.token().generation());
    std::io::stdout().flush().expect("publish child handshake");
    // The parent kills this process while ownership is held. Keeping stdin
    // open avoids polling, sleeps, and graceful destructor-based recovery.
    let mut line = String::new();
    std::io::stdin()
        .read_line(&mut line)
        .expect("wait for parent");
    drop(owner);
}
