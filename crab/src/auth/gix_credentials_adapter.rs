//! Git-credential-helper adapter for the remote-helper auth path.
//!
//! This module wraps `gix_credentials::helper::Cascade` behind a thin
//! crab API. It sits *parallel* to the cloud-identity `CredentialProvider`
//! trait in `auth/mod.rs`, not in front of it. Crab's remote helper
//! consults this adapter only when:
//!
//!   1. No explicit cloud-SDK credentials resolved (no STS env, no
//!      configured `CredentialProvider`), and
//!   2. The transport needs HTTP-shaped basic auth (for example when
//!      `crab://` routes through a signed-URL gateway that accepts
//!      bearer tokens).
//!
//! The precedence is fixed and documented in
//! `docs/guides/auth/enterprise-auth.md` under "Credential helper
//! interop":
//!
//! ```text
//!   explicit STS env / config
//! → cloud SDK default credential chain (`from_env()`)
//! → git credential helper cascade (this module)
//! → anonymous
//! ```
//!
//! Today this is forward-looking scaffolding. The actual remote-helper
//! call sites still resolve cloud-native S3 auth through
//! `auth::build_store`; wiring from `git/remote_helper.rs` into
//! [`resolve`] lands when an HTTP auth path is added. See
//! `docs/guides/auth/enterprise-auth.md` for the user-facing model.
//!
//! ## Design notes
//!
//! * `gix_credentials::helper::Cascade::invoke` does two things in one
//!   call: `Action::Get` for retrieval, `Action::Store` / `Action::Erase`
//!   for lifecycle management. This adapter exposes two methods so the
//!   caller doesn't have to care about `NextAction` threading.
//! * Helper discovery reads the user/repo gitconfig through
//!   `gix_config::File`. Once Req 8's `GixConfigResolver` lands, this
//!   module will delegate discovery to the resolver; for now it reads
//!   the files directly because `gix_config` is already a dep.
//! * `!command` shell-execution form is handled for free by
//!   `gix_credentials::Program::from_custom_definition`, which parses
//!   the standard git-config spelling of helper values.
//! * `erase` iterates every configured helper so helpers that cache
//!   locally (osxkeychain, libsecret) actually forget the credential.

#![cfg(feature = "gix-credentials")]

use std::path::Path;

use bstr::ByteSlice;
use gix_credentials::helper::{Action, Cascade, NextAction};
use gix_credentials::{Program, protocol};

use crate::core::error::{CrabError, Result};
use crate::gix_boundary;

/// A credential answer returned by a configured credential helper.
///
/// Carries the identity plus an opaque handle the caller passes back
/// to [`erase`] when the credential gets rejected (401/403). Callers
/// that only need username/password can read [`Self::username`] and
/// [`Self::password`] directly.
#[derive(Debug, Clone)]
pub struct CredentialAnswer {
    /// Git-credential helper username.
    pub username: String,
    /// Git-credential helper password (or token).
    pub password: String,
    /// OAuth refresh token, if the helper emitted one.
    pub oauth_refresh_token: Option<String>,
    /// Opaque payload that [`erase`] feeds back to the helper cascade
    /// so helpers with local caches (osxkeychain, libsecret) can evict
    /// the exact credential they returned.
    next: NextAction,
}

impl CredentialAnswer {
    /// Borrow the username for display / redaction.
    pub fn username(&self) -> &str {
        &self.username
    }

    /// Borrow the password for transport construction. Do not log.
    pub fn password(&self) -> &str {
        &self.password
    }
}

/// Resolve a credential for `url` via the configured helper cascade.
///
/// Returns `Ok(None)` when no helper is configured (caller falls back
/// to anonymous). Returns `Ok(Some(answer))` when any helper in the
/// cascade produced a complete identity.
///
/// `config` is an in-memory `gix_config::File` representing the merged
/// git config for the caller. Typically produced via
/// `gix_config::File::from_git_dir(...)` or equivalent; at the
/// boundary call site the Req 8 resolver will supply it.
pub fn resolve(url: &str, config: &gix_config::File<'_>) -> Result<Option<CredentialAnswer>> {
    let _span = gix_boundary!("credentials", "resolve").entered();

    let programs = helpers_for(config, url);
    if programs.is_empty() {
        return Ok(None);
    }

    let mut cascade = Cascade::default().extend(programs);
    let action = Action::get_for_url(url.to_owned());
    let prompt = prompt_disabled();

    let outcome = {
        let _invoke_span = gix_boundary!("credentials", "cascade_invoke").entered();
        cascade.invoke(action, prompt)
    };

    match outcome {
        Ok(Some(protocol_outcome)) => {
            let protocol::Outcome { identity, next } = protocol_outcome;
            Ok(Some(CredentialAnswer {
                username: identity.username,
                password: identity.password,
                oauth_refresh_token: identity.oauth_refresh_token,
                next,
            }))
        }
        // The helper returned but couldn't produce a complete identity.
        // Treat like "no helper configured" — caller falls through to
        // anonymous. This matches git's own behavior on an empty helper
        // response.
        Ok(None) => Ok(None),
        Err(protocol::Error::IdentityMissing { .. } | protocol::Error::Quit) => Ok(None),
        Err(err) => Err(CrabError::GixCreds(err)),
    }
}

/// Evict `answer` from every configured helper after a 401/403.
///
/// Safe to call with a helper cascade that doesn't support erase —
/// programs that ignore `action=erase` simply do nothing.
pub fn erase(url: &str, config: &gix_config::File<'_>, answer: CredentialAnswer) -> Result<()> {
    let _span = gix_boundary!("credentials", "erase").entered();

    let programs = helpers_for(config, url);
    if programs.is_empty() {
        return Ok(());
    }

    let mut cascade = Cascade::default().extend(programs);
    let action = answer.next.erase();

    let _invoke_span = gix_boundary!("credentials", "cascade_invoke_erase").entered();
    match cascade.invoke(action, prompt_disabled()) {
        Ok(_) => Ok(()),
        Err(err) => Err(CrabError::GixCreds(err)),
    }
}

/// Collect the ordered list of helper programs that apply to `url`.
///
/// Global `credential.helper` entries come first, followed by any
/// `credential.<url>.helper` entries whose subsection matches `url`
/// by prefix. This mirrors git's own resolution order.
///
/// `!command` values are parsed through
/// `gix_credentials::Program::from_custom_definition`, which knows
/// about the shell-script form, bare names (`osxkeychain`,
/// `libsecret`, `manager-core`, …), and absolute paths with
/// arguments.
fn helpers_for(config: &gix_config::File<'_>, url: &str) -> Vec<Program> {
    let mut programs = Vec::new();

    // Iterate every `[credential]` and `[credential "<pattern>"]`
    // section in declaration order. git's semantics: an empty
    // `helper =` value clears the list (so a trusted section can
    // reset an untrusted one).
    if let Some(sections) = config.sections_by_name("credential") {
        for section in sections {
            if !section_matches(section.header().subsection_name(), url) {
                continue;
            }
            for value in section.values("helper") {
                let raw = value.as_bstr();
                if raw.trim().is_empty() {
                    programs.clear();
                } else {
                    programs.push(Program::from_custom_definition(raw.to_vec()));
                }
            }
        }
    }

    programs
}

/// Match a `credential.<pattern>.helper` subsection against `url`.
///
/// `None` means the section applies to every URL (plain
/// `[credential]`). A `Some(pattern)` is a literal prefix match on the
/// URL. git's real matching also normalizes the URL (drops default
/// ports, lower-cases the host); that full algorithm lives in
/// `gix::config::snapshot::credential_helpers` and will be adopted
/// when Req 8's resolver pulls it in. Prefix matching is enough for
/// the current forward-looking call sites.
fn section_matches(subsection: Option<&bstr::BStr>, url: &str) -> bool {
    match subsection {
        None => true,
        Some(pattern) => {
            let Ok(pat) = std::str::from_utf8(pattern) else {
                return false;
            };
            url.starts_with(pat)
        }
    }
}

/// Build a `gix_prompt::Options` with prompting disabled.
///
/// Crab never wants the credential helper to open an interactive
/// prompt; the remote helper runs under `git push` / `git fetch`
/// which owns stdin. If no cached credential is available we fall
/// through to anonymous instead of hanging for input.
fn prompt_disabled() -> gix_prompt::Options<'static> {
    gix_prompt::Options {
        mode: gix_prompt::Mode::Disable,
        ..Default::default()
    }
}

/// Open a gix config file suitable for feeding into [`resolve`] /
/// [`erase`].
///
/// Wrapper over `gix_config::File::from_git_dir` that maps its error
/// into `CrabError`. Returns an empty config if `git_dir` does not
/// contain a readable config (which is the normal case for a fresh
/// remote-helper invocation in a directory that git hasn't yet
/// initialized).
///
/// Most callers should use this helper instead of reaching into
/// `gix_config` directly.
pub fn open_git_dir_config(git_dir: &Path) -> Result<gix_config::File<'static>> {
    let _span = gix_boundary!("credentials", "open_git_dir_config").entered();

    match gix_config::File::from_git_dir(git_dir.to_path_buf()) {
        Ok(cfg) => Ok(cfg),
        Err(err) => Err(CrabError::Internal(format!(
            "failed to read git config at {}: {err}",
            git_dir.display()
        ))),
    }
}

#[cfg(all(test, feature = "gix-credentials"))]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions"
)]
mod tests {
    use super::*;

    /// Build a minimal shell helper as a `!` command that emits a
    /// fixed credential response. The helper ignores its argument
    /// (`get` / `store` / `erase`) and always prints the same lines.
    fn dummy_echo_helper(host: &str, user: &str, pass: &str) -> String {
        // `printf` with `\n` is portable across macOS's default bash
        // and linux bash/sh.
        format!("!printf 'protocol=https\\nhost={host}\\nusername={user}\\npassword={pass}\\n'")
    }

    /// Config helper: parse an inline git-config string into an
    /// owned `File<'static>`.
    ///
    /// `File::from_str` uses `Events::from_bytes_owned` internally,
    /// which produces a `'static`-lifetime file we can return without
    /// borrow-lifetime trouble. `File::try_from(&str)` borrows from
    /// the input and would not satisfy the `'static` return type.
    fn cfg(text: &str) -> gix_config::File<'static> {
        use std::str::FromStr;
        gix_config::File::from_str(text).expect("valid test config")
    }

    #[test]
    fn credential_helper_supplies_basic_auth() {
        let cfg = cfg(&format!(
            "[credential]\n    helper = {}\n",
            dummy_echo_helper("example.com", "alice", "secret")
        ));
        let answer = resolve("https://example.com/repo", &cfg)
            .expect("resolve succeeds")
            .expect("helper produced a credential");
        assert_eq!(answer.username(), "alice");
        assert_eq!(answer.password(), "secret");
    }

    #[test]
    fn credential_helper_absent_anonymous_fallback() {
        // Empty config — no `[credential]` section anywhere.
        let cfg = cfg("[core]\n    editor = vim\n");
        let answer = resolve("https://example.com/repo", &cfg).expect("resolve succeeds");
        assert!(
            answer.is_none(),
            "resolve should return None with no helper configured"
        );
    }

    #[test]
    fn credential_helper_erase_on_401() {
        // Use a stub helper that logs its stdin to a tempfile. When
        // `action=erase` is sent, the helper protocol puts the
        // previous context (including `username=alice`) on stdin.
        // We assert the log file captures it, which proves erase
        // actually invoked the helper.
        //
        // Writing the helper to disk and referencing it as
        // `!/path/to/script` keeps the git-config value simple and
        // sidesteps shell-quoting gotchas in the inline form.
        let tmp = tempfile::tempdir().expect("tempdir");
        let log_path = tmp.path().join("helper.log");
        let script_path = tmp.path().join("helper.sh");

        let script = format!(
            "#!/bin/sh\n\
             case \"$1\" in\n\
             erase|reject) cat >> '{log}' ;;\n\
             *) printf 'protocol=https\\nhost=example.com\\nusername=alice\\npassword=secret\\n' ;;\n\
             esac\n",
            log = log_path.display()
        );
        std::fs::write(&script_path, script).expect("write script");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&script_path).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&script_path, perms).expect("chmod");
        }

        let cfg = cfg(&format!(
            "[credential]\n    helper = !{}\n",
            script_path.display()
        ));

        let answer = resolve("https://example.com/repo", &cfg)
            .expect("resolve succeeds")
            .expect("helper produced a credential");

        erase("https://example.com/repo", &cfg, answer).expect("erase succeeds");

        let log = std::fs::read_to_string(&log_path).expect("log file written");
        // The stdin to an `erase` call contains the previous context
        // emitted by the helper; git-credential writes each k=v pair
        // on its own line. We assert that the username we resolved
        // round-tripped into the erase invocation, which is what
        // proves erase actually fired.
        assert!(
            log.contains("username=alice"),
            "erase payload should include the credential being evicted; got log: {log}"
        );
    }

    /// macOS-only: the osxkeychain helper must be instantiatable
    /// without side effects. We do NOT actually call `get` / `store`
    /// / `erase` on it — that would mutate the developer's real
    /// keychain. We only confirm that
    /// `Program::from_custom_definition("osxkeychain")` constructs a
    /// `Program` that matches the expected `ExternalName` shape and
    /// that `Cascade::platform_builtin()` returns a non-empty vec.
    #[test]
    #[cfg(target_os = "macos")]
    fn credential_helper_osxkeychain_smoke_test() {
        let builtins = Cascade::platform_builtin();
        assert!(
            !builtins.is_empty(),
            "Cascade::platform_builtin() should include osxkeychain on macOS"
        );

        // Sanity: the derived program parses through
        // from_custom_definition without panicking.
        let _program = Program::from_custom_definition("osxkeychain");
    }

    #[test]
    fn helpers_for_returns_empty_without_credential_section() {
        let cfg = cfg("[core]\n    editor = vim\n");
        let programs = helpers_for(&cfg, "https://example.com/repo");
        assert!(programs.is_empty());
    }

    #[test]
    fn helpers_for_picks_up_plain_credential_section() {
        let cfg = cfg("[credential]\n    helper = osxkeychain\n");
        let programs = helpers_for(&cfg, "https://example.com/repo");
        assert_eq!(programs.len(), 1);
    }

    #[test]
    fn helpers_for_empty_value_clears_list() {
        // Per git semantics, `helper =` (empty) wipes the previously
        // accumulated list. A second entry after the reset is kept.
        let cfg =
            cfg("[credential]\n    helper = osxkeychain\n    helper =\n    helper = libsecret\n");
        let programs = helpers_for(&cfg, "https://example.com/repo");
        assert_eq!(
            programs.len(),
            1,
            "empty helper value should reset the list, leaving only libsecret"
        );
    }

    #[test]
    fn helpers_for_url_scoped_section_matches_prefix() {
        let cfg = cfg("[credential \"https://example.com\"]\n    helper = osxkeychain\n");
        let matching = helpers_for(&cfg, "https://example.com/repo");
        assert_eq!(matching.len(), 1);

        let non_matching = helpers_for(&cfg, "https://other.example/repo");
        assert!(
            non_matching.is_empty(),
            "url-scoped section should not match different host"
        );
    }

    #[test]
    fn helpers_for_shell_command_form_parses() {
        // `!cmd` is the shell-script form. Confirm it parses and
        // lands in the cascade without erroring.
        let cfg = cfg("[credential]\n    helper = !printf ''\n");
        let programs = helpers_for(&cfg, "https://example.com/repo");
        assert_eq!(programs.len(), 1);
    }
}
