use std::sync::Arc;
use uuid::Uuid;

use super::{GitToken, Identity, Session};
use crate::{RepositoryAccess, RepositoryConfig, RepositoryGroup, RepositoryMember};

pub(super) fn group_access(
    claims: &[String],
    grants: &[RepositoryGroup],
) -> Option<RepositoryAccess> {
    grants
        .iter()
        .filter(|grant| claims.binary_search(&grant.group).is_ok())
        .map(|grant| grant.access)
        .max()
}

#[derive(Clone)]
enum Credential {
    Anonymous,
    Local,
    User(Arc<Session>),
    Git(Arc<GitToken>),
    Automation(Arc<super::automation::Credential>),
    Deploy { expires_at: u64 },
    Import(Arc<crate::git_import::ReceiveGrant>),
    AutoMerge(Identity),
}

#[derive(Clone, Copy)]
struct Grant {
    repository: Uuid,
    access: RepositoryAccess,
    role: RepositoryAccess,
}

#[derive(Clone, Copy)]
enum RepositoryScope {
    Unrestricted,
    Target(Uuid),
    Denied,
}

#[derive(Clone)]
pub(crate) struct Principal {
    credential: Credential,
    grant: Option<Grant>,
    repository_scope: RepositoryScope,
}

impl Principal {
    pub(crate) fn anonymous() -> Self {
        Self {
            credential: Credential::Anonymous,
            grant: None,
            repository_scope: RepositoryScope::Unrestricted,
        }
    }
    pub(crate) fn local() -> Self {
        Self {
            credential: Credential::Local,
            grant: None,
            repository_scope: RepositoryScope::Unrestricted,
        }
    }
    pub(super) fn user(session: Arc<Session>) -> Self {
        Self {
            credential: Credential::User(session),
            grant: None,
            repository_scope: RepositoryScope::Unrestricted,
        }
    }
    pub(super) fn git(token: Arc<GitToken>) -> Self {
        Self {
            credential: Credential::Git(token),
            grant: None,
            repository_scope: RepositoryScope::Unrestricted,
        }
    }
    pub(super) fn automation(credential: super::automation::Credential) -> Self {
        Self {
            credential: Credential::Automation(Arc::new(credential)),
            grant: None,
            repository_scope: RepositoryScope::Unrestricted,
        }
    }

    pub(crate) fn automation_credential(&self) -> Option<&super::automation::Credential> {
        match &self.credential {
            Credential::Automation(credential) => Some(credential),
            _ => None,
        }
    }

    pub(super) fn with_automation_access(
        &self,
        repository: Uuid,
        access: RepositoryAccess,
    ) -> Self {
        Self {
            credential: self.credential.clone(),
            grant: Some(Grant {
                repository,
                access,
                role: access,
            }),
            repository_scope: self.repository_scope,
        }
    }

    pub(super) fn deploy(repository: Uuid, expires_at: u64) -> Self {
        Self {
            credential: Credential::Deploy { expires_at },
            grant: Some(Grant {
                repository,
                access: RepositoryAccess::Read,
                role: RepositoryAccess::Read,
            }),
            repository_scope: RepositoryScope::Target(repository),
        }
    }
    pub(crate) fn is_deploy(&self) -> bool {
        matches!(self.credential, Credential::Deploy { .. })
    }
    pub(crate) fn import(grant: Arc<crate::git_import::ReceiveGrant>) -> Self {
        let repository = grant.repository.id;
        Self {
            credential: Credential::Import(grant),
            grant: Some(Grant {
                repository,
                access: RepositoryAccess::Write,
                role: RepositoryAccess::Write,
            }),
            repository_scope: RepositoryScope::Target(repository),
        }
    }
    pub(crate) fn import_grant(&self) -> Option<&crate::git_import::ReceiveGrant> {
        match &self.credential {
            Credential::Import(grant) => Some(grant),
            _ => None,
        }
    }
    pub(crate) fn auto_merge(repository: Uuid, actor: Identity) -> Self {
        Self {
            credential: Credential::AutoMerge(actor),
            grant: None,
            repository_scope: RepositoryScope::Target(repository),
        }
    }
    pub(crate) fn is_auto_merge(&self) -> bool {
        matches!(self.credential, Credential::AutoMerge(_))
    }
    pub(crate) fn is_local(&self) -> bool {
        matches!(self.credential, Credential::Local)
    }
    /// Restrict repository access; an absent identity denies all repository access.
    #[must_use]
    pub(crate) fn scoped_to_repository(mut self, repository: Option<Uuid>) -> Self {
        // Current grants may refresh during a request. They cannot widen its
        // admitted target, including the otherwise unrestricted local principal.
        self.repository_scope = match (self.repository_scope, repository) {
            (RepositoryScope::Unrestricted, Some(target)) => RepositoryScope::Target(target),
            (RepositoryScope::Target(current), Some(target)) if current == target => {
                RepositoryScope::Target(current)
            }
            _ => RepositoryScope::Denied,
        };
        self
    }

    pub(crate) fn related_read(&self, origin: Uuid, related: Uuid) -> Option<Self> {
        if !self.can_read(origin) {
            return None;
        }
        // Resolve an independent grant from the browser credential. A Git
        // token or worker scoped to the origin cannot read another repository.
        let reader = if let Some(session) = self.user_session() {
            Self::user(Arc::clone(session))
        } else if self.is_local() {
            Self::local()
        } else {
            return None;
        };
        Some(reader.scoped_to_repository(Some(related)))
    }
    pub(crate) fn is_user(&self) -> bool {
        self.user_session().is_some()
    }
    pub(crate) fn is_git(&self) -> bool {
        matches!(
            self.credential,
            Credential::Git(_) | Credential::Deploy { .. } | Credential::Automation(_)
        )
    }
    pub(crate) fn is_anonymous(&self) -> bool {
        matches!(self.credential, Credential::Anonymous)
    }
    pub(super) fn user_session(&self) -> Option<&Arc<Session>> {
        match &self.credential {
            Credential::User(session) if session.active() => Some(session),
            _ => None,
        }
    }
    pub(crate) fn authenticated(&self) -> bool {
        match &self.credential {
            Credential::Anonymous => false,
            Credential::Local | Credential::Import(_) | Credential::AutoMerge(_) => true,
            Credential::User(session) => session.active(),
            Credential::Git(token) => token.active(),
            Credential::Automation(credential) => credential.active(),
            Credential::Deploy { expires_at } => {
                super::now_epoch().is_ok_and(|now| now < *expires_at)
            }
        }
    }
    pub(crate) fn identity(&self) -> Option<Identity> {
        match &self.credential {
            Credential::User(session) if session.active() => Some(session.identity.clone()),
            Credential::Git(token) if token.active() => Some(token.session.identity.clone()),
            Credential::Automation(credential) if credential.active() => {
                Some(credential.identity())
            }
            Credential::AutoMerge(actor) => Some(actor.clone()),
            Credential::Import(_) => Some(Identity {
                issuer: "urn:crab:git-import".into(),
                subject: "import-worker".into(),
                name: "Git import worker".into(),
            }),
            Credential::Local => Some(Identity {
                issuer: "urn:crab:local".into(),
                subject: "operator".into(),
                name: "Local operator".into(),
            }),
            _ => None,
        }
    }

    #[cfg(test)]
    pub(crate) fn with_membership(
        &self,
        repository: Uuid,
        config: &RepositoryConfig,
        issuer: &str,
        organization_access: Option<RepositoryAccess>,
    ) -> Self {
        self.with_catalog_membership(repository, config, issuer, organization_access, false)
    }

    pub(crate) fn with_catalog_membership(
        &self,
        repository: Uuid,
        config: &RepositoryConfig,
        issuer: &str,
        organization_access: Option<RepositoryAccess>,
        renamed: bool,
    ) -> Self {
        let account = match &self.credential {
            // Deploy credentials are checked against the repository's Cell
            // on each transport request. Catalog refresh cannot widen their UUID scope.
            Credential::Local | Credential::Deploy { .. } => return self.clone(),
            Credential::User(session) if session.active() => Some((
                &session.identity,
                session.groups.as_slice(),
                RepositoryAccess::Admin,
            )),
            Credential::Git(token)
                if token.active()
                    && token.repository_id == repository
                    && token.owner == config.owner
                    // The current catalog can attest to a rename of this UUID.
                    // The issuance address alone cannot establish that identity.
                    && (token.repository == config.name || renamed)
                    && token.session.active() =>
            {
                Some((
                    &token.session.identity,
                    token.session.groups.as_slice(),
                    token.access,
                ))
            }
            // A stored auto-merge can outlive the scheduling session. Only
            // current durable repository and organization grants qualify it.
            Credential::AutoMerge(actor) => Some((actor, &[][..], RepositoryAccess::Write)),
            _ => None,
        };
        let grant = account.and_then(|(identity, groups, ceiling)| {
            if identity.issuer != issuer {
                return None;
            }
            let individual = RepositoryMember::access_for(&config.members, &identity.subject);
            let group = group_access(groups, &config.groups);
            individual
                .into_iter()
                .chain(group)
                .chain(organization_access)
                .max()
                .map(|role| Grant {
                    repository,
                    role,
                    access: role.min(ceiling),
                })
        });
        Self {
            credential: self.credential.clone(),
            grant,
            repository_scope: self.repository_scope,
        }
    }

    fn access(&self, repository: Uuid) -> Option<RepositoryAccess> {
        match self.repository_scope {
            RepositoryScope::Denied => return None,
            RepositoryScope::Target(target) if target != repository => return None,
            _ => (),
        }
        if self.is_local() {
            return Some(RepositoryAccess::Admin);
        }
        // Credentials can expire or be revoked after admission. A grant remains
        // tied to its stable repository ID, never a reused route or cached config.
        self.grant
            .filter(|grant| grant.repository == repository && self.authenticated())
            .map(|grant| grant.access)
    }
    pub(crate) fn write_role(&self, repository: Uuid) -> Option<RepositoryAccess> {
        // A write-scoped Git token preserves the account role for rulesets
        // without gaining access to repository administration endpoints.
        // Automation pushes participate in protection without human role bypass.
        if !self.can_write(repository) || self.automation_credential().is_some() {
            return None;
        }
        if self.is_local() {
            return Some(RepositoryAccess::Admin);
        }
        self.grant.map(|grant| grant.role)
    }

    pub(crate) fn can_read(&self, repository: Uuid) -> bool {
        self.access(repository).is_some()
    }
    pub(crate) fn can_triage(&self, repository: Uuid) -> bool {
        self.access(repository)
            .is_some_and(|access| access >= RepositoryAccess::Triage)
    }
    pub(crate) fn can_write(&self, repository: Uuid) -> bool {
        self.access(repository)
            .is_some_and(|access| access >= RepositoryAccess::Write)
    }
    pub(crate) fn can_admin(&self, repository: Uuid) -> bool {
        self.access(repository) == Some(RepositoryAccess::Admin)
    }
}

#[cfg(test)]
mod tests;
