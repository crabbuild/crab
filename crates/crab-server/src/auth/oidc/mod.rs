use std::collections::BTreeMap;

use openidconnect::{
    EmptyExtraTokenFields, EndpointMaybeSet, EndpointNotSet, EndpointSet, IdTokenFields,
    StandardErrorResponse, StandardTokenResponse, core::*,
};
use serde::{Deserialize, Serialize};

use super::AuthError;

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub(super) struct Claims {
    #[serde(default)]
    groups: Vec<String>,
    #[serde(default)]
    hasgroups: bool,
    #[serde(default, rename = "_claim_names")]
    claim_names: BTreeMap<String, String>,
}
impl openidconnect::AdditionalClaims for Claims {}

impl Claims {
    pub(super) fn groups(&self) -> Result<Vec<String>, AuthError> {
        // Distributed/overage claims are not a complete membership set. Never
        // follow a token-supplied URL or grant access from a partial group list.
        if self.hasgroups || self.claim_names.contains_key("groups") {
            return Ok(Vec::new());
        }
        groups(self.groups.clone())
    }
}

pub(super) fn groups(mut groups: Vec<String>) -> Result<Vec<String>, AuthError> {
    if groups.len() > 256
        || groups.iter().map(String::len).sum::<usize>() > 32 * 1024
        || groups
            .iter()
            .any(|group| !crate::config::valid_group(group))
    {
        return Err(AuthError::Invalid);
    }
    groups.sort_unstable();
    groups.dedup();
    Ok(groups)
}

type TokenResponse = StandardTokenResponse<
    IdTokenFields<
        Claims,
        EmptyExtraTokenFields,
        CoreGenderClaim,
        CoreJweContentEncryptionAlgorithm,
        CoreJwsSigningAlgorithm,
    >,
    CoreTokenType,
>;

pub(super) type Client = openidconnect::Client<
    Claims,
    CoreAuthDisplay,
    CoreGenderClaim,
    CoreJweContentEncryptionAlgorithm,
    CoreJsonWebKey,
    CoreAuthPrompt,
    StandardErrorResponse<CoreErrorResponseType>,
    TokenResponse,
    CoreTokenIntrospectionResponse,
    CoreRevocableToken,
    CoreRevocationErrorResponse,
    EndpointSet,
    EndpointNotSet,
    EndpointNotSet,
    EndpointNotSet,
    EndpointMaybeSet,
    EndpointMaybeSet,
>;

#[cfg(test)]
mod tests;
