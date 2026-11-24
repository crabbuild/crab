use super::*;
use crate::cells::{Error, Result};
use uuid::Uuid;

const MAX_BRANCHES: usize = 10;
const STATES: &str = "collaboration/v1/links/development-branches/";
pub(crate) mod creation;
pub(crate) mod promotion;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(deny_unknown_fields)]
pub(crate) struct Branch {
    pub repository: Uuid,
    pub reference: String,
}

impl Branch {
    pub(crate) fn validate(&self) -> Result<()> {
        let valid = self
            .reference
            .strip_prefix("refs/heads/")
            .is_some_and(|name| {
                !name.is_empty() && name.len() <= 255 && !name.chars().any(char::is_control)
            });
        if self.repository.is_nil()
            || !valid
            || crab_git::validate_push_refname(&self.reference).is_err()
        {
            return Err(Error::Invalid("Invalid development branch"));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct State {
    pub version: u64,
    pub branches: Vec<Branch>,
}

impl State {
    pub(crate) fn is_empty(&self) -> bool {
        self.version == 0
    }

    pub(in crate::cells::repository) fn validate(&self) -> Result<()> {
        if self.version > super::super::super::MAX_NUMBER
            || (self.version == 0 && !self.branches.is_empty())
            || self.branches.len() > MAX_BRANCHES
            || self.branches.windows(2).any(|pair| pair[0] >= pair[1])
        {
            return Err(Error::Invalid("Invalid issue development branch state"));
        }
        self.branches.iter().try_for_each(Branch::validate)
    }
}

fn key(issue: u64) -> String {
    format!("{STATES}{issue:016x}")
}

pub(in crate::cells::repository) fn load(context: &Transaction<'_>, issue: u64) -> Result<State> {
    Ok(context.document(&key(issue))?.unwrap_or_default())
}

pub(in crate::cells::repository) fn install(
    context: &Transaction<'_>,
    issue: u64,
    state: &State,
) -> Result<()> {
    state.validate()?;
    if !state.is_empty() {
        let previous = load(context, issue)?;
        for branch in &previous.branches {
            if state.branches.binary_search(branch).is_err() {
                promotion::index(context, branch, issue, false)?;
            }
        }
        for branch in &state.branches {
            if previous.branches.binary_search(branch).is_err() {
                promotion::index(context, branch, issue, true)?;
            }
        }
        context.put_document(&key(issue), state)?;
    }
    Ok(())
}

pub(in crate::cells::repository) fn retire(context: &Transaction<'_>, issue: u64) -> Result<()> {
    for branch in load(context, issue)?.branches {
        promotion::index(context, &branch, issue, false)?;
    }
    context.delete_document(&key(issue)).map(drop)
}

fn exists(context: &Transaction<'_>, number: u64) -> Result<bool> {
    Ok(Subject {
        kind: SubjectKind::Issue,
        number,
    }
    .closed(context)?
    .is_some())
}

pub(crate) struct Get;
#[derive(Deserialize, Serialize)]
pub(crate) struct View {
    pub state: State,
    pub pending: Option<creation::Request>,
}
impl Read for Get {
    const CLASS: &'static str = super::super::super::CLASS;
    const ID: u32 = 462;
    type Input = u64;
    type Output = Option<View>;

    fn execute(context: &mut Transaction<'_>, issue: u64) -> Result<Option<View>> {
        if !exists(context, issue)? {
            return Ok(None);
        }
        Ok(Some(View {
            state: load(context, issue)?,
            pending: creation::for_issue(context, issue)?,
        }))
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Edit {
    pub issue: u64,
    pub branch: Branch,
    pub active: bool,
    pub version: u64,
}

#[derive(Deserialize, Serialize)]
pub(crate) enum Decision {
    Applied(u64),
    Archived,
    Missing,
    Conflict,
    Limit,
}

pub(crate) struct Set;
impl Mutation for Set {
    const CLASS: &'static str = super::super::super::CLASS;
    const ID: u32 = 462;
    type Input = Edit;
    type Output = Decision;

    fn execute(context: &mut Transaction<'_>, input: Edit) -> Result<MutationResult<Decision>> {
        input.branch.validate()?;
        if settings::archived(context)? {
            return Ok(MutationResult::Rejected(Decision::Archived));
        }
        if !exists(context, input.issue)? {
            return Ok(MutationResult::Rejected(Decision::Missing));
        }
        let mut state = load(context, input.issue)?;
        let position = state.branches.binary_search(&input.branch);
        if position.is_ok() == input.active
            && (input.version == state.version
                || input.version.checked_add(1) == Some(state.version))
        {
            return Ok(MutationResult::Success(Decision::Applied(state.version)));
        }
        if input.version != state.version {
            return Ok(MutationResult::Rejected(Decision::Conflict));
        }
        match position {
            Ok(index) => {
                state.branches.remove(index);
            }
            Err(_) if creation::pending(context, input.issue)?.is_some() => {
                return Ok(MutationResult::Rejected(Decision::Conflict));
            }
            Err(index) if state.branches.len() < MAX_BRANCHES => {
                state.branches.insert(index, input.branch);
            }
            Err(_) => return Ok(MutationResult::Rejected(Decision::Limit)),
        }
        state.version = next_version(state.version)?;
        // An issue transfer binds the repository revision to this snapshot.
        install(context, input.issue, &state)?;
        advance_revision(context)?;
        Ok(MutationResult::Success(Decision::Applied(state.version)))
    }
}
