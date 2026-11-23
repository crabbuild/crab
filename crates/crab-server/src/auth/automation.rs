use uuid::Uuid;

use super::{Identity, Principal, key};
use crate::{
    app::{self, Error, Result},
    cells::{Address, organization::automation as stored},
    server::Server,
};

const PREFIX: &str = "crab_automation_";

#[derive(Clone)]
pub(crate) struct Credential {
    organization: Uuid,
    proof: stored::Proof,
    verified: Identity,
    expires_at: u64,
}

impl Credential {
    pub(super) fn active(&self) -> bool {
        super::now_epoch().is_ok_and(|now| now < self.expires_at)
    }

    pub(super) fn identity(&self) -> Identity {
        self.verified.clone()
    }

    pub(crate) async fn authorize(
        &self,
        server: &Server,
        principal: &Principal,
        organization: Option<Uuid>,
        repository: Uuid,
        ownership_epoch: u32,
    ) -> Result<Principal> {
        if organization != Some(self.organization) {
            return Err(Error::NotFound);
        }
        let value = verification(
            server,
            self.organization,
            self.proof.clone(),
            Some((repository, ownership_epoch)),
        )
        .await?
        .ok_or(Error::NotFound)?;
        let access = value.access.ok_or(Error::NotFound)?;
        Ok(principal.with_automation_access(repository, access.into()))
    }
}

pub(crate) fn issue(organization: Uuid, identity: Uuid, token: Uuid) -> (String, [u8; 32]) {
    let secret = blake3::Hash::from_bytes(rand::random());
    let value = format!("{PREFIX}{organization}.{identity}.{token}.{secret}");
    let digest = key(&value);
    (value, digest)
}

pub(super) fn recognizes(password: &str) -> bool {
    password.starts_with(PREFIX)
}

pub(super) async fn verify(server: &Server, password: &str) -> Option<Principal> {
    let tail = password.strip_prefix(PREFIX)?;
    let (organization, tail) = tail.split_once('.')?;
    let (identity, tail) = tail.split_once('.')?;
    let (token, secret) = tail.split_once('.')?;
    let organization = Uuid::parse_str(organization).ok()?;
    let identity = Uuid::parse_str(identity).ok()?;
    let token = Uuid::parse_str(token).ok()?;
    if organization.is_nil() || identity.is_nil() || token.is_nil() {
        return None;
    }
    blake3::Hash::from_hex(secret).ok()?;
    // Public credentials cannot create work for an arbitrary Organization Cell.
    // Read the current catalog so a newly published repository works on peers.
    let (catalog, _) = server.catalog()?.load_snapshot().await.ok()?;
    if !catalog.repositories.iter().any(|record| {
        record.organization_id() == Some(organization)
            && record.application == crate::catalog::RepositoryApplicationState::CellReady
    }) {
        return None;
    }
    let proof = stored::Proof {
        identity,
        token,
        digest: key(password),
    };
    let value = verification(server, organization, proof.clone(), None)
        .await
        .ok()??;
    Some(Principal::automation(Credential {
        organization,
        proof,
        verified: Identity {
            issuer: "urn:crab:automation".into(),
            subject: format!("automation:{identity}"),
            name: value.name,
        },
        expires_at: value.expires_at,
    }))
}

async fn verification(
    server: &Server,
    organization: Uuid,
    proof: stored::Proof,
    repository: Option<(Uuid, u32)>,
) -> Result<Option<stored::Verified>> {
    let cells = server.cells().ok_or(Error::CellUnavailable)?;
    match cells
        .query::<stored::Verify>(
            Address::Organization(organization),
            None,
            stored::VerifyInput { proof, repository },
        )
        .await
    {
        Ok(result) => Ok(result.output),
        Err(crate::cells::Error::NotInitialized | crate::cells::Error::AccessDenied) => Ok(None),
        Err(error) => Err(app::Error::Cell(error)),
    }
}
