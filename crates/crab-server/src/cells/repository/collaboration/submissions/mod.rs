use super::SubjectKind;
use crate::cells::{
    Result,
    application::{MutationResult, Transaction},
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};

pub(crate) mod preparation;

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Kind {
    Issue,
    Pull,
    Bulk,
    Pins,
}
impl From<SubjectKind> for Kind {
    fn from(kind: SubjectKind) -> Self {
        match kind {
            SubjectKind::Issue => Self::Issue,
            SubjectKind::Pull => Self::Pull,
        }
    }
}
impl Kind {
    pub(crate) fn prefix(self) -> &'static str {
        match self {
            Self::Issue => "collaboration/v1/state-submissions/issue/",
            Self::Pull => "collaboration/v1/state-submissions/pull/",
            Self::Bulk => "collaboration/v1/bulk-submissions/",
            Self::Pins => "issues/v1/pins/submissions/",
        }
    }
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Saved<T> {
    digest: String,
    resolution: T,
}

// None means this identity already belongs to another payload. Domain policy
// stays with each caller; its rejected writes roll back before the decision commits.
pub(crate) fn decide<T: Serialize + DeserializeOwned>(
    context: &mut Transaction<'_>,
    kind: Kind,
    key: &str,
    digest: String,
    apply: impl FnOnce(&mut Transaction<'_>) -> Result<MutationResult<T>>,
) -> Result<Option<T>> {
    preparation::ensure_ready(context, kind)?;
    if let Some(saved) = context.document::<Saved<T>>(key)? {
        return Ok((saved.digest == digest).then_some(saved.resolution));
    }
    let resolution = match context.apply(apply)? {
        MutationResult::Success(value) | MutationResult::Rejected(value) => value,
    };
    let saved = Saved { digest, resolution };
    context.put_document(key, &saved)?;
    Ok(Some(saved.resolution))
}
pub(in crate::cells::repository) fn initialize(context: &Transaction<'_>) -> Result<()> {
    for kind in [Kind::Issue, Kind::Pull, Kind::Bulk, Kind::Pins] {
        preparation::initialize(context, kind)?;
    }
    Ok(())
}
