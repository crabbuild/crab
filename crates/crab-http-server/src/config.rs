use openidconnect::IssuerUrl;
use std::collections::HashSet;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use url::Url;

use serde::Deserialize;

use crate::{Error, Result};

/// Server listener and the explicit catalog of object-storage repositories.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub listen: SocketAddr,
    pub repositories: Vec<RepositoryConfig>,
    pub auth: Option<OidcConfig>,
}

/// One public repository name mapped to an operator-owned storage location.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepositoryConfig {
    pub owner: String,
    pub name: String,
    pub bucket: String,
    pub prefix: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub members: Vec<RepositoryMember>,
    #[serde(default)]
    pub protected_branches: Vec<BranchProtection>,
}

/// An exact branch whose direct updates are disabled.
#[derive(Clone, Debug, Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct BranchProtection {
    pub branch: String,
    #[serde(default)]
    pub required_approvals: u8,
    #[serde(default)]
    pub required_checks: Vec<String>,
}

/// A provider subject's explicit repository permission.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepositoryMember {
    pub subject: String,
    pub name: String,
    pub access: RepositoryAccess,
}

/// Repository access, with write permission also allowing reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum RepositoryAccess {
    Read,
    Write,
}

impl Config {
    /// Read and validate configuration without loading or exposing credentials.
    pub fn read(path: &Path) -> Result<Self> {
        let config: Self = toml::from_str(&std::fs::read_to_string(path)?)?;
        config.validate()?;
        Ok(config)
    }

    pub(crate) fn validate(&self) -> Result<()> {
        if let Some(auth) = &self.auth {
            validate_identity_url(&auth.public_url, true)?;
            validate_identity_url(auth.issuer.url(), auth.public_url.scheme() == "http")?;
            if auth.public_url.path() != "/"
                || auth.public_url.query().is_some()
                || auth.issuer.url().query().is_some()
                || auth.client_id.is_empty()
            {
                return Err(Error::Config(
                    "OIDC requires a client ID, an issuer without query parameters, and a public URL without a path or query",
                ));
            }
            if auth.public_url.scheme() == "http" && !self.listen.ip().is_loopback() {
                return Err(Error::Config(
                    "HTTP identity development requires a loopback listener",
                ));
            }
        } else if !self.listen.ip().is_loopback() {
            return Err(Error::Config(
                "OIDC authentication is required beyond loopback",
            ));
        }
        if self.repositories.is_empty() {
            return Err(Error::Config("configure at least one repository"));
        }
        let mut names = HashSet::new();
        for repository in &self.repositories {
            let mut subjects = HashSet::new();
            let mut member_names = HashSet::new();
            for member in &repository.members {
                if member.subject.trim() != member.subject
                    || member.subject.is_empty()
                    || member.subject.chars().count() > 512
                    || member.subject.chars().any(char::is_control)
                    || !subjects.insert(&member.subject)
                    || member.name.trim() != member.name
                    || member.name.is_empty()
                    || member.name.chars().count() > 160
                    || member.name.chars().any(char::is_control)
                    || !member_names.insert(member.name.to_lowercase())
                {
                    return Err(Error::Config(
                        "repository members require unique OIDC subjects of at most 512 characters and unique names of at most 160 characters",
                    ));
                }
            }
            let mut protected = HashSet::new();
            for rule in &repository.protected_branches {
                let reference = format!("refs/heads/{}", rule.branch);
                let mut checks = HashSet::new();
                if rule.branch.is_empty()
                    || rule.branch.starts_with("refs/")
                    || crab_git::validate_push_refname(&reference).is_err()
                    || rule.required_approvals > 20
                    || rule.required_checks.len() > 50
                    || rule.required_checks.iter().any(|check| {
                        check.trim() != check
                            || check.is_empty()
                            || check.chars().count() > 100
                            || check.chars().any(char::is_control)
                            || !checks.insert(check.to_lowercase())
                    })
                    || !protected.insert(&rule.branch)
                {
                    return Err(Error::Config(
                        "protected branches require unique valid names, at most 20 approvals, and at most 50 unique check names",
                    ));
                }
            }
            if matches!(repository.owner.as_str(), "api" | "assets" | "auth" | "git") {
                return Err(Error::Config(
                    "repository owner conflicts with a server route",
                ));
            }
            for value in [&repository.owner, &repository.name] {
                if value.is_empty()
                    || value.len() > 100
                    || !value
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || b"-_.".contains(&byte))
                    || matches!(value.as_str(), "." | "..")
                {
                    return Err(Error::Config(
                        "repository owner and name must be URL-safe identifiers",
                    ));
                }
            }
            if repository.bucket.is_empty() || repository.prefix.is_empty() {
                return Err(Error::Config("repository bucket and prefix are required"));
            }
            if !names.insert((
                repository.owner.to_lowercase(),
                repository.name.to_lowercase(),
            )) {
                return Err(Error::Config(
                    "repository names must be unique ignoring case",
                ));
            }
        }
        Ok(())
    }
}

impl RepositoryConfig {
    pub(crate) fn protection(&self, reference: &str) -> Option<&BranchProtection> {
        let branch = reference.strip_prefix("refs/heads/")?;
        self.protected_branches
            .iter()
            .find(|rule| rule.branch == branch)
    }
}

/// Browser identity provider and the application's canonical external origin.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OidcConfig {
    pub issuer: IssuerUrl,
    pub client_id: String,
    pub public_url: Url,
    pub client_secret_file: Option<PathBuf>,
}

pub(crate) fn validate_identity_url(url: &Url, allow_loopback_http: bool) -> Result<()> {
    let loopback = match url.host() {
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        Some(url::Host::Domain("localhost")) => true,
        _ => false,
    };
    if url.host().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
        || !(url.scheme() == "https" || (allow_loopback_http && url.scheme() == "http" && loopback))
    {
        return Err(Error::Config(
            "identity URLs require HTTPS without credentials or fragments; development HTTP is loopback-only",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repository_members_require_an_explicit_known_permission() {
        for member in [
            "'alice'",
            "{subject='alice'}",
            "{subject='alice',name='Alice',access='admin'}",
            "{subject='alice',name='Alice',access='read',extra=true}",
        ] {
            let source = format!(
                "owner='team'\nname='project'\nbucket='bucket'\nprefix='project'\nmembers=[{member}]"
            );
            assert!(toml::from_str::<RepositoryConfig>(&source).is_err());
        }
    }

    #[test]
    fn repository_members_require_bounded_unique_subjects_and_names() {
        for members in [
            "[{subject='',name='Alice',access='read'}]".into(),
            "[{subject=' alice',name='Alice',access='read'}]".into(),
            format!(
                "[{{subject='{}',name='Alice',access='read'}}]",
                "a".repeat(513)
            ),
            "[{subject='alice',name='',access='read'}]".into(),
            "[{subject='alice',name=' Alice',access='read'}]".into(),
            format!(
                "[{{subject='alice',name='{}',access='read'}}]",
                "a".repeat(161)
            ),
            "[{subject='alice',name='Alice',access='read'},{subject='bob',name='alice',access='write'}]".into(),
        ] {
            let source = format!(
                "listen='127.0.0.1:8788'\n[[repositories]]\nowner='team'\nname='project'\nbucket='bucket'\nprefix='project'\nmembers={members}"
            );
            let config: Config = toml::from_str(&source).unwrap();
            assert!(config.validate().is_err(), "{members}");
        }
    }

    #[test]
    fn protected_branches_are_exact_valid_branch_names() {
        for branches in [
            "[{branch=''}]",
            "[{branch='refs/heads/main'}]",
            "[{branch='release..next'}]",
            "[{branch='main'},{branch='main'}]",
            "[{branch='main',required_approvals=21}]",
            "[{branch='main',required_checks=['']}]",
            "[{branch='main',required_checks=['ci/test','CI/Test']}]",
            "[{branch='main',required_checks=['ci/test', 'ci/test']} ]",
            "[{branch='main',unexpected=true}]",
        ] {
            let source = format!(
                "listen='127.0.0.1:8788'\n[[repositories]]\nowner='team'\nname='project'\nbucket='bucket'\nprefix='project'\nprotected_branches={branches}"
            );
            if let Ok(config) = toml::from_str::<Config>(&source) {
                assert!(config.validate().is_err(), "{branches}");
            }
        }
        let config: Config = toml::from_str(
            "listen='127.0.0.1:8788'\n[[repositories]]\nowner='team'\nname='project'\nbucket='bucket'\nprefix='project'\nprotected_branches=[{branch='main',required_approvals=2,required_checks=['ci/test']},{branch='release/v1'}]",
        )
        .unwrap();
        assert!(config.validate().is_ok());
        assert_eq!(
            config.repositories[0]
                .protection("refs/heads/main")
                .unwrap()
                .required_approvals,
            2
        );
        assert_eq!(
            config.repositories[0]
                .protection("refs/heads/main")
                .unwrap()
                .required_checks,
            ["ci/test"]
        );
        assert!(
            config.repositories[0]
                .protection("refs/heads/Main")
                .is_none()
        );
    }

    #[test]
    fn public_listeners_require_identity_and_https() {
        let base = "listen = '0.0.0.0:8788'\n[[repositories]]\nowner='team'\nname='project'\nbucket='bucket'\nprefix='project'\nmembers=[{subject='alice',name='Alice',access='read'}]\n";
        let identity = "\n[auth]\nissuer='https://identity.example/realm'\nclient_id='crab'\npublic_url='https://git.example'\n";
        let config: Config = toml::from_str(base).unwrap();
        assert!(config.validate().is_err());
        let config: Config = toml::from_str(&format!("{base}{identity}")).unwrap();
        assert!(config.validate().is_ok());
        for replacement in [
            "http://git.example",
            "http://127.0.0.1:8788",
            "https://git.example/path",
            "https://user:password@git.example",
            "https://git.example/#fragment",
        ] {
            let config: Config = toml::from_str(&format!(
                "{base}{}",
                identity.replace("https://git.example", replacement)
            ))
            .unwrap();
            assert!(config.validate().is_err(), "{replacement}");
        }
        let config: Config = toml::from_str(&format!(
            "{}{identity}",
            base.replace(
                "members=[{subject='alice',name='Alice',access='read'}]",
                "members=[{subject='alice',name='Alice',access='read'},{subject='alice',name='Alice 2',access='write'}]"
            )
        ))
        .unwrap();
        assert!(config.validate().is_err());
    }
}
