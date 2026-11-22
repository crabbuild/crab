use uuid::Uuid;

use crate::{
    app,
    auth::{Identity, Principal},
    catalog::BrowserNamespace,
    cells::{
        Address,
        organization::{
            Role,
            repositories::{AuthorizeImportPublication, Authorized, ImportPublication},
        },
    },
    repository::access::{actor_id, organization_role},
    server::Server,
    storage_root::StorageRoot,
};

use super::{
    ImportError,
    jobs::{Jobs, Snapshot},
};

pub(super) struct ImportAccount {
    pub(super) identity: Identity,
    pub(super) principal: Principal,
    pub(super) organization: Option<OrganizationOwner>,
    local: bool,
}

impl ImportAccount {
    pub(super) async fn resolve(
        server: &Server,
        principal: &Principal,
        organization: Option<Uuid>,
    ) -> Result<Self, ImportError> {
        let key = app::account(principal).map_err(|_| ImportError::NotFound)?;
        let organization = if let Some(id) = organization {
            let actor = actor_id(server, key)
                .await
                .map_err(ImportError::organization)?
                .ok_or(ImportError::NotFound)?;
            let owner = OrganizationOwner { id, actor };
            owner.check(server).await?;
            Some(owner)
        } else {
            None
        };
        let identity = app::actor(principal).map_err(|_| ImportError::NotFound)?;
        Ok(Self {
            identity,
            principal: principal.clone(),
            organization,
            local: principal.is_local(),
        })
    }

    pub(super) fn jobs(&self, root: &StorageRoot) -> Jobs {
        match self.organization {
            Some(owner) => Jobs::for_organization(root, owner.id),
            None => Jobs::new(root, &self.identity),
        }
    }

    pub(super) fn namespace(&self, owner: &str) -> Result<BrowserNamespace, ImportError> {
        if let Some(organization) = self.organization {
            if owner != crate::catalog::organization_namespace(organization.id) {
                return Err(ImportError::DestinationChanged);
            }
            return Ok(BrowserNamespace::Organization(organization.id));
        }
        if crate::catalog::is_organization_namespace(owner) {
            return Err(ImportError::Invalid(
                "Start the import from the organization's Repositories tab",
            ));
        }
        if !self.local
            && crate::catalog::is_personal_namespace(owner)
            && owner
                != crate::catalog::personal_namespace(&self.identity.issuer, &self.identity.subject)
        {
            return Err(ImportError::Invalid(
                "Imports into personal namespaces must use your own namespace",
            ));
        }
        Ok(BrowserNamespace::Personal(owner.into()))
    }

    pub(super) async fn check(&self, server: &Server) -> Result<(), ImportError> {
        if let Some(owner) = self.organization {
            owner.check(server).await?;
        }
        Ok(())
    }
}

#[derive(Clone, Copy)]
pub(super) struct OrganizationOwner {
    pub(super) id: Uuid,
    pub(super) actor: Uuid,
}

impl OrganizationOwner {
    pub(super) async fn check(self, server: &Server) -> Result<(), ImportError> {
        match organization_role(server, self.id, self.actor)
            .await
            .map_err(ImportError::organization)?
        {
            Some(Role::Owner) => Ok(()),
            _ => Err(ImportError::NotFound),
        }
    }

    pub(super) async fn policy_grant(
        self,
        server: &Server,
    ) -> app::Result<crate::cells::organization::teams::repository_access::ActorTeams> {
        use crate::cells::organization::teams::repository_access::{
            RepositoryActor, RepositoryTeamsForActor,
        };
        let cells = server.cells().ok_or(app::Error::CellUnavailable)?;
        let grant = app::query_output(
            cells
                .query::<RepositoryTeamsForActor>(
                    Address::Organization(self.id),
                    None,
                    RepositoryActor {
                        actor: self.actor,
                        include_teams: true,
                    },
                )
                .await,
        )?;
        if !grant.owner {
            return Err(app::Error::NotFound);
        }
        Ok(grant)
    }

    pub(super) async fn authorize_publication(
        self,
        server: &Server,
        repository: Uuid,
        snapshot: &Snapshot,
        expected_policy: u64,
    ) -> Result<(), ImportError> {
        let cells = server.cells().ok_or(ImportError::Unavailable)?;
        let submission = app::mutation_identity().map_err(ImportError::organization)?;
        let response = cells
            .command::<AuthorizeImportPublication>(
                Address::Organization(self.id),
                submission,
                ImportPublication {
                    actor: self.actor,
                    repository,
                    snapshot: snapshot.publication_digest(),
                    expected_policy,
                },
            )
            .await
            .map_err(|error| match error {
                crate::cells::Error::NotInitialized | crate::cells::Error::AccessDenied => {
                    ImportError::NotFound
                }
                error => ImportError::organization(app::Error::Cell(error)),
            })?;
        match response.output {
            Authorized::Accepted => Ok(()),
            Authorized::Conflict => Err(ImportError::DestinationChanged),
        }
    }
}
