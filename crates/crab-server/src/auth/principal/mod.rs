use std::sync::Arc;
use uuid::Uuid;

use super::{GitToken, Identity, Session};
use crate::{RepositoryAccess, RepositoryConfig};

#[derive(Clone)]
enum Credential {
    Anonymous,
    Local,
    User(Arc<Session>),
    Git(Arc<GitToken>),
}

#[derive(Clone, Copy)]
struct Grant {
    repository: Uuid,
    access: RepositoryAccess,
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
    pub(crate) fn is_user(&self) -> bool {
        self.user_session().is_some()
    }
    pub(crate) fn is_git(&self) -> bool {
        matches!(self.credential, Credential::Git(_))
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
            Credential::Local => true,
            Credential::User(session) => session.active(),
            Credential::Git(token) => token.active(),
        }
    }
    pub(crate) fn identity(&self) -> Option<Identity> {
        match &self.credential {
            Credential::User(session) if session.active() => Some(session.identity.clone()),
            Credential::Git(token) if token.active() => Some(token.session.identity.clone()),
            Credential::Local => Some(Identity {
                issuer: "urn:crab:local".into(),
                subject: "operator".into(),
                name: "Local operator".into(),
            }),
            _ => None,
        }
    }

    pub(crate) fn with_membership(
        &self,
        repository: Uuid,
        config: &RepositoryConfig,
        issuer: &str,
    ) -> Self {
        let account = match &self.credential {
            Credential::Local => return self.clone(),
            Credential::User(session) => Some((session, RepositoryAccess::Admin)),
            Credential::Git(token)
                if token.active()
                    && token.repository_id == repository
                    && token.owner == config.owner
                    && token.repository == config.name =>
            {
                Some((&token.session, token.access))
            }
            _ => None,
        };
        let access = account.and_then(|(session, ceiling)| {
            if !session.active() || session.identity.issuer != issuer {
                return None;
            }
            config
                .members
                .iter()
                .find(|member| member.subject == session.identity.subject)
                .map(|member| std::cmp::min(member.access, ceiling))
        });
        Self {
            credential: self.credential.clone(),
            grant: access.map(|access| Grant { repository, access }),
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
