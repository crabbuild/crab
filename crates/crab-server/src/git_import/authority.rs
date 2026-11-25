use uuid::Uuid;

use crate::{
    app,
    auth::{Identity, Principal},
    catalog::{BrowserNamespace, CatalogStore, MembershipActor, visibility::Visibility},
    cells::{
        Address,
        organization::{
            Caller, Role,
            properties::{ImportValues, InitializeImportValues, PropertyValue},
            repositories::{
                AuthorizeImportPublication, Authorized, GetCreationOptions, ImportPublication,
            },
        },
    },
    repository::{
        Repository,
        access::{actor_id, organization_grants, organization_role},
        settings::{RulesetScope, with_ruleset_lease},
    },
    server::Server,
    storage_root::StorageRoot,
};

use super::{
    ImportError,
    jobs::{Jobs, Snapshot, Writer},
};
use std::{collections::BTreeMap, sync::Arc};

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

    pub(super) fn namespace(
        &self,
        owner: &str,
        visibility: Visibility,
    ) -> Result<BrowserNamespace, ImportError> {
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
        let personal =
            crate::catalog::personal_namespace(&self.identity.issuer, &self.identity.subject);
        if visibility.is_public() && (self.local || owner != personal) {
            return Err(ImportError::Invalid(
                "Public imports require an organization or your managed personal namespace",
            ));
        }
        if !self.local && crate::catalog::is_personal_namespace(owner) && owner != personal {
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

    pub(super) async fn fork_source(
        &self,
        server: &Server,
        catalog: &CatalogStore,
        source: Option<Uuid>,
    ) -> Result<Option<Arc<ForkSource>>, ImportError> {
        let Some(source) = source else {
            return Ok(None);
        };
        let (document, _) = catalog
            .load_snapshot()
            .await
            .map_err(ImportError::Catalog)?;
        let organization = document
            .repository_by_id(source)
            .and_then(|record| record.organization_id())
            .ok_or(ImportError::NotFound)?;
        if self
            .organization
            .is_some_and(|owner| owner.id != organization)
        {
            return Err(ImportError::NotFound);
        }
        let access = Arc::new(ForkSource {
            source,
            organization,
            owner: self.organization,
            principal: self.principal.clone(),
        });
        access.current(server).await?;
        Ok(Some(access))
    }

    pub(super) fn destination(
        &self,
        owner: &str,
        visibility: Visibility,
        source: Option<&ForkSource>,
    ) -> Result<BrowserNamespace, ImportError> {
        let namespace = self.namespace(owner, visibility)?;
        let Some(source) = source.filter(|_| self.organization.is_none()) else {
            return Ok(namespace);
        };
        let identity = MembershipActor {
            issuer: self.identity.issuer.clone(),
            subject: self.identity.subject.clone(),
        };
        if owner != crate::catalog::personal_namespace(&identity.issuer, &identity.subject) {
            return Err(ImportError::DestinationChanged);
        }
        Ok(BrowserNamespace::PersonalFork {
            organization: source.organization,
            owner: identity,
        })
    }
}

pub(super) struct ForkSource {
    pub(super) source: Uuid,
    pub(super) organization: Uuid,
    owner: Option<OrganizationOwner>,
    principal: Principal,
}

impl ForkSource {
    pub(super) async fn current(
        &self,
        server: &Server,
    ) -> Result<(Arc<Repository>, Principal), ImportError> {
        app::account(&self.principal).map_err(|_| ImportError::NotFound)?;
        if let Some(owner) = self.owner {
            owner.check(server).await?;
        }
        let catalog = server.catalog().ok_or(ImportError::Unavailable)?;
        let (document, _) = catalog
            .load_snapshot()
            .await
            .map_err(ImportError::Catalog)?;
        let record = document
            .repository_by_id(self.source)
            .filter(|record| {
                record.organization_id() == Some(self.organization) && record.forked_from.is_none()
            })
            .ok_or(ImportError::NotFound)?;
        let repository = server
            .repositories
            .by_id(record.id)
            .ok_or(ImportError::NotFound)?;
        // The fork grant rechecks the source account against the catalog.
        // The import credential being verified only authorizes the destination.
        let membership = app::catalog_membership(server, &self.principal, &repository)
            .await
            .map_err(ImportError::organization)?;
        let allowed = self.policy_grant(server, false).await?.private_forks;
        if !allowed || !record.forking.allowed || record.visibility.value.is_public() {
            return Err(ImportError::ForkingDisabled);
        }
        Ok((repository, membership.principal))
    }

    pub(super) async fn policy_grant(
        &self,
        server: &Server,
        include_teams: bool,
    ) -> Result<crate::cells::organization::teams::repository_access::ActorTeams, ImportError> {
        Ok(organization_grants(
            server,
            &self.principal,
            [(self.organization, include_teams)].into(),
        )
        .await
        .map_err(ImportError::organization)?
        .remove(&self.organization)
        .unwrap_or_default())
    }
}

#[derive(Clone, Copy)]
pub(super) struct OrganizationOwner {
    pub(super) id: Uuid,
    pub(super) actor: Uuid,
}

impl OrganizationOwner {
    pub(super) async fn validate_properties(
        self,
        server: &Server,
        properties: &BTreeMap<Uuid, PropertyValue>,
    ) -> Result<(), ImportError> {
        let cells = server.cells().ok_or(ImportError::Unavailable)?;
        let options = cells
            .query::<GetCreationOptions>(
                Address::Organization(self.id),
                None,
                Caller { actor: self.actor },
            )
            .await
            .map_err(import_cell_error)?
            .output;
        if !properties.iter().all(|(id, value)| {
            options
                .properties
                .iter()
                .find(|field| field.id == *id)
                .is_some_and(|field| field.accepts(value))
        }) {
            return Err(ImportError::Invalid(
                "Choose valid values for this organization's repository properties",
            ));
        }
        Ok(())
    }

    pub(super) async fn initialize_properties(
        self,
        server: &Server,
        job: &Writer,
        repository: Uuid,
        overrides: &BTreeMap<Uuid, PropertyValue>,
    ) -> Result<(), ImportError> {
        if overrides.is_empty() {
            return Ok(());
        }
        // Ruleset evaluation during receive must see the saved job's values,
        // including when a different owner resumes the same quarantine.
        with_ruleset_lease(
            server,
            RulesetScope::Organization(self.id),
            &server.cancellation,
            |cancel| async move {
                let outcome = async {
                    job.verify().await?;
                    if cancel.is_cancelled() {
                        return Err(ImportError::Cancelled);
                    }
                    let cells = server.cells().ok_or(ImportError::Unavailable)?;
                    let identity = app::mutation_identity().map_err(ImportError::organization)?;
                    cells
                        .command::<InitializeImportValues>(
                            Address::Organization(self.id),
                            identity,
                            ImportValues {
                                actor: self.actor,
                                repository,
                                overrides: overrides.clone(),
                            },
                        )
                        .await
                        .map_err(import_cell_error)?;
                    Ok(())
                }
                .await;
                Ok(outcome)
            },
        )
        .await
        .map_err(ImportError::organization)?
    }

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
            .map_err(import_cell_error)?;
        match response.output {
            Authorized::Accepted => Ok(()),
            Authorized::Conflict => Err(ImportError::DestinationChanged),
        }
    }
}

fn import_cell_error(error: crate::cells::Error) -> ImportError {
    match error {
        crate::cells::Error::NotInitialized | crate::cells::Error::AccessDenied => {
            ImportError::NotFound
        }
        error => ImportError::organization(app::Error::Cell(error)),
    }
}
