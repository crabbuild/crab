use std::fmt;

use bytes::Bytes;

use crate::{Error, ErrorKind, Result};

/// Object-hash algorithms supported by the SDK.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum HashAlgorithm {
    Sha1,
}

/// A complete Git object identity with an explicit hash algorithm.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ObjectId([u8; 20]);

impl ObjectId {
    #[cfg(feature = "remote")]
    pub(crate) fn from_owner(oid: gix_hash::ObjectId) -> Result<Self> {
        let bytes = oid.as_bytes().try_into().map_err(|source| {
            Error::with_source(
                ErrorKind::UnsupportedCapability,
                "unsupported object ID algorithm",
                source,
            )
        })?;
        Ok(Self(bytes))
    }
    #[cfg(feature = "write")]
    pub(crate) const fn owner(self) -> gix_hash::ObjectId {
        gix_hash::ObjectId::Sha1(self.0)
    }
    /// Parse a complete hexadecimal SHA-1 identity; SHA-256 is unsupported.
    pub fn from_hex(value: &str) -> Result<Self> {
        if value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(Error::new(
                ErrorKind::UnsupportedCapability,
                "SHA-256 repositories are unsupported",
            ));
        }
        let oid = gix_hash::ObjectId::from_hex(value.as_bytes()).map_err(|source| {
            Error::with_source(
                ErrorKind::InvalidInput,
                "invalid complete SHA-1 object ID",
                source,
            )
        })?;
        let bytes = oid.as_bytes().try_into().map_err(|source| {
            Error::with_source(
                ErrorKind::UnsupportedCapability,
                "unsupported object ID algorithm",
                source,
            )
        })?;
        Ok(Self(bytes))
    }

    /// Return the hash algorithm encoded by this identity.
    #[must_use]
    pub const fn algorithm(&self) -> HashAlgorithm {
        HashAlgorithm::Sha1
    }

    /// Return the complete binary identity.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 20] {
        &self.0
    }
}

impl fmt::Display for ObjectId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            write!(formatter, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl fmt::Debug for ObjectId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, formatter)
    }
}

/// A repository-relative byte path, distinct from a filesystem path.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct GitPath(Bytes);

impl GitPath {
    /// Construct the repository root for tree and archive traversal.
    #[must_use]
    pub const fn root() -> Self {
        Self(Bytes::new())
    }

    /// Validate a nonempty path, rejecting NUL, empty and parent components.
    pub fn new(bytes: impl Into<Bytes>) -> Result<Self> {
        let bytes = bytes.into();
        if bytes.contains(&0)
            || bytes
                .split(|byte| *byte == b'/')
                .any(|component| component.is_empty() || component == b"..")
        {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "invalid repository-relative Git path",
            ));
        }
        Ok(Self(bytes))
    }

    /// Return the exact repository path bytes.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    #[cfg(feature = "write")]
    pub(crate) fn owner(&self) -> Result<crab_remote_git::GitPath> {
        crab_remote_git::GitPath::new(self.0.to_vec()).map_err(crate::remote_error::remote_error)
    }

    /// Return whether the path identifies the repository root.
    #[must_use]
    pub fn is_root(&self) -> bool {
        self.0.is_empty()
    }
}

impl fmt::Debug for GitPath {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_tuple("GitPath").field(&self.0).finish()
    }
}

/// An unambiguous branch, tag or complete commit selector.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Revision(RevisionKind);

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum RevisionKind {
    Reference(String),
    Commit(ObjectId),
}

impl Revision {
    #[cfg(feature = "remote")]
    pub(crate) fn into_owner(self) -> crab_remote_git::Revision {
        match self.0 {
            RevisionKind::Reference(name) => crab_remote_git::Revision::Reference(name),
            RevisionKind::Commit(oid) => {
                crab_remote_git::Revision::Commit(gix_hash::ObjectId::Sha1(oid.0))
            }
        }
    }
    /// Select a branch by its short name, without tag-name ambiguity.
    pub fn branch(name: &str) -> Result<Self> {
        let reference = format!("refs/heads/{name}");
        gix_validate::reference::branch_name(reference.as_bytes().into()).map_err(|source| {
            Error::with_source(ErrorKind::InvalidInput, "invalid branch name", source)
        })?;
        Ok(Self(RevisionKind::Reference(reference)))
    }

    /// Select a tag by its short name, without branch-name ambiguity.
    pub fn tag(name: &str) -> Result<Self> {
        let reference = format!("refs/tags/{name}");
        gix_validate::reference::name(reference.as_bytes().into()).map_err(|source| {
            Error::with_source(ErrorKind::InvalidInput, "invalid tag name", source)
        })?;
        Ok(Self(RevisionKind::Reference(reference)))
    }

    /// Select a complete commit identity; remote reads still prove reachability.
    #[must_use]
    pub const fn commit(oid: ObjectId) -> Self {
        Self(RevisionKind::Commit(oid))
    }
}

/// A validated direct prefix or managed logical repository identity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RepositoryLocator(RepositoryLocation);

#[derive(Clone, Debug, PartialEq, Eq)]
enum RepositoryLocation {
    Direct(String),
    #[cfg(feature = "local")]
    Http(String),
    #[cfg(feature = "managed")]
    Managed {
        authority: String,
        organization: String,
        repository: String,
    },
}

impl RepositoryLocator {
    /// Return the direct repository prefix, or `None` for managed repositories.
    #[must_use]
    pub fn prefix(&self) -> Option<&str> {
        match &self.0 {
            RepositoryLocation::Direct(prefix) => Some(prefix),
            #[cfg(feature = "local")]
            RepositoryLocation::Http(_) => None,
            #[cfg(feature = "managed")]
            RepositoryLocation::Managed { .. } => None,
        }
    }

    /// Validate an HTTP(S) Git repository URL for local clone, fetch and push.
    #[cfg(feature = "local")]
    pub fn http(url: &str) -> Result<Self> {
        let authority_and_path = url
            .strip_prefix("https://")
            .or_else(|| url.strip_prefix("http://"))
            .ok_or_else(|| {
                Error::new(
                    ErrorKind::InvalidInput,
                    "local Git URL must use HTTP or HTTPS",
                )
            })?;
        let authority = authority_and_path.split('/').next().unwrap_or_default();
        if authority.is_empty()
            || authority.contains('@')
            || url
                .bytes()
                .any(|byte| byte.is_ascii_control() || byte == b' ')
            || url.contains('#')
        {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "invalid HTTP Git repository URL",
            ));
        }
        Ok(Self(RepositoryLocation::Http(url.to_owned())))
    }

    /// Validate a nonempty slash-separated repository prefix before storage access.
    pub fn new(prefix: &str) -> Result<Self> {
        if prefix.is_empty()
            || prefix.contains('\0')
            || prefix.contains('\\')
            || prefix
                .split('/')
                .any(|part| part.is_empty() || part == "." || part == "..")
        {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "invalid repository prefix",
            ));
        }
        Ok(Self(RepositoryLocation::Direct(prefix.to_owned())))
    }

    /// Validate one managed authority, organization and repository identity.
    #[cfg(feature = "managed")]
    pub fn managed(authority: &str, organization: &str, repository: &str) -> Result<Self> {
        let owner = crab_git::ManagedRepository::new(authority, organization, repository).map_err(
            |source| {
                Error::with_source(
                    ErrorKind::InvalidInput,
                    "invalid managed repository identity",
                    source,
                )
            },
        )?;
        Ok(Self(RepositoryLocation::Managed {
            authority: owner.authority,
            organization: owner.organization,
            repository: owner.repository,
        }))
    }

    /// Return the stable managed URL, or `None` for a direct prefix.
    #[must_use]
    pub fn managed_url(&self) -> Option<String> {
        match &self.0 {
            RepositoryLocation::Direct(_) => None,
            #[cfg(feature = "local")]
            RepositoryLocation::Http(_) => None,
            #[cfg(feature = "managed")]
            RepositoryLocation::Managed {
                authority,
                organization,
                repository,
            } => Some(format!("crab://{authority}/{organization}/{repository}")),
        }
    }

    #[cfg(feature = "local")]
    pub(crate) fn http_url(&self) -> Option<&str> {
        let RepositoryLocation::Http(url) = &self.0 else {
            return None;
        };
        Some(url)
    }

    #[cfg(feature = "remote")]
    pub(crate) fn direct_prefix(&self) -> Result<&str> {
        match &self.0 {
            RepositoryLocation::Direct(prefix) => Ok(prefix),
            #[cfg(feature = "local")]
            RepositoryLocation::Http(_) => Err(Error::new(
                ErrorKind::UnsupportedCapability,
                "HTTP repositories are available only through local Git workflows",
            )),
            #[cfg(feature = "managed")]
            RepositoryLocation::Managed { .. } => Err(Error::new(
                ErrorKind::UnsupportedCapability,
                "managed repository requires managed resolution",
            )),
        }
    }

    #[cfg(feature = "managed")]
    pub(crate) fn managed_owner(&self) -> Option<crab_git::ManagedRepository> {
        let RepositoryLocation::Managed {
            authority,
            organization,
            repository,
        } = &self.0
        else {
            return None;
        };
        Some(crab_git::ManagedRepository {
            authority: authority.clone(),
            organization: organization.clone(),
            repository: repository.clone(),
        })
    }

    #[cfg(all(feature = "managed", feature = "local"))]
    pub(crate) fn from_managed_owner(owner: crab_git::ManagedRepository) -> Self {
        Self(RepositoryLocation::Managed {
            authority: owner.authority,
            organization: owner.organization,
            repository: owner.repository,
        })
    }
}
