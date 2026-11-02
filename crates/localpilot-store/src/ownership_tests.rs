use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};

use localpilot_core::{Message, Role};

use super::*;
use crate::{RetentionPolicy, SessionEventKind, Store};

const CHILD_ENV: &str = "LOCALPILOT_OWNERSHIP_TEST_CHILD";

fn store() -> (tempfile::TempDir, Store) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path());
    (dir, store)
}

fn holder() -> Holder {
    Holder::this_process(HolderKind::Tui)
}

fn transfer(id: &str) -> TransferRecord {
    TransferRecord {
        id: id.to_string(),
        state: TransferState::Released,
        tail: None,
        manifest_digest: "d".to_string(),
    }
}

fn claim(store: &Store, session: SessionId) -> SessionClaim {
    store.claim_session(session, holder()).unwrap().unwrap()
}

fn append(store: &Store, session: SessionId) -> Result<(), StoreError> {
    store
        .append_event(session, None, SessionEventKind::SessionClosed)
        .map(|_| ())
}

fn prune_everything(store: &Store) {
    store
        .prune(
            RetentionPolicy {
                max_sessions: 0,
                max_age_days: 1,
            },
            u64::MAX / 2,
            false,
        )
        .unwrap();
}

#[test]
fn a_claimed_session_is_busy_for_every_other_claim_until_it_is_dropped() {
    let (_dir, store) = store();
    let session = SessionId::new();
    let first = claim(&store, session);
    assert!(matches!(
        store.claim_session(session, holder()),
        Err(StoreError::SessionBusy {
            holder: Some(_),
            ..
        })
    ));
    drop(first);
    let second = claim(&store, session);
    assert_eq!(second.generation(), 2, "every claim is one generation more");
    assert_eq!(second.previous().map(|r| r.generation), Some(1));
}

#[test]
fn a_release_fences_every_handle_of_this_process_and_frees_the_session() {
    let (_dir, store) = store();
    let session = SessionId::new();
    let stale = store.clone();
    let owner = claim(&store, session);
    append(&stale, session).unwrap();
    owner.release(transfer("t1")).unwrap();

    assert!(matches!(
        append(&stale, session),
        Err(StoreError::OwnershipLost { .. })
    ));
    assert!(matches!(
        stale.append_message(session, &Message::text(Role::User, "late")),
        Err(StoreError::OwnershipLost { .. })
    ));
    assert!(matches!(
        stale.set_session_name(session, "late"),
        Err(StoreError::OwnershipLost { .. })
    ));
    let record = store.owner_record(session).unwrap().unwrap();
    assert_eq!(
        record.generation, 1,
        "a release keeps its claim's generation"
    );
    let released = record.transfer.unwrap();
    assert_eq!(released.id, "t1");
    assert_eq!(released.state, TransferState::Released);
}

#[test]
fn a_failed_release_publish_keeps_the_source_the_sole_live_owner() {
    let (dir, store) = store();
    let session = SessionId::new();
    let owner = claim(&store, session);
    // Make the record path unpublishable.
    let record = record_path(&dir.path().join(".localpilot/sessions"), session);
    fs::remove_file(&record).unwrap();
    fs::create_dir(&record).unwrap();
    fs::write(record.join("block"), b"x").unwrap();

    let (owner, _error) = owner.release(transfer("t1")).unwrap_err();
    append(&store, session).expect("the lease is live again");
    assert!(
        matches!(
            store.claim_session(session, holder()),
            Err(StoreError::SessionBusy { .. })
        ),
        "the lock was never let go"
    );
    drop(owner);
}

#[test]
fn after_a_claim_is_dropped_a_write_needs_the_free_lock() {
    let (_dir, store) = store();
    let session = SessionId::new();
    drop(claim(&store, session));
    append(&store, session).unwrap();
}

#[test]
fn an_owner_record_of_an_unknown_version_refuses_the_claim() {
    let (dir, store) = store();
    let session = SessionId::new();
    let sessions = dir.path().join(".localpilot/sessions");
    fs::create_dir_all(&sessions).unwrap();
    fs::write(record_path(&sessions, session), br#"{"v":9}"#).unwrap();
    assert!(matches!(
        store.claim_session(session, holder()),
        Err(StoreError::UnsupportedFormat { found: 9, .. })
    ));
}

#[test]
fn the_destination_reads_the_released_transfer_and_commits_durably() {
    let (_dir, store) = store();
    let session = SessionId::new();
    claim(&store, session).release(transfer("t9")).unwrap();
    let mut destination = store
        .claim_session(session, Holder::this_process(HolderKind::Server))
        .unwrap()
        .unwrap();
    let released = destination.previous().unwrap().transfer.clone().unwrap();
    assert_eq!(released.state, TransferState::Released);
    assert_eq!(destination.generation(), 2);
    destination.commit_transfer(released).unwrap();
    append(&store, session).expect("the destination's lease is live");
    let record = store.owner_record(session).unwrap().unwrap();
    assert_eq!(record.transfer.unwrap().state, TransferState::Committed);
    assert_eq!(record.holder.kind, HolderKind::Server);
}

#[test]
fn an_incognito_store_has_no_owners() {
    let store = Store::ephemeral();
    assert!(store
        .claim_session(SessionId::new(), holder())
        .unwrap()
        .is_none());
}

#[test]
fn prune_skips_a_session_another_owner_holds_and_keeps_every_lock_file() {
    let (dir, store) = store();
    let held = SessionId::new();
    let idle = SessionId::new();
    store
        .append_message(idle, &Message::text(Role::User, "a"))
        .unwrap();
    store
        .append_message(held, &Message::text(Role::User, "b"))
        .unwrap();
    drop(claim(&store, idle));
    let owner = claim(&store, held);
    prune_everything(&store);
    let ids: Vec<SessionId> = store
        .list_sessions()
        .unwrap()
        .iter()
        .map(|e| e.id)
        .collect();
    assert_eq!(ids, vec![held]);
    let sessions = dir.path().join(".localpilot/sessions");
    assert!(
        lock_path(&sessions, idle).exists(),
        "lock files are permanent"
    );
    assert!(!record_path(&sessions, idle).exists());
    drop(owner);
}

// --- cross-process ----------------------------------------------------------

/// Re-run this test binary as a child that executes `mode` against `dir`.
fn child(mode: &str, dir: &Path, session: SessionId) -> Child {
    Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "ownership::tests::ownership_test_child",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CHILD_ENV, format!("{mode}|{}|{session}", dir.display()))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap()
}

fn wait_for_line(child: &mut Child, want: &str) {
    let stdout = child.stdout.as_mut().unwrap();
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    loop {
        line.clear();
        assert!(
            reader.read_line(&mut line).unwrap() > 0,
            "child ended before {want}"
        );
        // The test harness may print the test name on the same line.
        if line.trim_end().ends_with(want) {
            return;
        }
    }
}

/// The child side of the cross-process tests; a no-op in a normal run.
#[test]
fn ownership_test_child() {
    let Ok(spec) = std::env::var(CHILD_ENV) else {
        return;
    };
    let mut parts = spec.splitn(3, '|');
    let mode = parts.next().unwrap().to_string();
    let dir = PathBuf::from(parts.next().unwrap());
    let session = parts.next().unwrap().parse::<SessionId>().unwrap();
    let store = Store::open(&dir);
    let mut stdin = String::new();
    match mode.as_str() {
        "hold" => {
            let _claim = claim(&store, session);
            println!("held");
            let _ = std::io::stdin().read_line(&mut stdin);
        }
        "append" => {
            // Each child appends to its own new session.
            let own = SessionId::new();
            for n in 0..150 {
                store
                    .append_message(own, &Message::text(Role::User, n.to_string()))
                    .unwrap();
            }
        }
        "name" => {
            let mut won = 0;
            for round in 0..40 {
                // A new session per round, so a won name is never renamed away.
                if store
                    .set_session_name(SessionId::new(), &format!("race-{round}"))
                    .is_ok()
                {
                    won += 1;
                }
            }
            println!("won {won}");
        }
        "open-then-lock" => {
            let sessions = dir.join(".localpilot/sessions");
            let file = fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(lock_path(&sessions, session))
                .unwrap();
            println!("opened");
            let _ = std::io::stdin().read_line(&mut stdin);
            let mut lock = fd_lock::RwLock::new(file);
            let _held = lock.try_write().unwrap();
            println!("locked");
            stdin.clear();
            let _ = std::io::stdin().read_line(&mut stdin);
        }
        other => panic!("unknown child mode {other}"),
    }
    let _ = std::io::stdout().flush();
}

#[test]
fn a_killed_owner_process_frees_its_session_without_any_timeout() {
    let (dir, store) = store();
    let session = SessionId::new();
    let mut holder_process = child("hold", dir.path(), session);
    wait_for_line(&mut holder_process, "held");
    assert!(matches!(
        store.claim_session(session, holder()),
        Err(StoreError::SessionBusy {
            holder: Some(_),
            ..
        })
    ));
    holder_process.kill().unwrap();
    holder_process.wait().unwrap();
    let claim = store
        .claim_session_within(session, holder(), Duration::from_secs(10))
        .unwrap()
        .unwrap();
    assert_eq!(claim.generation(), 2);
}

#[test]
fn processes_updating_different_sessions_never_lose_an_index_update() {
    let (dir, store) = store();
    let children: Vec<Child> = (0..3)
        .map(|_| child("append", dir.path(), SessionId::new()))
        .collect();
    for child in children {
        assert!(child.wait_with_output().unwrap().status.success());
    }
    let entries = store.list_sessions().unwrap();
    assert_eq!(entries.len(), 3, "every session survives in the index");
    assert!(entries.iter().all(|e| e.message_count == 150));
}

#[test]
fn processes_racing_for_one_name_leave_exactly_one_winner_per_name() {
    let (dir, store) = store();
    let children: Vec<Child> = (0..3)
        .map(|_| child("name", dir.path(), SessionId::new()))
        .collect();
    let mut total = 0;
    for child in children {
        let out = child.wait_with_output().unwrap();
        assert!(out.status.success());
        let text = String::from_utf8_lossy(&out.stdout).into_owned();
        total += text
            .lines()
            .find_map(|l| l.split("won ").nth(1))
            .unwrap()
            .trim()
            .parse::<usize>()
            .unwrap();
    }
    assert_eq!(total, 40, "each name was won by exactly one session");
    for round in 0..40 {
        assert!(store
            .find_session_by_name(&format!("race-{round}"))
            .unwrap()
            .is_some());
    }
}

#[test]
fn an_opener_paused_across_prune_still_locks_the_one_shared_lock_file() {
    let (dir, store) = store();
    let session = SessionId::new();
    store
        .append_message(session, &Message::text(Role::User, "old"))
        .unwrap();
    drop(claim(&store, session));
    let mut opener = child("open-then-lock", dir.path(), session);
    wait_for_line(&mut opener, "opened");
    prune_everything(&store);
    assert!(
        store.list_sessions().unwrap().is_empty(),
        "the session was pruned"
    );
    opener.stdin.as_mut().unwrap().write_all(b"go\n").unwrap();
    wait_for_line(&mut opener, "locked");
    assert!(
        matches!(
            store.claim_session(session, holder()),
            Err(StoreError::SessionBusy { .. })
        ),
        "the paused opener and a new opener share one lock file"
    );
    opener.kill().unwrap();
    opener.wait().unwrap();
}

// --- review regressions: writes must carry their own authority ---------------

#[test]
fn a_stale_handle_stays_fenced_after_another_handle_reclaims_in_this_process() {
    let (dir, store) = store();
    let session = SessionId::new();
    let stale = store.clone();
    claim(&store, session).release(transfer("t1")).unwrap();
    let other = Store::open(dir.path());
    let _new_owner = claim(&other, session);
    assert!(
        append(&stale, session).is_err(),
        "the released owner's handles stay dead"
    );
    append(&other, session).expect("the new owner writes");
}

#[test]
fn a_handle_that_never_claimed_cannot_write_to_a_session_another_process_holds() {
    let (dir, store) = store();
    let session = SessionId::new();
    let mut holder_process = child("hold", dir.path(), session);
    wait_for_line(&mut holder_process, "held");
    assert!(append(&store, session).is_err());
    assert!(store
        .append_message(session, &Message::text(Role::User, "x"))
        .is_err());
    assert!(store.set_session_name(session, "taken").is_err());
    holder_process.kill().unwrap();
    holder_process.wait().unwrap();
}

#[test]
fn a_differently_spelled_root_cannot_write_to_an_owned_session() {
    let (dir, store) = store();
    let session = SessionId::new();
    let _owner = claim(&store, session);
    let alias = Store::at(dir.path().join(".").join(".localpilot"));
    assert!(append(&alias, session).is_err());
}

#[test]
fn a_dry_run_prune_changes_no_owner_record() {
    let (dir, store) = store();
    let session = SessionId::new();
    store
        .append_message(session, &Message::text(Role::User, "old"))
        .unwrap();
    claim(&store, session).release(transfer("t1")).unwrap();
    let record = record_path(&dir.path().join(".localpilot/sessions"), session);
    let before = fs::read(&record).unwrap();
    store
        .prune(
            RetentionPolicy {
                max_sessions: 0,
                max_age_days: 1,
            },
            u64::MAX / 2,
            true,
        )
        .unwrap();
    assert_eq!(fs::read(&record).unwrap(), before);
}

// --- review regressions: a dropped claim revokes its lease first ------------

#[test]
fn dropping_a_claim_waits_for_a_running_write_before_letting_the_session_go() {
    let (dir, store) = store();
    let session = SessionId::new();
    let owner = claim(&store, session);
    let lease = lease_in(&store.leases, session).unwrap();
    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let writer = std::thread::spawn(move || {
        lease.guarded(session, || {
            started_tx.send(()).unwrap();
            std::thread::sleep(Duration::from_millis(300));
            Ok(Instant::now())
        })
    });
    started_rx.recv().unwrap();
    drop(owner);
    let released_at = Instant::now();
    let write_finished_at = writer.join().unwrap().unwrap();
    assert!(
        write_finished_at <= released_at,
        "the claim was let go only after the running write finished"
    );
    let _next = claim(&Store::open(dir.path()), session);
}

#[test]
fn a_lease_kept_across_a_dropped_claim_never_writes_again() {
    let (_dir, store) = store();
    let session = SessionId::new();
    let owner = claim(&store, session);
    let kept = lease_in(&store.leases, session).unwrap();
    drop(owner);
    let _next = claim(&store, session);
    assert!(matches!(
        kept.guarded(session, || Ok(())),
        Err(StoreError::OwnershipLost { .. })
    ));
}
