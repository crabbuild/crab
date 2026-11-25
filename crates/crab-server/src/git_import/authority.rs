use uuid::Uuid;

use crate::{
    app,
    auth::{Identity, Principal},
    catalog::{BrowserNamespace, CatalogStore, MembershipActor, visibility::Visibility},
    cells::{
        Address,
        organization::{
            Caller, Role,
            properties::{
                ImportValues, InitializeImportValues, PropertyValue, validate_initial_values,
            },
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
    pub(super) organization: Option<OrganizationDestination>,
    organization_role: Option<Role>,
    local: bool,
}

impl ImportAccount {
    pub(super) async fn resolve(
        server: &Server,
        principal: &Principal,
        organization: Option<Uuid>,
    ) -> Result<Self, ImportError> {
        let key = app::account(principal).map_err(|_| ImportError::NotFound)?;
        let (organization, organization_role) = if let Some(id) = organization {
            let actor = actor_id(server, key)
                .await
                .map_err(ImportError::organization)?
                .ok_or(ImportError::NotFound)?;
            let destination = OrganizationDestination {
                id,
                actor,
                visibility: Visibility::Private,
            };
            (Some(destination), Some(destination.role(server).await?))
        } else {
            (None, None)
        };
        let identity = app::actor(principal).map_err(|_| ImportError::NotFound)?;
        Ok(Self {
            identity,
            principal: principal.clone(),
            organization,
            organization_role,
            local: principal.is_local(),
        })
    }

    pub(super) fn jobs(&self, root: &StorageRoot) -> Jobs {
        match self.organization {
            Some(destination) => Jobs::for_organization(root, destination.id).for_viewer(
                destination.actor,
                self.organization_role == Some(Role::Owner),
            ),
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

    pub(super) async fn check(&self, server: &Server) -> Result<bool, ImportError> {
        app::account(&self.principal).map_err(|_| ImportError::NotFound)?;
        match self.organization {
            Some(destination) => destination.check(server).await,
            None => Ok(false),
        }
    }

    pub(super) async fn check_viewer(&self, server: &Server) -> Result<(), ImportError> {
        if let Some(destination) = self.organization {
            let role = destination.role(server).await?;
            if self.organization_role == Some(Role::Owner) && role != Role::Owner {
                return Err(ImportError::NotFound);
            }
        }
        app::account(&self.principal).map_err(|_| ImportError::NotFound)?;
        Ok(())
    }

    pub(super) fn for_request(mut self, request: &super::ValidatedImport) -> Self {
        if let Some(destination) = &mut self.organization {
            destination.visibility = request.visibility;
        }
        self
    }

    pub(super) fn member_creator(&self, owner: bool) -> Option<Identity> {
        (self.organization.is_some() && !owner && !self.local).then(|| self.identity.clone())
    }

    pub(super) async fn fork_source(
        &self,
        server: &Server,
        catalog: &CatalogStore,
        source: Option<Uuid>,
        visibility: Visibility,
    ) -> Result<Option<Arc<ForkSource>>, ImportError> {
        let Some(source) = source else {
            return Ok(None);
        };
        let (document, _) = catalog
            .load_snapshot()
            .await
            .map_err(ImportError::Catalog)?;
        let record = document
            .repository_by_id(source)
            .ok_or(ImportError::NotFound)?;
        let organization = record.organization_id();
        let access = Arc::new(ForkSource {
            source,
            organization,
            visibility,
            external: self
                .organization
                .is_some_and(|owner| Some(owner.id) != organization),
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
        if visibility.is_public() {
            return Ok(namespace);
        }
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
            organization: source.organization.ok_or(ImportError::NotFound)?,
            owner: identity,
        })
    }
}

pub(super) struct ForkSource {
    pub(super) source: Uuid,
    pub(super) organization: Option<Uuid>,
    pub(super) visibility: Visibility,
    external: bool,
    principal: Principal,
}

impl ForkSource {
    pub(super) async fn current(
        &self,
        server: &Server,
    ) -> Result<(Arc<Repository>, Principal), ImportError> {
        app::account(&self.principal).map_err(|_| ImportError::NotFound)?;
        let catalog = server.catalog().ok_or(ImportError::Unavailable)?;
        let (document, _) = catalog
            .load_snapshot()
            .await
            .map_err(ImportError::Catalog)?;
        let record = document
            .repository_by_id(self.source)
            .filter(|record| {
                record.organization_id() == self.organization
                    && record.forked_from.is_none()
                    && record.visibility.value == self.visibility
            })
            .ok_or(ImportError::NotFound)?;
        let repository = server
            .repositories
            .by_id(record.id)
            .ok_or(ImportError::NotFound)?;
        // Recheck the signed-in account independently of its destination
        // grant. An import credential never confers upstream read access.
        let mut principal = self.principal.clone();
        if self.visibility.is_public() {
            principal.allow_public_reads();
        }
        let membership = app::catalog_membership(server, &principal, &repository)
            .await
            .map_err(ImportError::organization)?;
        if self.visibility.is_public() {
            return Ok((repository, membership.principal));
        }
        let policy = self.policy_grant(server, false).await?;
        if !policy.private_forks || !record.forking.allowed {
            return Err(ImportError::ForkingDisabled);
        }
        if self.external && !policy.external_forks {
            return Err(ImportError::ExternalForkingDisabled);
        }
        Ok((repository, membership.principal))
    }

    pub(super) async fn policy_grant(
        &self,
        server: &Server,
        include_teams: bool,
    ) -> Result<crate::cells::organization::teams::repository_access::ActorTeams, ImportError> {
        let organization = self.organization.ok_or(ImportError::NotFound)?;
        Ok(organization_grants(
            server,
            &self.principal,
            [(organization, include_teams)].into(),
        )
        .await
        .map_err(ImportError::organization)?
        .remove(&organization)
        .unwrap_or_default())
    }
}

#[derive(Clone, Copy)]
pub(super) struct OrganizationDestination {
    pub(super) id: Uuid,
    pub(super) actor: Uuid,
    visibility: Visibility,
}

impl OrganizationDestination {
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
        validate_initial_values(&options.properties, properties)
            .map_err(|error| ImportError::Invalid(error.message()))
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
                                visibility: self.visibility,
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

    pub(super) async fn role(self, server: &Server) -> Result<Role, ImportError> {
        organization_role(server, self.id, self.actor)
            .await
            .map_err(ImportError::organization)?
            .ok_or(ImportError::NotFound)
    }

    pub(super) async fn check(self, server: &Server) -> Result<bool, ImportError> {
        Ok(self.grant(server, false).await?.owner)
    }

    pub(super) async fn policy_grant(
        self,
        server: &Server,
    ) -> app::Result<crate::cells::organization::teams::repository_access::ActorTeams> {
        self.grant(server, true).await.map_err(|error| match error {
            ImportError::Organization(error) => *error,
            ImportError::CreationRestricted => app::Error::Forbidden,
            _ => app::Error::NotFound,
        })
    }

    async fn grant(
        self,
        server: &Server,
        include_teams: bool,
    ) -> Result<crate::cells::organization::teams::repository_access::ActorTeams, ImportError> {
        use crate::cells::organization::teams::repository_access::{
            RepositoryActor, RepositoryTeamsForActor,
        };
        let cells = server.cells().ok_or(ImportError::Unavailable)?;
        let grant = app::query_output(
            cells
                .query::<RepositoryTeamsForActor>(
                    Address::Organization(self.id),
                    None,
                    RepositoryActor {
                        actor: self.actor,
                        include_teams,
                    },
                )
                .await,
        )
        .map_err(ImportError::organization)?;
        if !grant.member {
            return Err(ImportError::NotFound);
        }
        if !grant.creation_visibilities.contains(&self.visibility) {
            return Err(ImportError::CreationRestricted);
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
                    visibility: self.visibility,
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
