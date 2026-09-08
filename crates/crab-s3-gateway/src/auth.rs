use std::{collections::BTreeMap, sync::Arc};

use s3s::{
    S3Result,
    auth::{S3Auth, SecretKey},
};

use crate::{Config, CredentialConfig, Error, Result};

#[derive(Clone)]
pub(crate) struct GatewayAuth {
    keys: Arc<BTreeMap<String, Credential>>,
}

struct Credential {
    secret: SecretKey,
    principal: String,
}

impl GatewayAuth {
    pub(crate) fn load(config: &Config) -> Result<Self> {
        let keys = config
            .credentials
            .iter()
            .map(load_credential)
            .collect::<Result<BTreeMap<_, _>>>()?;
        Ok(Self {
            keys: Arc::new(keys),
        })
    }

    pub(crate) fn principal(&self, access_key: &str) -> Option<&str> {
        self.keys
            .get(access_key)
            .map(|credential| credential.principal.as_str())
    }
}

fn load_credential(config: &CredentialConfig) -> Result<(String, Credential)> {
    let value = std::fs::read_to_string(&config.secret_key_file)?;
    let secret = value.trim_end_matches(['\r', '\n']);
    if secret.len() < 16 || secret.len() > 256 || secret.chars().any(char::is_whitespace) {
        return Err(Error::Config(
            "credential secret files must contain one 16-256 character value",
        ));
    }
    Ok((
        config.access_key.clone(),
        Credential {
            secret: SecretKey::from(secret.to_owned()),
            principal: config.principal.clone(),
        },
    ))
}

#[async_trait::async_trait]
impl S3Auth for GatewayAuth {
    async fn get_secret_key(&self, access_key: &str) -> S3Result<SecretKey> {
        self.keys
            .get(access_key)
            .map(|credential| credential.secret.clone())
            .ok_or_else(|| s3s::s3_error!(InvalidAccessKeyId))
    }
}
