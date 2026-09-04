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
    pub protected_branches: Vec<String>,
}

/// A provider subject's explicit repository permission.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepositoryMember {
    pub subject: String,
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
            for member in &repository.members {
                if member.subject.is_empty() || !subjects.insert(&member.subject) {
                    return Err(Error::Config(
                        "repository members must be unique nonempty OIDC subjects",
                    ));
                }
            }
            let mut protected = HashSet::new();
            for branch in &repository.protected_branches {
                let reference = format!("refs/heads/{branch}");
                if branch.is_empty()
                    || branch.starts_with("refs/")
                    || crab_git::validate_push_refname(&reference).is_err()
                    || !protected.insert(branch)
                {
                    return Err(Error::Config(
                        "protected branches must be unique valid branch names without a refs/heads prefix",
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
    pub(crate) fn protects(&self, reference: &str) -> bool {
        reference
            .strip_prefix("refs/heads/")
            .is_some_and(|branch| self.protected_branches.iter().any(|value| value == branch))
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
            "{subject='alice',access='admin'}",
            "{subject='alice',access='read',extra=true}",
        ] {
            let source = format!(
                "owner='team'\nname='project'\nbucket='bucket'\nprefix='project'\nmembers=[{member}]"
            );
            assert!(toml::from_str::<RepositoryConfig>(&source).is_err());
        }
    }

    #[test]
    fn protected_branches_are_exact_valid_branch_names() {
        for branches in [
            "['']",
            "['refs/heads/main']",
            "['release..next']",
            "['main','main']",
        ] {
            let source = format!(
                "listen='127.0.0.1:8788'\n[[repositories]]\nowner='team'\nname='project'\nbucket='bucket'\nprefix='project'\nprotected_branches={branches}"
            );
            let config: Config = toml::from_str(&source).unwrap();
            assert!(config.validate().is_err(), "{branches}");
        }
        let config: Config = toml::from_str(
            "listen='127.0.0.1:8788'\n[[repositories]]\nowner='team'\nname='project'\nbucket='bucket'\nprefix='project'\nprotected_branches=['main','release/v1']",
        )
        .unwrap();
        assert!(config.validate().is_ok());
        assert!(config.repositories[0].protects("refs/heads/main"));
        assert!(!config.repositories[0].protects("refs/heads/Main"));
    }

    #[test]
    fn public_listeners_require_identity_and_https() {
        let base = "listen = '0.0.0.0:8788'\n[[repositories]]\nowner='team'\nname='project'\nbucket='bucket'\nprefix='project'\nmembers=[{subject='alice',access='read'}]\n";
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
                "members=[{subject='alice',access='read'}]",
                "members=[{subject='alice',access='read'},{subject='alice',access='write'}]"
            )
        ))
        .unwrap();
        assert!(config.validate().is_err());
    }
}
