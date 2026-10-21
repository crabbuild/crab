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
}

impl Address {
    pub(crate) fn name(self) -> String {
        match self {
            Self::Repository(id) => id.to_string(),
            Self::Directory => "directory".into(),
        }
    }

    pub(crate) fn class(self) -> &'static str {
        match self {
            Self::Repository(_) => super::repository::CLASS,
            Self::Directory => super::directory::CLASS,
        }
    }

    pub(crate) fn namespace(self) -> &'static str {
        match self {
            Self::Repository(_) => super::repository::NAMESPACE,
            Self::Directory => super::directory::NAMESPACE,
        }
    }

    pub(crate) fn path(self) -> String {
        match self {
            Self::Repository(id) => format!("/repositories/{id}"),
            Self::Directory => "/directory".into(),
        }
    }

    pub(crate) fn parse(path: &str) -> super::Result<Self> {
        if path == "/directory" {
            return Ok(Self::Directory);
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
