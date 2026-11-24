use super::{NoticeTarget, Reason, active_account};
use crate::cells::{
    directory::{
        CLASS, Error, IdentityKey, MAX_VERSION, Mutation, MutationResult, Read, Result,
        Transaction, accounts, advance,
    },
    repository::collaboration::SubjectKind,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub(crate) const LIMIT: usize = 15;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Kind {
    Issue,
    Pull,
    Release,
}

impl Kind {
    pub(crate) fn matches(self, target: NoticeTarget) -> bool {
        self == match target {
            NoticeTarget::Discussion(target) => match target.subject().kind {
                SubjectKind::Issue => Self::Issue,
                SubjectKind::Pull => Self::Pull,
            },
            NoticeTarget::IssueTransfer { .. } => Self::Issue,
            NoticeTarget::Release { .. } => Self::Release,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum View {
    Unread,
    All,
    Read,
    Saved,
    Done,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(try_from = "String", into = "String")]
pub(crate) enum Author {
    Me,
    Actor(Uuid),
}

impl TryFrom<String> for Author {
    type Error = &'static str;

    fn try_from(value: String) -> std::result::Result<Self, Self::Error> {
        if value == "me" {
            return Ok(Self::Me);
        }
        Uuid::parse_str(&value)
            .ok()
            .filter(|id| !id.is_nil())
            .map(Self::Actor)
            .ok_or("Author must be the current account or a valid actor ID")
    }
}

impl From<Author> for String {
    fn from(author: Author) -> Self {
        match author {
            Author::Me => "me".into(),
            Author::Actor(id) => id.to_string(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct Criteria {
    pub view: View,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repository: Option<Uuid>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub organization: Option<Uuid>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<Reason>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<Kind>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub author: Option<Author>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct Filter {
    pub id: Uuid,
    pub name: String,
    pub criteria: Criteria,
}

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Collection {
    pub revision: u64,
    pub items: Vec<Filter>,
}

fn key(actor: Uuid) -> String {
    format!("notifications/v1/filters/{actor}")
}

pub(crate) struct ListFilters;
impl Read for ListFilters {
    const CLASS: &'static str = CLASS;
    const ID: u32 = 33;
    type Input = IdentityKey;
    type Output = Collection;

    fn execute(context: &mut Transaction<'_>, identity: IdentityKey) -> Result<Self::Output> {
        let Some(actor) = active_account(context, identity)? else {
            return Ok(Collection::default());
        };
        Ok(context.document(&key(actor))?.unwrap_or_default())
    }
}

#[derive(Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum Change {
    Save { filter: Filter },
    Delete { id: Uuid },
}

impl Change {
    fn id(&self) -> Uuid {
        match self {
            Self::Save { filter } => filter.id,
            Self::Delete { id } => *id,
        }
    }

    fn validate(&self) -> Result<()> {
        if self.id().is_nil() {
            return Err(Error::Invalid("Saved notification filter ID is invalid"));
        }
        if let Self::Save { filter } = self {
            if filter.name.is_empty()
                || filter.name.len() > 128
                || filter.name.trim() != filter.name
                || filter.name.chars().any(char::is_control)
            {
                return Err(Error::Invalid("Enter a filter name of up to 128 bytes"));
            }
            if filter.criteria.repository.is_some_and(|id| id.is_nil())
                || filter.criteria.organization.is_some_and(|id| id.is_nil())
            {
                return Err(Error::Invalid("Saved notification filter scope is invalid"));
            }
        }
        Ok(())
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Update {
    pub revision: u64,
    pub change: Change,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Input {
    pub identity: IdentityKey,
    pub update: Update,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Outcome {
    Saved(Collection),
    Conflict,
    Limit,
}

pub(crate) struct UpdateFilters;
impl Mutation for UpdateFilters {
    const CLASS: &'static str = CLASS;
    const ID: u32 = 33;
    type Input = Input;
    type Output = Outcome;

    fn execute(context: &mut Transaction<'_>, input: Input) -> Result<MutationResult<Outcome>> {
        let Update { revision, change } = input.update;
        change.validate()?;
        if revision > MAX_VERSION {
            return Err(Error::Invalid(
                "Saved notification filter revision is invalid",
            ));
        }
        let (actor, _) = accounts::ensure(context, &input.identity)?;
        let key = key(actor);
        let mut collection: Collection = context.document(&key)?.unwrap_or_default();
        let index = collection
            .items
            .iter()
            .position(|item| item.id == change.id());
        // An exact retry can recover a saved result after the HTTP reply is lost.
        let applied = match (&change, index) {
            (Change::Save { filter }, Some(index)) => collection.items[index] == *filter,
            (Change::Delete { .. }, None) => true,
            _ => false,
        };
        if applied {
            return Ok(MutationResult::Success(Outcome::Saved(collection)));
        }
        if revision != collection.revision {
            return Ok(MutationResult::Rejected(Outcome::Conflict));
        }
        match (change, index) {
            (Change::Save { filter }, Some(index)) => collection.items[index] = filter,
            (Change::Save { filter }, None) => {
                if collection.items.len() >= LIMIT {
                    return Ok(MutationResult::Rejected(Outcome::Limit));
                }
                collection.items.push(filter);
            }
            (Change::Delete { .. }, Some(index)) => {
                collection.items.remove(index);
            }
            (Change::Delete { .. }, None) => {
                return Ok(MutationResult::Success(Outcome::Saved(collection)));
            }
        }
        collection.revision = revision
            .checked_add(1)
            .filter(|next| *next <= MAX_VERSION)
            .ok_or(Error::Invalid(
                "Saved notification filter revision limit reached",
            ))?;
        context.put_document(&key, &collection)?;
        advance(context)?;
        Ok(MutationResult::Success(Outcome::Saved(collection)))
    }
}
