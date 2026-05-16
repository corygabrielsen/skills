//! OODA PR-side state-root resolution.
//!
//! Single source of truth for the chain used by every PR-side
//! binary (`ooda-pr`, `ooda-prs`, `ooda-pr-codex-review`) and the
//! `ooda-attest` CLI:
//!
//! 1. `explicit` (the `--state-root PATH` flag), if `Some`.
//! 2. `$OODA_PR_STATE_HOME`, if set and non-empty.
//! 3. `$XDG_STATE_HOME/ooda-pr`, if `XDG_STATE_HOME` is set and
//!    non-empty.
//! 4. `$HOME/.local/state/ooda-pr`, if `HOME` is set and non-empty.
//! 5. `$TMPDIR/ooda-pr` (via [`std::env::temp_dir`]) as a final
//!    fallback so the function is total.
//!
//! `ooda-codex-review` uses its own (separate) state root and does
//! not call this function.

use std::path::{Path, PathBuf};

/// Resolve the OODA PR-side state root.
///
/// See the module docs for the precedence chain.
#[must_use]
pub fn resolve_ooda_pr_state_root(explicit: Option<&Path>) -> PathBuf {
    if let Some(path) = explicit {
        return path.to_path_buf();
    }
    if let Some(path) = nonempty_env_path("OODA_PR_STATE_HOME") {
        return path;
    }
    if let Some(path) = nonempty_env_path("XDG_STATE_HOME") {
        return path.join("ooda-pr");
    }
    if let Some(home) = nonempty_env_path("HOME") {
        return home.join(".local").join("state").join("ooda-pr");
    }
    std::env::temp_dir().join("ooda-pr")
}

fn nonempty_env_path(name: &str) -> Option<PathBuf> {
    let value = std::env::var_os(name)?;
    if value.is_empty() {
        None
    } else {
        Some(PathBuf::from(value))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;
    use std::sync::Mutex;

    // Env mutation must serialize across tests in the same process;
    // tests in this module read/write OODA_PR_STATE_HOME / XDG_STATE_HOME /
    // HOME / TMPDIR concurrently otherwise.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    struct EnvGuard {
        saved: Vec<(&'static str, Option<OsString>)>,
    }

    impl EnvGuard {
        fn new(keys: &[&'static str]) -> Self {
            let saved = keys
                .iter()
                .map(|k| (*k, std::env::var_os(k)))
                .collect::<Vec<_>>();
            for k in keys {
                // SAFETY: serialized by ENV_LOCK in each test.
                unsafe {
                    std::env::remove_var(k);
                }
            }
            Self { saved }
        }
    }

    fn set_env(key: &str, value: &str) {
        // SAFETY: serialized by ENV_LOCK in each test.
        unsafe {
            std::env::set_var(key, value);
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            for (k, v) in &self.saved {
                // SAFETY: serialized by ENV_LOCK in each test.
                unsafe {
                    match v {
                        Some(val) => std::env::set_var(k, val),
                        None => std::env::remove_var(k),
                    }
                }
            }
        }
    }

    const ALL_KEYS: &[&str] = &["OODA_PR_STATE_HOME", "XDG_STATE_HOME", "HOME", "TMPDIR"];

    #[test]
    fn explicit_wins_over_all_env_vars() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _guard = EnvGuard::new(ALL_KEYS);
        set_env("OODA_PR_STATE_HOME", "/env/state");
        set_env("XDG_STATE_HOME", "/env/xdg");
        set_env("HOME", "/env/home");

        let explicit = PathBuf::from("/cli/state");
        assert_eq!(
            resolve_ooda_pr_state_root(Some(&explicit)),
            PathBuf::from("/cli/state"),
        );
    }

    #[test]
    fn ooda_pr_state_home_wins_over_xdg_and_home() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _guard = EnvGuard::new(ALL_KEYS);
        set_env("OODA_PR_STATE_HOME", "/from/env");
        set_env("XDG_STATE_HOME", "/from/xdg");
        set_env("HOME", "/from/home");

        assert_eq!(resolve_ooda_pr_state_root(None), PathBuf::from("/from/env"),);
    }

    #[test]
    fn xdg_state_home_appends_ooda_pr_subdir() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _guard = EnvGuard::new(ALL_KEYS);
        set_env("XDG_STATE_HOME", "/from/xdg");
        set_env("HOME", "/from/home");

        assert_eq!(
            resolve_ooda_pr_state_root(None),
            PathBuf::from("/from/xdg/ooda-pr"),
        );
    }

    #[test]
    fn home_appends_local_state_ooda_pr() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _guard = EnvGuard::new(ALL_KEYS);
        set_env("HOME", "/from/home");

        assert_eq!(
            resolve_ooda_pr_state_root(None),
            PathBuf::from("/from/home/.local/state/ooda-pr"),
        );
    }

    #[test]
    fn empty_env_var_is_treated_as_unset() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _guard = EnvGuard::new(ALL_KEYS);
        set_env("OODA_PR_STATE_HOME", "");
        set_env("XDG_STATE_HOME", "");
        set_env("HOME", "/from/home");

        assert_eq!(
            resolve_ooda_pr_state_root(None),
            PathBuf::from("/from/home/.local/state/ooda-pr"),
        );
    }

    #[test]
    fn falls_back_to_tmp_dir_when_no_env_vars() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _guard = EnvGuard::new(ALL_KEYS);
        // std::env::temp_dir() falls back to "/tmp" on Unix when
        // TMPDIR is unset, which is the platform contract we want
        // to test here.
        let resolved = resolve_ooda_pr_state_root(None);
        assert!(
            resolved.ends_with("ooda-pr"),
            "expected temp-dir fallback to end with ooda-pr; got {}",
            resolved.display(),
        );
    }
}
