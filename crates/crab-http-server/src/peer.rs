use std::{future::Future, pin::Pin};

use crab_cell_runtime::{
    ApplicationIdentity, CellAuthority, CellCatalog, CellHandle, CellRuntime, CellTarget,
    Error as CellError, PeerAuthorizer, PeerCellResolver, VerifiedPeerRequest, peer_wire,
};
use crab_storage::CellStorageLayout;
use uuid::Uuid;

use crate::{RepositoryAccess, RepositoryConfig, server::Server};

/// Resolves peer requests only when this process still owns the exact active Cell.
#[derive(Clone)]
pub(crate) struct LocalCellResolver {
    identity: ApplicationIdentity,
    catalog: CellCatalog,
    authority: CellAuthority,
    runtime: CellRuntime,
}

impl LocalCellResolver {
    pub(crate) fn new(
        layout: CellStorageLayout,
        identity: ApplicationIdentity,
        runtime: CellRuntime,
    ) -> Self {
        Self {
            identity,
            catalog: CellCatalog::new(layout.clone(), identity.tenant()),
            authority: CellAuthority::new(layout),
            runtime,
        }
    }
}

impl PeerCellResolver for LocalCellResolver {
    fn resolve(
        &self,
        target: CellTarget,
    ) -> Pin<Box<dyn Future<Output = crab_cell_runtime::Result<CellHandle>> + Send + 'static>> {
        let resolver = self.clone();
        Box::pin(async move {
            if target.tenant() != resolver.identity.tenant()
                || target.application() != resolver.identity.application()
            {
                return Err(denied());
            }
            let proof = resolver
                .catalog
                .lookup(target.cell_id())
                .await?
                .ok_or(CellError::CellNotActive)?;
            if proof.entry().namespace() != target.namespace()
                || proof.entry().partition() != target.partition()
            {
                return Err(CellError::CatalogCollision);
            }
            let control = resolver
                .authority
                .load(target.cell_id())
                .await?
                .ok_or(CellError::CellNotActive)?;
            resolver
                .runtime
                .local_handle(proof, &control)
                .await?
                .ok_or(CellError::CellNotActive)
        })
    }
}

impl PeerAuthorizer for Server {
    fn authorize(&self, request: &VerifiedPeerRequest) -> crab_cell_runtime::Result<()> {
        if request.target().namespace() != crate::cells::REPOSITORY_NAMESPACE {
            return Err(denied());
        }
        let repository_id = Uuid::from_bytes(
            request
                .target()
                .partition()
                .try_into()
                .map_err(|_| denied())?,
        );
        let repository = self.repositories.by_id(repository_id).ok_or_else(denied)?;
        let issuer = self
            .auth
            .as_ref()
            .map(crate::auth::Authentication::peer_issuer);
        authorize_repository(&repository.config, issuer.as_deref(), request)
    }
}

fn authorize_repository(
    repository: &RepositoryConfig,
    issuer: Option<&str>,
    request: &VerifiedPeerRequest,
) -> crab_cell_runtime::Result<()> {
    let principal = request.principal();
    let access = match issuer {
        Some(expected) if principal.issuer == expected => repository
            .members
            .iter()
            .find(|member| member.subject == principal.subject)
            .map(|member| member.access),
        None if principal.issuer == "urn:crab:local" && principal.subject == "operator" => {
            Some(RepositoryAccess::Admin)
        }
        Some(_) | None => None,
    }
    .ok_or_else(denied)?;

    let authorized = match request.operation() {
        Some(peer_wire::peer_request::Operation::Mutate(mutation)) => {
            access >= RepositoryAccess::Write
                && match mutation.operation.as_ref() {
                    Some(peer_wire::mutation_request::Operation::CellCommand(command)) => {
                        required_mutation_action(command.command_id)
                            .is_some_and(|action| request.permits(action))
                    }
                    _ => false,
                }
        }
        Some(peer_wire::peer_request::Operation::Read(read)) => {
            access >= RepositoryAccess::Read
                && request.permits("repository.read")
                && matches!(
                    read.operation,
                    Some(peer_wire::read_request::Operation::Describe(true))
                        | Some(peer_wire::read_request::Operation::CellQuery(_))
                )
        }
        Some(peer_wire::peer_request::Operation::Resolve(_)) => {
            access >= RepositoryAccess::Write
                && ["repository.issue.create", "repository.comment.create"]
                    .iter()
                    .any(|action| request.permits(action))
        }
        _ => false,
    };
    if !authorized {
        return Err(denied());
    }
    Ok(())
}

const fn required_mutation_action(command_id: u32) -> Option<&'static str> {
    match command_id {
        1 => Some("repository.issue.create"),
        2 => Some("repository.comment.create"),
        _ => None,
    }
}

const fn denied() -> CellError {
    CellError::PeerAuthorization("repository principal or action is no longer authorized")
}

#[cfg(test)]
mod tests {
    use crab_cell_runtime::{
        Digest, PeerOperation, PeerPrincipal, PeerSigner, PeerVerifier, RequestId, SessionId,
    };
    use ed25519_dalek::SigningKey;

    use super::*;

    const NOW_MS: i64 = 1_000_000;

    fn repository() -> RepositoryConfig {
        RepositoryConfig {
            owner: "team".into(),
            name: "repository".into(),
            bucket: "bucket".into(),
            prefix: "repository".into(),
            default_branch: "main".into(),
            description: String::new(),
            members: vec![crate::RepositoryMember {
                subject: "alice".into(),
                name: "Alice".into(),
                access: RepositoryAccess::Write,
            }],
            protected_branches: Vec::new(),
        }
    }

    fn verified(actions: Vec<String>) -> VerifiedPeerRequest {
        let key = SigningKey::from_bytes(&[1; 32]);
        let signer = PeerSigner::new(
            SessionId::from_bytes([2; 16]),
            Digest::from_bytes([3; 32]),
            key,
        );
        let target = peer_wire::Target {
            tenant_id: vec![4; 16],
            application_id: vec![5; 16],
            namespace_id: crate::cells::REPOSITORY_NAMESPACE.as_bytes().to_vec(),
            partition: [6; 16].to_vec(),
        };
        let encoded = signer
            .sign(
                PeerPrincipal {
                    issuer: "https://issuer.example".into(),
                    subject: "alice".into(),
                    actions,
                },
                NOW_MS,
                NOW_MS + 60_000,
                30_000,
                PeerOperation::Mutate(peer_wire::MutationRequest {
                    target: Some(target),
                    identity: Some(peer_wire::MutationIdentity {
                        request_id: RequestId::from_bytes([7; 16]).as_bytes().to_vec(),
                        incarnation: [8; 16].to_vec(),
                        issued_at_ms: NOW_MS,
                        expires_at_ms: NOW_MS + 60_000,
                    }),
                    timeout_ms: 30_000,
                    operation: Some(peer_wire::mutation_request::Operation::CellCommand(
                        peer_wire::CellCommand {
                            command_id: 1,
                            codec_version: 1,
                            input: Vec::new(),
                        },
                    )),
                }),
            )
            .unwrap();
        PeerVerifier::new(
            SessionId::from_bytes([2; 16]),
            Digest::from_bytes([3; 32]),
            signer.verifying_key(),
        )
        .verify(&encoded, NOW_MS)
        .unwrap()
    }

    #[test]
    fn current_membership_and_exact_action_are_required() {
        let request = verified(vec!["repository.issue.create".into()]);
        assert!(
            authorize_repository(&repository(), Some("https://issuer.example"), &request).is_ok()
        );

        let mut revoked = repository();
        revoked.members.clear();
        assert!(authorize_repository(&revoked, Some("https://issuer.example"), &request).is_err());
        assert!(
            authorize_repository(&repository(), Some("https://other.example"), &request).is_err()
        );
        let wrong_action = verified(vec!["repository.comment.create".into()]);
        assert!(
            authorize_repository(&repository(), Some("https://issuer.example"), &wrong_action)
                .is_err()
        );
    }
}
