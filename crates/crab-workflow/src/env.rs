//! Environment sanitization for stage execution.
//!
//! Produces the environment block a child process runs with, per the
//! stage's [`EnvSpec`]. Inherit copies the whole parent environment
//! (and omits it from the stage hash — see R7 warning path).
//! Allowlist copies only the named vars. Empty still injects the
//! minimum a reasonable child needs (`PATH`, `HOME`, `TMPDIR`) but
//! those minimums do NOT participate in the stage hash either.

use std::collections::HashMap;

use crate::stage::EnvSpec;

/// Variables injected into every sanitized env regardless of policy.
/// They are required for almost any process to run and never affect
/// the stage hash — users who care about hermeticity should declare
/// them explicitly via [`EnvSpec::Allowlist`].
const MINIMUM_VARS: &[&str] = &["PATH", "HOME", "TMPDIR"];

/// Produce the environment block a stage will run with, based on its
/// [`EnvSpec`].
pub fn sanitize(spec: &EnvSpec) -> HashMap<String, String> {
    match spec {
        EnvSpec::Inherit => std::env::vars().collect(),

        EnvSpec::Allowlist(names) => {
            let mut out = HashMap::new();
            // Minimum vars first so an allowlist entry with the same
            // name takes precedence — the user's declared value wins.
            inject_minimums(&mut out);
            for name in names {
                if let Ok(value) = std::env::var(name) {
                    out.insert(name.clone(), value);
                }
            }
            out
        }

        EnvSpec::Empty => {
            let mut out = HashMap::new();
            inject_minimums(&mut out);
            out
        }
    }
}

fn inject_minimums(out: &mut HashMap<String, String>) {
    for name in MINIMUM_VARS {
        if let Ok(value) = std::env::var(name) {
            out.insert((*name).to_owned(), value);
        }
    }
}

/// Return a human-readable label for an [`EnvSpec`] — used by
/// telemetry and by the first-run warn path for [`EnvSpec::Inherit`].
pub fn policy_label(spec: &EnvSpec) -> &'static str {
    match spec {
        EnvSpec::Inherit => "inherit",
        EnvSpec::Allowlist(_) => "allowlist",
        EnvSpec::Empty => "empty",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // `std::env` is process-global; serialize env-mutating tests on a
    // shared mutex so concurrent tests in the same binary can't race.
    use std::sync::Mutex;
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn with_env<F: FnOnce()>(vars: &[(&str, &str)], f: F) {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prior: Vec<_> = vars
            .iter()
            .map(|(k, _)| ((*k).to_owned(), std::env::var(k).ok()))
            .collect();
        // Safety: env mutation is single-threaded under ENV_LOCK.
        unsafe {
            for (k, v) in vars {
                std::env::set_var(k, v);
            }
        }
        f();
        unsafe {
            for (k, prev) in prior {
                match prev {
                    Some(v) => std::env::set_var(&k, v),
                    None => std::env::remove_var(&k),
                }
            }
        }
    }

    #[test]
    fn inherit_copies_parent_environment() {
        with_env(&[("CRAB_TEST_INHERIT", "yes")], || {
            let env = sanitize(&EnvSpec::Inherit);
            assert_eq!(
                env.get("CRAB_TEST_INHERIT").map(String::as_str),
                Some("yes"),
                "Inherit should copy the full parent env"
            );
        });
    }

    #[test]
    fn allowlist_copies_only_named_vars() {
        with_env(
            &[
                ("CRAB_TEST_ALLOW_KEEP", "keep"),
                ("CRAB_TEST_ALLOW_DROP", "drop"),
            ],
            || {
                let spec = EnvSpec::Allowlist(vec!["CRAB_TEST_ALLOW_KEEP".into()]);
                let env = sanitize(&spec);
                assert_eq!(
                    env.get("CRAB_TEST_ALLOW_KEEP").map(String::as_str),
                    Some("keep"),
                );
                assert!(
                    !env.contains_key("CRAB_TEST_ALLOW_DROP"),
                    "non-listed var leaked: {env:?}"
                );
            },
        );
    }

    #[test]
    fn allowlist_missing_var_is_silently_skipped() {
        // Not-set parent vars are ignored rather than raising. This
        // matches the design: missing env vars are a stage-execution
        // concern, surfaced by `StageEnvMissing` when the stage
        // actually needs them.
        with_env(&[], || {
            let spec = EnvSpec::Allowlist(vec!["CRAB_DEFINITELY_NOT_SET_XYZZY".into()]);
            let env = sanitize(&spec);
            assert!(!env.contains_key("CRAB_DEFINITELY_NOT_SET_XYZZY"));
        });
    }

    #[test]
    fn empty_injects_minimums_only() {
        with_env(
            &[
                ("PATH", "/usr/bin"),
                ("HOME", "/tmp/home"),
                ("CRAB_TEST_EMPTY_DROP", "drop"),
            ],
            || {
                let env = sanitize(&EnvSpec::Empty);
                assert_eq!(env.get("PATH").map(String::as_str), Some("/usr/bin"));
                assert_eq!(env.get("HOME").map(String::as_str), Some("/tmp/home"));
                assert!(!env.contains_key("CRAB_TEST_EMPTY_DROP"));
            },
        );
    }

    #[test]
    fn policy_labels() {
        assert_eq!(policy_label(&EnvSpec::Inherit), "inherit");
        assert_eq!(policy_label(&EnvSpec::Allowlist(vec![])), "allowlist");
        assert_eq!(policy_label(&EnvSpec::Empty), "empty");
    }
}
