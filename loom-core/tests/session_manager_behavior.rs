// Behavior tests for LocalSessionManager.
// (Originally TDD Red-phase scaffolding; all tests now exercise real impl.)
//
// Coverage:
//   - test_create_returns_ulid_session_id
//   - test_create_persists_manifest_header
//   - test_unknown_session_returns_session_not_found_error
//   - test_no_implicit_session_creation_on_unknown_id
//   - test_close_transitions_to_closed_status
//   - test_close_finalizes_manifest_with_terminal_entry
//   - test_action_on_closed_session_returns_session_already_closed
//   - test_profile_mutation_returns_session_profile_immutable
//   - test_abort_sets_abort_flag_and_notifies
//   - test_abort_appends_terminal_entry_to_manifest
//   - test_abort_completes_within_1s_wall_clock
//   - test_abort_all_aborts_every_active_session
//   - in_flight_counter_is_balanced_under_concurrent_start_finish

use loom_core::budget_enforcer::{BudgetEnforcer, LocalBudgetEnforcer};
use loom_core::content_store::{ContentStore, LocalContentStore};
use loom_core::error::{LoomError, LoomErrorCode};
use loom_core::manifest_writer::{LocalManifestWriter, ManifestWriter, SessionId};
use loom_core::observability::Observability;
use loom_core::session_manager::{
    AbortReason, LocalSessionManager, SessionCreateOpts, SessionError, SessionStatus,
    TERMINAL_RETENTION_CAP,
};
use loom_core::vault::{KeychainAccess, LocalVault, Vault};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Instant;
use tempfile::TempDir;
use zeroize::Zeroizing;

struct StubKc;
impl KeychainAccess for StubKc {
    fn get_secret(&self, _label: &str) -> Result<Zeroizing<Vec<u8>>, loom_keychain::KeychainError> {
        Ok(Zeroizing::new(vec![0u8; 16]))
    }
    fn set_secret(
        &self,
        _label: &str,
        _secret: Zeroizing<Vec<u8>>,
    ) -> Result<(), loom_keychain::KeychainError> {
        Err(loom_keychain::KeychainError::new(
            loom_keychain::KeychainErrorKind::Unavailable,
            "test stub",
        ))
    }
    fn delete_secret(&self, _label: &str) -> Result<(), loom_keychain::KeychainError> {
        Err(loom_keychain::KeychainError::new(
            loom_keychain::KeychainErrorKind::Unavailable,
            "test stub",
        ))
    }
    fn list_labels(&self) -> Result<Vec<String>, loom_keychain::KeychainError> {
        Err(loom_keychain::KeychainError::new(
            loom_keychain::KeychainErrorKind::Unavailable,
            "test stub",
        ))
    }
}

// Each test gets its own `tempfile::TempDir` (auto-removed on drop) so a stale
// session table / WAL / downloads tree from a previous run can never leak into
// the next — the same hermetic-per-run pattern as manifest_writer_behavior.rs.
// `make_sm()` returns the TempDir alongside the manager so the caller keeps it
// alive (and can resolve on-disk paths from it); bind it as `_tmp` when unused.
fn make_sm() -> (Arc<LocalSessionManager>, TempDir) {
    let tmp = TempDir::new().unwrap();
    let sm = make_sm_at(tmp.path());
    (sm, tmp)
}

fn make_sm_at(root: &std::path::Path) -> Arc<LocalSessionManager> {
    let obs = Observability::new(root.join("loom.log"), false);
    let cs: Arc<dyn ContentStore> =
        Arc::new(LocalContentStore::new(root.join("store"), obs.clone()));
    let mw: Arc<dyn ManifestWriter> =
        Arc::new(LocalManifestWriter::new(root.join("sessions"), obs.clone()));
    let kc: Arc<dyn KeychainAccess> = Arc::new(StubKc);
    let v: Arc<dyn Vault> = Arc::new(LocalVault::new(kc, mw.clone(), obs.clone()));
    let be: Arc<dyn BudgetEnforcer> = Arc::new(LocalBudgetEnforcer::new(obs.clone()));
    LocalSessionManager::new(cs, mw, v, be, obs, 0, root.join("sessions"))
}

fn default_opts() -> SessionCreateOpts {
    SessionCreateOpts {
        agent_id: "test-agent".into(),
        surface: "test".into(),
        seed: None,
        limits: None,
        replay_of: None,
        started_at_ms_override: None,
        capture_policy: None,
        no_blocklist: false,
        no_determinism: false,
        record_screencast: false,
        audio: false,
        profile: "safe".to_string(),
    }
}

// === idle-reaper activity tracking + two-phase eviction ===

#[test]
fn activity_methods_track_in_flight_and_last_activity() {
    let (sm, _tmp) = make_sm();
    let id = sm.create(default_opts()).unwrap();
    let s = sm.get(id).unwrap();
    assert_eq!(s.in_flight(), 0);
    let base = s.last_activity();

    s.action_started(base + 5_000);
    assert_eq!(s.in_flight(), 1, "action_started bumps in-flight");
    assert_eq!(s.last_activity(), base + 5_000, "action_started touches");

    s.action_finished(base + 9_000);
    assert_eq!(s.in_flight(), 0, "action_finished decrements");
    assert_eq!(s.last_activity(), base + 9_000, "action_finished touches");

    // Unbalanced finish floors at 0 (never underflows).
    s.action_finished(base + 9_500);
    assert_eq!(s.in_flight(), 0);
}

#[test]
fn in_flight_counter_is_balanced_under_concurrent_start_finish() {
    // `action_finished` decrements through a hand-rolled compare-exchange loop so it
    // can floor at 0. Prove that loop keeps the counter exact under contention: it
    // must not lose a decrement to a racing thread, nor underflow on the extra
    // finishes that outnumber the starts.
    let (sm, _tmp) = make_sm();
    let id = sm.create(default_opts()).unwrap();
    let s = sm.get(id).unwrap();
    let base = s.last_activity();

    const THREADS: usize = 8;
    const PER_THREAD: usize = 500;

    // Each thread balances its own starts and finishes, so the net effect is zero.
    let mut handles = Vec::with_capacity(THREADS);
    for _ in 0..THREADS {
        let s = Arc::clone(&s);
        handles.push(std::thread::spawn(move || {
            for _ in 0..PER_THREAD {
                s.action_started(base);
                s.action_finished(base);
            }
        }));
    }
    for h in handles {
        h.join().unwrap();
    }
    assert_eq!(
        s.in_flight(),
        0,
        "balanced concurrent start/finish must settle back to 0"
    );

    // Now race unbalanced finishes against the floor: every thread over-finishes.
    s.action_started(base);
    let mut handles = Vec::with_capacity(THREADS);
    for _ in 0..THREADS {
        let s = Arc::clone(&s);
        handles.push(std::thread::spawn(move || {
            for _ in 0..PER_THREAD {
                s.action_finished(base);
            }
        }));
    }
    for h in handles {
        h.join().unwrap();
    }
    assert_eq!(
        s.in_flight(),
        0,
        "concurrent over-finish must saturate at 0, never wrap"
    );
}

#[test]
fn evict_if_idle_closes_idle_session_but_spares_busy_and_fresh() {
    let (sm, _tmp) = make_sm();
    let id = sm.create(default_opts()).unwrap();
    let base = sm.get(id.clone()).unwrap().last_activity();
    let ttl = 1_000u64;

    // Fresh: idle_for (0) < ttl → spared.
    assert!(!sm.evict_if_idle(id.clone(), ttl, base).unwrap());
    assert_eq!(
        *sm.get(id.clone()).unwrap().status.lock(),
        SessionStatus::Active
    );

    // Busy: in-flight > 0 → spared even when idle past ttl (hardening A / C5).
    sm.get(id.clone()).unwrap().action_started(base);
    assert!(!sm.evict_if_idle(id.clone(), ttl, base + 10_000).unwrap());
    assert_eq!(
        *sm.get(id.clone()).unwrap().status.lock(),
        SessionStatus::Active
    );
    sm.get(id.clone()).unwrap().action_finished(base);

    // Idle past ttl, no in-flight → evicted (Closed + idle_ttl terminal reason).
    assert!(sm.evict_if_idle(id.clone(), ttl, base + 10_000).unwrap());
    assert_eq!(
        *sm.get(id.clone()).unwrap().status.lock(),
        SessionStatus::Closed
    );

    // Idempotent: a second evict on a now-closed session is a no-op (Ok(false), not error).
    assert!(!sm.evict_if_idle(id, ttl, base + 20_000).unwrap());
}

#[test]
fn oldest_active_age_tracks_least_recently_active() {
    let (sm, _tmp) = make_sm();
    assert_eq!(
        sm.oldest_active_age_secs(1_000_000),
        None,
        "no sessions → None"
    );
    let id = sm.create(default_opts()).unwrap();
    let base = sm.get(id).unwrap().last_activity();
    // 30s after last activity.
    assert_eq!(sm.oldest_active_age_secs(base + 30_000), Some(30));
}

// === session creation + ULID ===

#[test]
fn test_create_returns_ulid_session_id() {
    let (sm, _tmp) = make_sm();
    let id = sm.create(default_opts()).unwrap();
    // ULID: 26 chars, Crockford base32 lowercase.
    assert_eq!(id.0.len(), 26, "session id must be 26-char ULID");
    assert!(
        id.0.chars()
            .all(|c: char| c.is_ascii_digit() || c.is_ascii_lowercase()),
        "session id must be lowercase alphanumeric Crockford base32"
    );
}

#[test]
fn test_create_persists_manifest_header() {
    let (sm, tmp) = make_sm();
    let id = sm.create(default_opts()).unwrap();
    let wal_path = tmp.path().join("sessions").join(&id.0).join("manifest.wal");
    assert!(wal_path.exists(), "manifest.wal must exist after create");
    let contents = std::fs::read_to_string(&wal_path).unwrap();
    let first_line: serde_json::Value = serde_json::from_str(contents.lines().next().unwrap())
        .expect("first WAL line must be valid JSON");
    assert_eq!(
        first_line["kind"], "header",
        "first manifest entry must be a Header"
    );
    assert_eq!(first_line["session_id"], id.0.as_str());
}

// === unknown session returns error ===

#[test]
fn test_unknown_session_returns_session_not_found_error() {
    let (sm, _tmp) = make_sm();
    let result = sm.get(SessionId("00000000000000000000000000".into()));
    let err = match result {
        Err(e) => e,
        Ok(_) => panic!("expected SessionNotFound error"),
    };
    assert_eq!(err.code, LoomErrorCode::SessionNotFound);
}

#[test]
fn test_no_implicit_session_creation_on_unknown_id() {
    let (sm, tmp) = make_sm();
    let unknown = SessionId("00000000000000000000000000".into());
    let _ = sm.get(unknown.clone());
    let session_dir = tmp.path().join("sessions").join(&unknown.0);
    assert!(
        !session_dir.exists(),
        "get on unknown id must not create session directory"
    );
}

// === close lifecycle ===

#[tokio::test]
async fn test_close_transitions_to_closed_status() {
    let (sm, _tmp) = make_sm();
    let id = sm.create(default_opts()).unwrap();
    sm.close(id.clone()).unwrap();
    let session = sm.get(id).unwrap();
    // parking_lot::Mutex — sync lock, no await needed
    let status = *session.status.lock();
    assert_eq!(status, SessionStatus::Closed);
}

#[test]
fn test_close_finalizes_manifest_with_terminal_entry() {
    let (sm, tmp) = make_sm();
    let id = sm.create(default_opts()).unwrap();
    sm.close(id.clone()).unwrap();
    let wal_path = tmp.path().join("sessions").join(&id.0).join("manifest.wal");
    let contents = std::fs::read_to_string(&wal_path).unwrap();
    let has_terminal = contents.lines().any(|line| {
        serde_json::from_str::<serde_json::Value>(line)
            .map(|v| v["kind"] == "session_terminal")
            .unwrap_or(false)
    });
    assert!(
        has_terminal,
        "close must append SessionTerminal entry to manifest"
    );
}

#[test]
fn test_action_on_closed_session_returns_session_already_closed() {
    let (sm, _tmp) = make_sm();
    let id = sm.create(default_opts()).unwrap();
    sm.close(id.clone()).unwrap();
    // Second close on already-closed session must return SessionAlreadyClosed.
    let err = sm.close(id).unwrap_err();
    assert_eq!(err.code, LoomErrorCode::SessionAlreadyClosed);
}

// === profile immutability ===

#[test]
fn test_profile_mutation_returns_session_profile_immutable() {
    // LocalSessionManager exposes no update_profile() method.
    // The facade returns SessionError::SessionProfileImmutable if profile
    // mutation is attempted. Verify the variant compiles (shape check).
    let _e = SessionError::SessionProfileImmutable;
    // Verify it maps to InvalidArgument at the facade boundary.
    let loom_err: LoomError = _e.into();
    assert_eq!(loom_err.code, LoomErrorCode::InvalidArgument);
}

// === abort signal ===

#[tokio::test]
async fn test_abort_sets_abort_flag_and_notifies() {
    let (sm, _tmp) = make_sm();
    let id = sm.create(default_opts()).unwrap();
    sm.abort(
        id.clone(),
        AbortReason {
            reason: "test".into(),
        },
    )
    .unwrap();
    // get() should return the session (still in DashMap, now Aborted status)
    let session = match sm.get(id) {
        Ok(s) => s,
        Err(_) => panic!("aborted session should still be retrievable"),
    };
    assert!(
        session.abort_flag.load(Ordering::Acquire),
        "abort() must set abort_flag to true"
    );
}

#[test]
fn test_abort_appends_terminal_entry_to_manifest() {
    let (sm, tmp) = make_sm();
    let id = sm.create(default_opts()).unwrap();
    sm.abort(
        id.clone(),
        AbortReason {
            reason: "user_request".into(),
        },
    )
    .unwrap();
    // Give the async task up to 1s to write the terminal entry.
    let wal_path = tmp.path().join("sessions").join(&id.0).join("manifest.wal");
    let deadline = Instant::now() + std::time::Duration::from_secs(1);
    loop {
        let contents = std::fs::read_to_string(&wal_path).unwrap_or_default();
        let has_terminal = contents.lines().any(|line| {
            serde_json::from_str::<serde_json::Value>(line)
                .map(|v| v["kind"] == "session_terminal")
                .unwrap_or(false)
        });
        if has_terminal {
            break;
        }
        if Instant::now() > deadline {
            panic!("abort() must write SessionTerminal to manifest within 1s");
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

#[test]
fn test_abort_completes_within_1s_wall_clock() {
    let (sm, _tmp) = make_sm();
    let id = sm.create(default_opts()).unwrap();
    let start = Instant::now();
    sm.abort(
        id,
        AbortReason {
            reason: "timing_test".into(),
        },
    )
    .unwrap();
    let elapsed = start.elapsed();
    assert!(
        elapsed.as_millis() < 1000,
        "abort() must return within 1s; took {}ms",
        elapsed.as_millis()
    );
}

// === abort_all ===

#[test]
fn test_abort_all_aborts_every_active_session() {
    let (sm, _tmp) = make_sm();
    let id1 = sm.create(default_opts()).unwrap();
    let id2 = sm.create(default_opts()).unwrap();
    let id3 = sm.create(default_opts()).unwrap();
    sm.abort_all(AbortReason {
        reason: "sigterm".into(),
    })
    .unwrap();
    // All sessions must have their abort_flag set within 1s.
    let deadline = Instant::now() + std::time::Duration::from_secs(1);
    for id in [id1, id2, id3] {
        let session = sm.get(id).unwrap();
        loop {
            if session.abort_flag.load(Ordering::Acquire) {
                break;
            }
            if Instant::now() > deadline {
                panic!("abort_all() must signal all sessions within 1s");
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }
}

// === bounded terminal retention (audit 2026-06-10) ===
//
// The in-memory session table used to have exactly one insert and zero
// removes — every Arc<Session> lived for the daemon's lifetime. Terminal
// sessions are now retained in a FIFO capped at TERMINAL_RETENTION_CAP;
// overflow evicts the OLDEST terminal session from the table (disk stays
// the source of truth for historical queries).

#[test]
fn terminal_sessions_evicted_beyond_retention_cap() {
    let (sm, _tmp) = make_sm();

    // An ACTIVE session must survive any amount of terminal churn.
    let active_id = sm.create(default_opts()).unwrap();

    // Oldest terminal session — will be pushed out of the retention window.
    let first_closed = sm.create(default_opts()).unwrap();
    sm.close(first_closed.clone()).unwrap();

    // Fill the retention window past the cap (mix close + abort: both are
    // terminal transitions and both must count against the FIFO).
    let mut recent = Vec::new();
    for i in 0..TERMINAL_RETENTION_CAP {
        let id = sm.create(default_opts()).unwrap();
        if i % 2 == 0 {
            sm.close(id.clone()).unwrap();
        } else {
            sm.abort(
                id.clone(),
                AbortReason {
                    reason: "test-churn".into(),
                },
            )
            .unwrap();
        }
        recent.push(id);
    }

    // The oldest terminal session is evicted: lookups degrade to the same
    // SessionNotFound a daemon restart would produce.
    let err = match sm.get(first_closed.clone()) {
        Err(e) => e,
        Ok(_) => panic!("oldest terminal session must be evicted from the in-memory table"),
    };
    assert_eq!(
        err.code,
        LoomErrorCode::SessionNotFound,
        "evicted terminal session must answer SessionNotFound"
    );
    assert!(
        !sm.live_session_ids().contains(&first_closed),
        "evicted terminal session must leave live_session_ids (unshields orphan GC)"
    );

    // The MOST RECENT terminal sessions keep their typed-error behaviour.
    let last_closed = recent.last().unwrap().clone();
    assert!(
        sm.get(last_closed.clone()).is_ok(),
        "recent terminal sessions stay in the table"
    );
    let err = sm.close(last_closed).unwrap_err();
    assert_eq!(
        err.code,
        LoomErrorCode::SessionAlreadyClosed,
        "recent terminal sessions still answer SessionAlreadyClosed (not NotFound)"
    );

    // Active sessions are NEVER evicted by terminal churn.
    let active = sm
        .get(active_id)
        .expect("active session must survive terminal churn");
    assert_eq!(*active.status.lock(), SessionStatus::Active);

    // The table is bounded: 1 active + at most TERMINAL_RETENTION_CAP terminal.
    assert!(
        sm.live_session_ids().len() <= TERMINAL_RETENTION_CAP + 1,
        "table must stay bounded at cap + active sessions; got {}",
        sm.live_session_ids().len()
    );
}

#[test]
fn terminal_retention_keeps_sessions_within_cap() {
    let (sm, _tmp) = make_sm();

    // Closing FEWER than cap sessions evicts nothing — every terminal
    // session stays addressable with its typed status.
    let mut ids = Vec::new();
    for _ in 0..3 {
        let id = sm.create(default_opts()).unwrap();
        sm.close(id.clone()).unwrap();
        ids.push(id);
    }
    for id in ids {
        let session = sm.get(id).expect("within-cap terminal session retained");
        assert_eq!(*session.status.lock(), SessionStatus::Closed);
    }
}
// === active_session_count (daemon concurrency-cap source) ===

#[test]
fn test_active_session_count_tracks_fsm_transitions() {
    // The daemon's session-cap check reads this count on EVERY session.create
    // (instead of re-parsing every WAL on disk) — it must track the in-memory
    // FSM exactly: create increments, close/abort immediately decrement.
    let (sm, _tmp) = make_sm();
    assert_eq!(sm.active_session_count(), 0);

    let id1 = sm.create(default_opts()).unwrap();
    let id2 = sm.create(default_opts()).unwrap();
    assert_eq!(sm.active_session_count(), 2);

    sm.close(id1).unwrap();
    assert_eq!(
        sm.active_session_count(),
        1,
        "closed session must leave the active count (frees a cap slot)"
    );

    sm.abort(
        id2,
        AbortReason {
            reason: "test".into(),
        },
    )
    .unwrap();
    assert_eq!(
        sm.active_session_count(),
        0,
        "aborted session must leave the active count (frees a cap slot)"
    );
}
