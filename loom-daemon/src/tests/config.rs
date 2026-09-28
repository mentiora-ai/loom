use super::*;

#[test]
fn data_root_flag_derives_log_path_when_log_path_not_explicit() {
    let args = with_parse_args_env(None, || {
        parse_args(&argv(&["loom-daemon", "--data-root", "/srv/loom"]))
    });
    assert_eq!(args.data_root, PathBuf::from("/srv/loom"));
    assert_eq!(args.log_path, PathBuf::from("/srv/loom/daemon.log"));
}

#[test]
fn data_root_flag_does_not_clobber_explicit_loom_log_path() {
    let args = with_parse_args_env(Some("/var/log/custom-loom.log"), || {
        parse_args(&argv(&["loom-daemon", "--data-root", "/srv/loom"]))
    });
    assert_eq!(args.data_root, PathBuf::from("/srv/loom"));
    assert_eq!(
        args.log_path,
        PathBuf::from("/var/log/custom-loom.log"),
        "an explicit LOOM_LOG_PATH must win over --data-root's derived default"
    );
}

#[test]
fn explicit_loom_log_path_alone_overrides_default() {
    let args = with_parse_args_env(Some("/var/log/custom-loom.log"), || {
        parse_args(&argv(&["loom-daemon"]))
    });
    assert_eq!(args.log_path, PathBuf::from("/var/log/custom-loom.log"));
}

#[cfg(unix)]
#[test]
fn write_auth_file_0600_creates_with_0600_under_permissive_umask() {
    use std::os::unix::fs::PermissionsExt;
    let dir = test_scratch_dir("auth-0600");
    let path = dir.join("hello.token");
    // umask 0: a plain fs::write would create this 0666 — the regression
    // under test is that the file is NEVER creatable looser than 0600.
    with_umask(0, || {
        write_auth_file_0600(&path, b"tok-secret", "hello.token")
    })
    .expect("write_auth_file_0600");
    let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(
        mode, 0o600,
        "auth file must be CREATED 0600 (no transient world-readable window)"
    );
    assert_eq!(std::fs::read(&path).unwrap(), b"tok-secret");
    let _ = std::fs::remove_dir_all(&dir);
}

#[cfg(unix)]
#[test]
fn write_auth_file_0600_truncates_and_rewrites_existing_file() {
    use std::os::unix::fs::PermissionsExt;
    let dir = test_scratch_dir("auth-0600-rewrite");
    let path = dir.join("hello.token");
    write_auth_file_0600(&path, b"first-token-longer", "hello.token").unwrap();
    // Daemon restart path: same file, new token — must fully replace.
    write_auth_file_0600(&path, b"second", "hello.token").unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), b"second");
    let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn abort_session_raw_without_host_still_aborts() {
    let tmp = test_scratch_dir("abort-no-host");
    let bridge = CoreBridge {
        core: make_core_at(&tmp),
        wasm_host: None,
        cleanup_tasks: Arc::new(std::sync::Mutex::new(tokio::task::JoinSet::new())),
    };
    let sid = create_session_via(&bridge);
    bridge
        .abort_session_raw(&sid, "test-abort")
        .expect("abort without a WasmHost must still succeed");
    assert_eq!(bridge.cleanup_tasks.lock().unwrap().len(), 0);
    let _ = std::fs::remove_dir_all(&tmp);
}
