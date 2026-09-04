use std::collections::HashSet;
use std::net::SocketAddr;
use std::path::Path;

use serde::Deserialize;

use crate::{Error, Result};

/// Server listener and the explicit catalog of object-storage repositories.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub listen: SocketAddr,
    pub repositories: Vec<RepositoryConfig>,
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
}

impl Config {
    /// Read and validate configuration without loading or exposing credentials.
    pub fn read(path: &Path) -> Result<Self> {
        let config: Self = toml::from_str(&std::fs::read_to_string(path)?)?;
        config.validate()?;
        Ok(config)
    }

    pub(crate) fn validate(&self) -> Result<()> {
        if !self.listen.ip().is_loopback() {
            return Err(Error::Config(
                "the development server requires a loopback listener",
            ));
        }
        if self.repositories.is_empty() {
            return Err(Error::Config("configure at least one repository"));
        }
        let mut names = HashSet::new();
        for repository in &self.repositories {
            if matches!(repository.owner.as_str(), "api" | "assets") {
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
