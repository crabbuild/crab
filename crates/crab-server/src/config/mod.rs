use openidconnect::IssuerUrl;
use std::collections::HashSet;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use url::{Host, Url};

use serde::Deserialize;

use crate::{Error, Result};

/// Server listeners, object-storage root, and browser identity configuration.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub listen: SocketAddr,
    pub management_listen: SocketAddr,
    pub storage: StorageConfig,
    pub cells: CellsConfig,
    #[serde(default)]
    pub import: ImportConfig,
    pub auth: Option<OidcConfig>,
}

/// Outbound Git source policy for the browser import service.
///
/// Hosts are matched exactly after lower-casing. An explicit allowlist keeps
/// the server from becoming an unrestricted internal-network fetch proxy while
/// still allowing operators to import from GitHub, GitLab, Gitea, or a private
/// Git service.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImportConfig {
    #[serde(default = "default_import_allowed_hosts")]
    pub allowed_hosts: Vec<String>,
}

impl Default for ImportConfig {
    fn default() -> Self {
        Self {
            allowed_hosts: default_import_allowed_hosts(),
        }
    }
}

fn default_import_allowed_hosts() -> Vec<String> {
    vec!["github.com".to_owned()]
}

/// Provider-neutral object-storage root containing the repository catalog.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StorageConfig {
    pub url: String,
}

/// Local Cell storage and private fleet identity configuration.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CellsConfig {
    pub data_dir: PathBuf,
    pub local_disk_limit_bytes: u64,
    pub peer_advertise: Url,
    #[serde(default)]
    pub failure_zone: Option<String>,
    #[serde(default)]
    pub peer_tls_server_name: Option<String>,
    pub peer_certificate: PathBuf,
    pub peer_private_key: PathBuf,
    pub peer_ca: PathBuf,
    #[serde(default)]
    pub durability: CellDurabilityMode,
}

/// Proof required before the Cell runtime acknowledges a mutation.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum CellDurabilityMode {
    /// Use Cellar's native fleet durability policy.
    #[default]
    Fleet,
    /// Require Cellar object-store publication before acknowledgement.
    Object,
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
    pub groups: Vec<RepositoryGroup>,
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

/// An exact signed OIDC group identifier's repository permission.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct RepositoryGroup {
    pub group: String,
    pub access: RepositoryAccess,
}

/// Repository access, ordered from read-only through repository administration.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Deserialize, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum RepositoryAccess {
    Read,
    Triage,
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

    /// Replaces the node-specific host advertised by the Cell peer listener.
    pub fn set_peer_advertise_host(&mut self, host: &str) -> Result<()> {
        let ipv6 = host
            .parse::<std::net::Ipv6Addr>()
            .ok()
            .map(|address| format!("[{address}]"));
        self.cells
            .peer_advertise
            .set_host(Some(ipv6.as_deref().unwrap_or(host)))
            .map_err(|_| Error::Config("Cell peer advertise host is invalid"))?;
        self.validate()
    }

    /// Overrides the region advertised by the native Cellar node.
    pub fn set_cell_failure_zone(&mut self, zone: Option<&str>) -> Result<()> {
        if let Some(zone) = zone {
            self.cells.failure_zone = Some(zone.into());
        }
        self.validate()
    }

    pub(crate) fn validate(&self) -> Result<()> {
        if self.listen == self.management_listen {
            return Err(Error::Config(
                "public and management listeners must be different",
            ));
        }
        validate_cells(&self.cells, self.management_listen)?;
        validate_import(&self.import)?;
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
                    "browser authentication requires a client ID, an issuer without query parameters, and a public URL without a path or query",
                ));
            }
            if auth.provider == AuthProvider::GitHub {
                if auth.client_secret_file.is_none() {
                    return Err(Error::Config(
                        "GitHub OAuth requires auth.client_secret_file",
                    ));
                }
                let github = auth.github.clone().unwrap_or_default();
                for endpoint in [
                    github.authorize_url.as_str(),
                    github.token_url.as_str(),
                    github.api_url.as_str(),
                ] {
                    let endpoint = Url::parse(endpoint).map_err(|_| {
                        Error::Config("GitHub OAuth endpoints must be valid identity URLs")
                    })?;
                    validate_identity_url(&endpoint, auth.public_url.scheme() == "http")?;
                }
                let api_url = Url::parse(&github.api_url).map_err(|_| {
                    Error::Config("GitHub OAuth endpoints must be valid identity URLs")
                })?;
                if !api_url.path().ends_with('/') {
                    return Err(Error::Config("GitHub OAuth api_url must end with a slash"));
                }
            }
            if auth.public_url.scheme() == "http" && !self.listen.ip().is_loopback() {
                return Err(Error::Config(
                    "HTTP identity development requires a loopback listener",
                ));
            }
            if !self.listen.ip().is_loopback() && auth.state_key_file.is_none() {
                return Err(Error::Config(
                    "browser authentication beyond loopback requires auth.state_key_file",
                ));
            }
        } else if !self.listen.ip().is_loopback() {
            return Err(Error::Config(
                "browser authentication is required beyond loopback",
            ));
        }
        Ok(())
    }
}

fn validate_import(import: &ImportConfig) -> Result<()> {
    if import.allowed_hosts.is_empty() || import.allowed_hosts.len() > 128 {
        return Err(Error::Config(
            "import.allowed_hosts must contain between 1 and 128 hosts",
        ));
    }
    let mut seen = HashSet::new();
    for value in &import.allowed_hosts {
        let normalized = value.trim().to_ascii_lowercase();
        if normalized != *value
            || normalized.is_empty()
            || normalized.len() > 253
            || !seen.insert(normalized.clone())
        {
            return Err(Error::Config(
                "import.allowed_hosts must contain unique lower-case host names",
            ));
        }
        let host = Host::parse(&normalized)
            .map_err(|_| Error::Config("import.allowed_hosts contains an invalid host"))?;
        if matches!(host, Host::Domain(domain) if domain.is_empty()) {
            return Err(Error::Config(
                "import.allowed_hosts contains an invalid host",
            ));
        }
    }
    Ok(())
}

fn validate_cells(cells: &CellsConfig, management_listen: SocketAddr) -> Result<()> {
    if !cells.data_dir.is_absolute()
        || !cells.peer_certificate.is_absolute()
        || !cells.peer_private_key.is_absolute()
        || !cells.peer_ca.is_absolute()
    {
        return Err(Error::Config(
            "Cell data and peer identity paths must be absolute",
        ));
    }
    if cells.local_disk_limit_bytes == 0 {
        return Err(Error::Config(
            "cells.local_disk_limit_bytes must be greater than zero",
        ));
    }
    let endpoint = &cells.peer_advertise;
    if endpoint.scheme() != "https"
        || endpoint.host().is_none()
        || !endpoint.username().is_empty()
        || endpoint.password().is_some()
        || endpoint.path() != "/"
        || endpoint.query().is_some()
        || endpoint.fragment().is_some()
        || endpoint.port_or_known_default() != Some(management_listen.port())
        || endpoint.as_str().len() > 512
    {
        return Err(Error::Config(
            "cells.peer_advertise must be a root HTTPS URL on management_listen",
        ));
    }
    if let Some(name) = cells.peer_tls_server_name.as_deref()
        && (name.trim() != name
            || name.is_empty()
            || name.len() > 253
            || !matches!(Host::parse(name), Ok(Host::Domain(_))))
    {
        return Err(Error::Config(
            "cells.peer_tls_server_name must be a DNS name without a port",
        ));
    }
    if let Some(label) = cells.failure_zone.as_deref()
        && (label.is_empty()
            || label.len() > 256
            || label.trim() != label
            || label.chars().any(char::is_control))
    {
        return Err(Error::Config(
            "Cell region labels must be nonempty text of at most 256 bytes",
        ));
    }
    Ok(())
}

pub(crate) fn validate_repository(repository: &RepositoryConfig) -> Result<()> {
    let mut groups = HashSet::new();
    if repository.groups.len() > 100
        || repository
            .groups
            .iter()
            .any(|grant| !valid_group(&grant.group) || !groups.insert(&grant.group))
    {
        return Err(Error::Config(
            "repository groups require at most 100 unique exact OIDC group identifiers of at most 512 bytes",
        ));
    }
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
    if matches!(
        repository.owner.as_str(),
        "api" | "assets" | "auth" | "git" | "-"
    ) {
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

pub(crate) fn valid_group(group: &str) -> bool {
    !group.is_empty()
        && group.len() <= 512
        && group.trim() == group
        && !group.chars().any(char::is_control)
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
    #[serde(default)]
    pub provider: AuthProvider,
    pub issuer: IssuerUrl,
    pub client_id: String,
    pub public_url: Url,
    pub client_secret_file: Option<PathBuf>,
    pub state_key_file: Option<PathBuf>,
    #[serde(default)]
    pub github: Option<GitHubConfig>,
}

/// Browser authentication protocol selected for the configured identity client.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum AuthProvider {
    #[default]
    Oidc,
    #[serde(rename = "github")]
    GitHub,
}

/// GitHub OAuth endpoints. Defaults target github.com; loopback endpoints are
/// useful for deterministic integration tests and local provider proxies.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GitHubConfig {
    #[serde(default = "default_github_authorize_url")]
    pub authorize_url: String,
    #[serde(default = "default_github_token_url")]
    pub token_url: String,
    #[serde(default = "default_github_api_url")]
    pub api_url: String,
}

impl Default for GitHubConfig {
    fn default() -> Self {
        Self {
            authorize_url: default_github_authorize_url(),
            token_url: default_github_token_url(),
            api_url: default_github_api_url(),
        }
    }
}

fn default_github_authorize_url() -> String {
    "https://github.com/login/oauth/authorize".to_owned()
}

fn default_github_token_url() -> String {
    "https://github.com/login/oauth/access_token".to_owned()
}

fn default_github_api_url() -> String {
    "https://api.github.com/".to_owned()
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
mod tests;
