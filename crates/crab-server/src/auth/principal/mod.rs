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
    Mirror,
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
    allow_public_reads: bool,
    public_read: Option<Uuid>,
}

impl Principal {
    pub(crate) fn anonymous() -> Self {
        Self {
            credential: Credential::Anonymous,
            grant: None,
            repository_scope: RepositoryScope::Unrestricted,
            allow_public_reads: false,
            public_read: None,
        }
    }
    pub(crate) fn local() -> Self {
        Self {
            credential: Credential::Local,
            grant: None,
            repository_scope: RepositoryScope::Unrestricted,
            allow_public_reads: false,
            public_read: None,
        }
    }
    pub(super) fn user(session: Arc<Session>) -> Self {
        Self {
            credential: Credential::User(session),
            grant: None,
            repository_scope: RepositoryScope::Unrestricted,
            allow_public_reads: false,
            public_read: None,
        }
    }
    pub(super) fn git(token: Arc<GitToken>) -> Self {
        Self {
            credential: Credential::Git(token),
            grant: None,
            repository_scope: RepositoryScope::Unrestricted,
            allow_public_reads: false,
            public_read: None,
        }
    }
    pub(super) fn automation(credential: super::automation::Credential) -> Self {
        Self {
            credential: Credential::Automation(Arc::new(credential)),
            grant: None,
            repository_scope: RepositoryScope::Unrestricted,
            allow_public_reads: false,
            public_read: None,
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
            ..self.clone()
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
            allow_public_reads: false,
            public_read: None,
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
            allow_public_reads: false,
            public_read: None,
        }
    }
    pub(crate) fn import_grant(&self) -> Option<&crate::git_import::ReceiveGrant> {
        match &self.credential {
            Credential::Import(grant) => Some(grant),
            _ => None,
        }
    }
    /// The server's own synchronization of one read-only pull mirror.
    ///
    /// It is never derived from a request credential, so no client can
    /// publish to a mirror through it.
    pub(crate) fn mirror(repository: Uuid) -> Self {
        Self {
            credential: Credential::Mirror,
            grant: Some(Grant {
                repository,
                access: RepositoryAccess::Write,
                role: RepositoryAccess::Write,
            }),
            repository_scope: RepositoryScope::Target(repository),
            allow_public_reads: false,
            public_read: None,
        }
    }
    pub(crate) fn is_mirror(&self) -> bool {
        matches!(self.credential, Credential::Mirror)
    }
    pub(crate) fn auto_merge(repository: Uuid, actor: Identity) -> Self {
        Self {
            credential: Credential::AutoMerge(actor),
            grant: None,
            repository_scope: RepositoryScope::Target(repository),
            allow_public_reads: false,
            public_read: None,
        }
    }
    pub(crate) fn is_auto_merge(&self) -> bool {
        matches!(self.credential, Credential::AutoMerge(_))
    }
    pub(crate) fn is_local(&self) -> bool {
        matches!(self.credential, Credential::Local)
    }

    pub(crate) fn allow_public_reads(&mut self) {
        self.allow_public_reads = self.is_anonymous() || self.is_user();
    }

    pub(crate) fn permits_public_reads(&self) -> bool {
        self.allow_public_reads
    }

    pub(crate) fn without_public_reads(mut self) -> Self {
        self.allow_public_reads = false;
        self.public_read = None;
        self
    }

    pub(crate) fn with_public_visibility(mut self, repository: Uuid, public: bool) -> Self {
        self.public_read = (self.allow_public_reads && public).then_some(repository);
        self
    }

    pub(crate) fn is_public_reader(&self, repository: Uuid) -> bool {
        self.public_read == Some(repository) && self.grant.is_none()
    }

    pub(crate) fn member_identity(&self, repository: Uuid) -> Option<Identity> {
        (!self.is_public_reader(repository))
            .then(|| self.identity())
            .flatten()
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
        Some(
            self.related_catalog(origin)?
                .scoped_to_repository(Some(related)),
        )
    }

    // Only publication can exchange an opted-in Git credential for the
    // issuing account's live grant on a specific issue repository.
    pub(crate) fn related_issue_writer(&self, origin: Uuid, related: Uuid) -> Option<Self> {
        if !self.can_write(origin) {
            return None;
        }
        match &self.credential {
            Credential::Git(token) if token.close_related_issues && token.active() => {
                Some(Self::user(Arc::clone(&token.session)).scoped_to_repository(Some(related)))
            }
            Credential::AutoMerge(actor) => Some(Self::auto_merge(related, actor.clone())),
            _ => self.related_read(origin, related),
        }
    }

    pub(crate) fn related_catalog(&self, origin: Uuid) -> Option<Self> {
        if !self.can_read(origin) {
            return None;
        }
        // Resolve an independent grant from the browser credential. A Git
        // token or worker scoped to the origin cannot read another repository.
        if let Some(session) = self.user_session() {
            Some(Self::user(Arc::clone(session)))
        } else if self.is_local() {
            Some(Self::local())
        } else {
            None
        }
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
            Credential::Local
            | Credential::Import(_)
            | Credential::Mirror
            | Credential::AutoMerge(_) => true,
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
            Credential::Mirror => Some(Identity {
                issuer: "urn:crab:git-mirror".into(),
                subject: "mirror-sync".into(),
                name: "Git mirror".into(),
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
        self.with_catalog_membership(repository, config, issuer, organization_access, false, None)
    }

    pub(crate) fn with_catalog_membership(
        &self,
        repository: Uuid,
        config: &RepositoryConfig,
        issuer: &str,
        organization_access: Option<RepositoryAccess>,
        renamed: bool,
        fork_owner: Option<&crate::catalog::MembershipActor>,
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
            let owns_fork = fork_owner.is_some_and(|owner| {
                owner.issuer == identity.issuer && owner.subject == identity.subject
            });
            let individual = if fork_owner.is_none() || owns_fork {
                RepositoryMember::access_for(&config.members, &identity.subject)
            } else {
                None
            };
            let group = group_access(groups, &config.groups);
            individual
                .into_iter()
                .chain(group)
                .chain(organization_access)
                .max()
                .map(|role| {
                    // The fork owner needs upstream access before getting
                    // write access to their own copy. Keep Git token ceilings.
                    let role = if owns_fork {
                        role.max(RepositoryAccess::Write)
                    } else {
                        role
                    };
                    Grant {
                        repository,
                        role,
                        access: role.min(ceiling),
                    }
                })
        });
        Self {
            grant,
            public_read: None,
            ..self.clone()
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
            .or_else(|| {
                (self.public_read == Some(repository) && (self.is_anonymous() || self.is_user()))
                    .then_some(RepositoryAccess::Read)
            })
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
    pub(crate) fn can_moderate(&self, repository: Uuid) -> bool {
        self.access(repository)
            .is_some_and(|access| access >= RepositoryAccess::Moderate)
    }
    pub(crate) fn can_write(&self, repository: Uuid) -> bool {
        self.access(repository)
            .is_some_and(|access| access >= RepositoryAccess::Write)
    }
    pub(crate) fn can_maintain(&self, repository: Uuid) -> bool {
        self.access(repository)
            .is_some_and(|access| access >= RepositoryAccess::Maintain)
    }
    pub(crate) fn can_admin(&self, repository: Uuid) -> bool {
        self.access(repository) == Some(RepositoryAccess::Admin)
    }
}

#[cfg(test)]
mod tests;
