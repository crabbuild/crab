use openidconnect::IssuerUrl;
use std::collections::HashSet;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use url::Url;

use serde::Deserialize;

use crate::{Error, Result};

/// Server listeners, object-storage root, and browser identity configuration.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub listen: SocketAddr,
    pub management_listen: SocketAddr,
    pub storage: StorageConfig,
    pub auth: Option<OidcConfig>,
}

/// Provider-neutral object-storage root containing the repository catalog.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StorageConfig {
    pub url: String,
}

/// One resolved repository from the durable application catalog.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct RepositoryConfig {
    pub owner: String,
    pub name: String,
    pub bucket: String,
    pub prefix: String,
    #[serde(default = "default_repository_branch")]
    pub default_branch: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub members: Vec<RepositoryMember>,
    #[serde(default)]
    pub protected_branches: Vec<BranchProtection>,
}

/// An exact branch whose direct updates are disabled.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct BranchProtection {
    pub branch: String,
    #[serde(default)]
    pub required_approvals: u8,
    #[serde(default)]
    pub required_checks: Vec<String>,
}

/// A provider subject's explicit repository permission.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct RepositoryMember {
    pub subject: String,
    pub name: String,
    pub access: RepositoryAccess,
}

/// Repository access, ordered from read-only through repository administration.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Deserialize, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum RepositoryAccess {
    Read,
    Write,
    Admin,
}

impl Config {
    /// Read and validate configuration without loading or exposing credentials.
    pub fn read(path: &Path) -> Result<Self> {
        let config: Self = toml::from_str(&std::fs::read_to_string(path)?)?;
        config.validate()?;
        Ok(config)
    }

    pub(crate) fn validate(&self) -> Result<()> {
        if self.listen == self.management_listen {
            return Err(Error::Config(
                "public and management listeners must be different",
            ));
        }
        let storage = crab_git::url::ObjectUrl::parse(&self.storage.url)
            .map_err(|_| Error::Config("storage.url must be a valid raw cloud URL"))?;
        if storage.form != crab_git::url::UrlForm::Raw
            || storage.cloud == crab_git::url::Cloud::Local
            || storage.prefix.is_empty()
        {
            return Err(Error::Config(
                "storage.url must use s3://, gs://, or az:// with a nonempty root prefix",
            ));
        }
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
            if !self.listen.ip().is_loopback() && auth.state_key_file.is_none() {
                return Err(Error::Config(
                    "OIDC deployments beyond loopback require auth.state_key_file",
                ));
            }
        } else if !self.listen.ip().is_loopback() {
            return Err(Error::Config(
                "OIDC authentication is required beyond loopback",
            ));
        }
        Ok(())
    }
}

pub(crate) fn validate_repository(repository: &RepositoryConfig) -> Result<()> {
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
    if !valid_branch_protections(&repository.protected_branches) {
        return Err(Error::Config(
            "protected branches require at most 100 unique valid names, at most 20 approvals, and at most 50 unique check names",
        ));
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
    let default_ref = format!("refs/heads/{}", repository.default_branch);
    if repository.default_branch.starts_with("refs/")
        || crab_git::validate_push_refname(&default_ref).is_err()
    {
        return Err(Error::Config(
            "repository default_branch must be a valid short branch name",
        ));
    }
    Ok(())
}

pub(crate) fn valid_branch_protections(rules: &[BranchProtection]) -> bool {
    if rules.len() > 100 {
        return false;
    }
    let mut protected = HashSet::new();
    rules.iter().all(|rule| {
        let reference = format!("refs/heads/{}", rule.branch);
        let mut checks = HashSet::new();
        !rule.branch.is_empty()
            && !rule.branch.starts_with("refs/")
            && crab_git::validate_push_refname(&reference).is_ok()
            && rule.required_approvals <= 20
            && rule.required_checks.len() <= 50
            && rule.required_checks.iter().all(|check| {
                check.trim() == check
                    && !check.is_empty()
                    && check.chars().count() <= 100
                    && !check.chars().any(char::is_control)
                    && checks.insert(check.to_lowercase())
            })
            && protected.insert(&rule.branch)
    })
}

fn default_repository_branch() -> String {
    "main".to_owned()
}

/// Browser identity provider and the application's canonical external origin.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OidcConfig {
    pub issuer: IssuerUrl,
    pub client_id: String,
    pub public_url: Url,
    pub client_secret_file: Option<PathBuf>,
    pub state_key_file: Option<PathBuf>,
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

    fn local_config(storage_url: &str) -> Config {
        toml::from_str(&format!(
            "listen='127.0.0.1:8788'\nmanagement_listen='127.0.0.1:8789'\n[storage]\nurl='{storage_url}'"
        ))
        .unwrap()
    }

    fn repository() -> RepositoryConfig {
        toml::from_str("owner='team'\nname='project'\nbucket='bucket'\nprefix='project'").unwrap()
    }

    #[test]
    fn repository_members_require_an_explicit_known_permission() {
        for member in [
            "'alice'",
            "{subject='alice'}",
            "{subject='alice',name='Alice',access='owner'}",
            "{subject='alice',name='Alice',access='read',extra=true}",
        ] {
            let source = format!(
                "owner='team'\nname='project'\nbucket='bucket'\nprefix='project'\nmembers=[{member}]"
            );
            assert!(toml::from_str::<RepositoryConfig>(&source).is_err());
        }
        let administrator: RepositoryConfig = toml::from_str(
            "owner='team'\nname='project'\nbucket='bucket'\nprefix='project'\nmembers=[{subject='alice',name='Alice',access='admin'}]",
        )
        .unwrap();
        assert_eq!(administrator.members[0].access, RepositoryAccess::Admin);
    }

    #[test]
    fn repository_default_branch_is_short_valid_and_defaults_to_main() {
        let repository_config = repository();
        assert_eq!(repository_config.default_branch, "main");

        for branch in ["", "refs/heads/main", "release..next"] {
            let mut repository = repository();
            repository.default_branch = branch.into();
            assert!(validate_repository(&repository).is_err(), "{branch}");
        }

        let mut repository = repository();
        repository.default_branch = "trunk".into();
        assert!(validate_repository(&repository).is_ok());
    }

    #[test]
    fn storage_root_accepts_each_cloud_provider() {
        for url in [
            "s3://bucket/repositories",
            "gs://bucket/repositories",
            "az://account/container/repositories",
        ] {
            assert!(local_config(url).validate().is_ok(), "{url}");
        }
        for url in [
            "s3://bucket",
            "file:///repositories",
            "https://example.com/repositories",
        ] {
            assert!(local_config(url).validate().is_err(), "{url}");
        }
    }

    #[test]
    fn management_listener_is_separate() {
        let mut config = local_config("s3://bucket/repositories");
        config.management_listen = config.listen;
        assert!(config.validate().is_err());
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
            let mut repository = repository();
            repository.members = toml::from_str::<RepositoryConfig>(&format!(
                "owner='team'\nname='project'\nbucket='bucket'\nprefix='project'\nmembers={members}"
            ))
            .unwrap()
            .members;
            assert!(validate_repository(&repository).is_err(), "{members}");
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
                "owner='team'\nname='project'\nbucket='bucket'\nprefix='project'\nprotected_branches={branches}"
            );
            if let Ok(repository) = toml::from_str::<RepositoryConfig>(&source) {
                assert!(validate_repository(&repository).is_err(), "{branches}");
            }
        }
        let repository: RepositoryConfig = toml::from_str(
            "owner='team'\nname='project'\nbucket='bucket'\nprefix='project'\nprotected_branches=[{branch='main',required_approvals=2,required_checks=['ci/test']},{branch='release/v1'}]",
        )
        .unwrap();
        assert!(validate_repository(&repository).is_ok());
        let main = repository
            .protected_branches
            .iter()
            .find(|rule| rule.branch == "main")
            .unwrap();
        assert_eq!(main.required_approvals, 2);
        assert_eq!(main.required_checks, ["ci/test"]);
        assert!(
            !repository
                .protected_branches
                .iter()
                .any(|rule| rule.branch == "Main")
        );
    }

    #[test]
    fn public_listeners_require_identity_and_https() {
        let base = "listen='0.0.0.0:8788'\nmanagement_listen='0.0.0.0:8789'\n[storage]\nurl='s3://bucket/repositories'\n";
        let identity = "\n[auth]\nissuer='https://identity.example/realm'\nclient_id='crab'\npublic_url='https://git.example'\nstate_key_file='/run/secrets/crab/state-key'\n";
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
    }
}
