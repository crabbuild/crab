use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(
    tag = "kind",
    content = "id",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub(crate) enum Address {
    Repository(Uuid),
    Directory,
    Organization(Uuid),
}

impl Address {
    pub(crate) fn name(self) -> String {
        match self {
            Self::Repository(id) => id.to_string(),
            Self::Directory => "directory".into(),
            Self::Organization(id) => id.to_string(),
        }
    }

    pub(crate) fn class(self) -> &'static str {
        match self {
            Self::Repository(_) => super::repository::CLASS,
            Self::Directory => super::directory::CLASS,
            Self::Organization(_) => super::organization::CLASS,
        }
    }

    pub(crate) fn namespace(self) -> &'static str {
        match self {
            Self::Repository(_) => super::repository::NAMESPACE,
            Self::Directory => super::directory::NAMESPACE,
            Self::Organization(_) => super::organization::NAMESPACE,
        }
    }

    pub(crate) fn path(self) -> String {
        match self {
            Self::Repository(id) => format!("/repositories/{id}"),
            Self::Directory => "/directory".into(),
            Self::Organization(id) => format!("/organizations/{id}"),
        }
    }

    pub(crate) fn parse(path: &str) -> super::Result<Self> {
        if path == "/directory" {
            return Ok(Self::Directory);
        }
        if let Some(id) = path.strip_prefix("/organizations/") {
            return Uuid::parse_str(id)
                .ok()
                .filter(|id| !id.is_nil())
                .map(Self::Organization)
                .ok_or(super::Error::Invalid("Cell address is invalid"));
        }
        let id = path
            .strip_prefix("/repositories/")
            .and_then(|value| Uuid::parse_str(value).ok())
            .filter(|id| !id.is_nil())
            .ok_or(super::Error::Invalid("Cell address is invalid"))?;
        Ok(Self::Repository(id))
    }
}

impl From<Uuid> for Address {
    fn from(id: Uuid) -> Self {
        Self::Repository(id)
    }
}
impl From<&Uuid> for Address {
    fn from(id: &Uuid) -> Self {
        Self::Repository(*id)
    }
}
