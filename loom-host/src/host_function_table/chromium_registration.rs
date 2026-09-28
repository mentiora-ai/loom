// Lazy per-session Chromium shim registration (`register_chromium_shim_if_absent`).
// Split out of `host_impl.rs`.

use crate::wit_type_marshaller::loom_surface_bindings::loom::surface::types::HostError;

/// Lazy-register a per-session chromium shim if it isn't already registered,
/// returning the namespaced effective `ShimId` (`chromium:<session>`).
///
/// Clones the `"chromium"` template config, sets the session-scoped
/// `LOOM_SHIM_USER_DATA_DIR`, runs `configure` (callers needing extra env —
/// e.g. `navigate_execute`'s safe-profile block — mutate the config there,
/// BEFORE register), then registers. Shared by navigate / evaluate /
/// set_input_files; each call site keeps its OWN replay-mode guard (distinct
/// error message). `shim_call` is intentionally not a caller — it resolves the
/// template id differently.
pub(crate) fn register_chromium_shim_if_absent(
    shim_manager: &crate::shim_manager::ShimManager,
    session_id_str: &str,
    configure: impl FnOnce(&mut crate::shim_manager::ShimConfig),
) -> Result<crate::shim_manager::ShimId, HostError> {
    use crate::shim_manager::ShimId;
    let effective_id = ShimId(format!("chromium:{}", session_id_str));
    if shim_manager.is_registered(&effective_id) {
        return Ok(effective_id);
    }
    match shim_manager
        .configs
        .get(&ShimId("chromium".to_owned()))
        .map(|c| c.clone())
    {
        Some(mut config) => {
            let dir = std::env::temp_dir().join(format!("loom-chromium-{}", session_id_str));
            config
                .env
                .push(("LOOM_SHIM_USER_DATA_DIR".into(), dir.display().to_string()));
            configure(&mut config);
            shim_manager.register(effective_id.clone(), config);
            Ok(effective_id)
        }
        None => Err(HostError::ShimFailure(
            "no template config registered for shim chromium".to_owned(),
        )),
    }
}

#[cfg(test)]
mod register_chromium_shim_tests {
    use super::{register_chromium_shim_if_absent, HostError};
    use crate::host_observability::HostObservability;
    use crate::shim_manager::{ShimConfig, ShimId, ShimManager};
    use std::sync::Arc;

    fn mgr_with_template() -> Arc<ShimManager> {
        let sm = ShimManager::new(HostObservability::new(false));
        sm.register(ShimId("chromium".to_owned()), ShimConfig::default());
        sm
    }

    #[test]
    fn registers_session_scoped_shim_with_user_data_dir() {
        let sm = mgr_with_template();
        let id = register_chromium_shim_if_absent(&sm, "sess-a", |_| {}).unwrap();
        assert_eq!(id, ShimId("chromium:sess-a".to_owned()));
        assert!(sm.is_registered(&ShimId("chromium:sess-a".to_owned())));
        let cfg = sm
            .configs
            .get(&ShimId("chromium:sess-a".to_owned()))
            .unwrap();
        assert!(cfg
            .env
            .iter()
            .any(|(k, v)| k == "LOOM_SHIM_USER_DATA_DIR" && v.contains("loom-chromium-sess-a")));
    }

    #[test]
    fn configure_closure_can_add_safe_profile_env() {
        // Mirrors navigate_execute's safe-profile block staying at the call site.
        let sm = mgr_with_template();
        register_chromium_shim_if_absent(&sm, "sess-b", |config| {
            config
                .env
                .push(("LOOM_SHIM_PROFILE".to_owned(), "safe".to_owned()));
        })
        .unwrap();
        let cfg = sm
            .configs
            .get(&ShimId("chromium:sess-b".to_owned()))
            .unwrap();
        assert!(cfg
            .env
            .iter()
            .any(|(k, v)| k == "LOOM_SHIM_PROFILE" && v == "safe"));
    }

    #[test]
    fn idempotent_when_already_registered_skips_configure() {
        let sm = mgr_with_template();
        let first = register_chromium_shim_if_absent(&sm, "sess-c", |c| {
            c.env.push(("FIRST".to_owned(), "1".to_owned()));
        })
        .unwrap();
        // Already registered → returns the same id WITHOUT re-running configure.
        let second = register_chromium_shim_if_absent(&sm, "sess-c", |c| {
            c.env.push(("SECOND".to_owned(), "2".to_owned()));
        })
        .unwrap();
        assert_eq!(first, second);
        let cfg = sm
            .configs
            .get(&ShimId("chromium:sess-c".to_owned()))
            .unwrap();
        assert!(cfg.env.iter().any(|(k, _)| k == "FIRST"));
        assert!(!cfg.env.iter().any(|(k, _)| k == "SECOND"));
    }

    #[test]
    fn missing_template_returns_shim_failure() {
        let sm = ShimManager::new(HostObservability::new(false)); // no "chromium" template
        let err = register_chromium_shim_if_absent(&sm, "sess-d", |_| {}).unwrap_err();
        assert!(matches!(err, HostError::ShimFailure(_)));
    }
}
