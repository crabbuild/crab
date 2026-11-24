use super::*;
use crate::cells::repository::{GetPull, PullRecord, PullState, result_blob};

const PREPARATION: &str = "collaboration/v1/development-branch-head-index";
const BATCH: usize = 32;

fn prefix(branch: &Branch) -> String {
    let reference = blake3::hash(branch.reference.as_bytes()).to_hex();
    format!(
        "{ROOT}/development-heads/{}/{reference}/",
        branch.repository
    )
}

pub(super) fn index(
    context: &Transaction<'_>,
    branch: &Branch,
    issue: u64,
    linked: bool,
) -> Result<()> {
    let key = format!("{}{issue:016x}", prefix(branch));
    if linked {
        context.put_document(&key, &issue)
    } else {
        context.delete_document(&key).map(drop)
    }
}

#[derive(Deserialize, Serialize)]
enum Preparation {
    Active(Option<String>),
    Pending(Option<String>),
    Ready,
}

fn number(key: &str, prefix: &str) -> Result<u64> {
    let number = key
        .strip_prefix(prefix)
        .and_then(|suffix| u64::from_str_radix(suffix, 16).ok())
        .ok_or(Error::Invalid("Development branch index key is invalid"))?;
    validate_number(number)?;
    Ok(number)
}

fn step(context: &Transaction<'_>) -> Result<bool> {
    let state = context
        .document::<Preparation>(PREPARATION)?
        .unwrap_or(Preparation::Active(None));
    let state = match state {
        Preparation::Active(after) => {
            let page = context.documents_after::<State>(STATES, after.as_deref(), BATCH)?;
            for (key, state) in &page {
                let issue = number(key, STATES)?;
                for branch in &state.branches {
                    index(context, branch, issue, true)?;
                }
            }
            if let Some((last, _)) = page.last().filter(|_| page.len() == BATCH) {
                context.put_document(PREPARATION, &Preparation::Active(Some(last.clone())))?;
                return Ok(false);
            }
            Preparation::Pending(None)
        }
        state => state,
    };
    let Preparation::Pending(after) = state else {
        return Ok(true);
    };
    let page = context.documents_after::<Uuid>(creation::ISSUES, after.as_deref(), BATCH)?;
    for (key, _) in &page {
        let issue = number(key, creation::ISSUES)?;
        let request = creation::for_issue(context, issue)?
            .ok_or(Error::Invalid("Development branch reservation disappeared"))?;
        index(context, &request.branch, issue, true)?;
    }
    if let Some((last, _)) = page.last().filter(|_| page.len() == BATCH) {
        context.put_document(PREPARATION, &Preparation::Pending(Some(last.clone())))?;
        return Ok(false);
    }
    context.put_document(PREPARATION, &Preparation::Ready)?;
    Ok(true)
}

pub(crate) struct Prepare;
impl Mutation for Prepare {
    const CLASS: &'static str = crate::cells::repository::CLASS;
    const ID: u32 = 465;
    type Input = ();
    type Output = bool;

    fn execute(context: &mut Transaction<'_>, (): ()) -> Result<MutationResult<bool>> {
        if matches!(
            context.document::<Preparation>(PREPARATION)?,
            Some(Preparation::Ready)
        ) {
            return Ok(MutationResult::Rejected(true));
        }
        Ok(MutationResult::Success(step(context)?))
    }
}

pub(in crate::cells::repository) fn create(
    context: &mut Transaction<'_>,
    pull: &PullRecord,
) -> Result<()> {
    if !pull.head_ref.starts_with("refs/heads/") {
        return Ok(());
    }
    if !step(context)? {
        return Err(Error::Invalid("Development branch index needs preparation"));
    }
    let repository = match pull.head_repository {
        Some(repository) => repository,
        None => {
            let rows = context.query(
                "SELECT repository_uuid FROM repository_identity WHERE singleton = 1",
                &[],
            )?;
            Uuid::from_slice(result_blob(&rows.rows[0], 0)?).map_err(Error::sdk)?
        }
    };
    let branch = Branch {
        repository,
        reference: pull.head_ref.clone(),
    };
    let candidates = context.documents_after::<u64>(&prefix(&branch), None, MAX_CLOSING_ISSUES)?;
    for (_, issue) in candidates {
        let mut state = load(context, issue)?;
        if let Ok(position) = state.branches.binary_search(&branch) {
            if attach(context, pull, issue)? {
                state.branches.remove(position);
                state.version = next_version(state.version)?;
                install(context, issue, &state)?;
            }
        } else {
            // Git may publish a reserved branch before its link settles.
            // Keep the PR on the exact creation request until that proof arrives.
            creation::record_pull(context, &branch, issue, pull.number)?;
        }
    }
    Ok(())
}

pub(super) fn settle(context: &mut Transaction<'_>, issue: u64, pull: u64) -> Result<bool> {
    let Some(pull) = GetPull::execute(context, pull)? else {
        return Ok(false);
    };
    attach(context, &pull, issue)
}

fn attach(context: &mut Transaction<'_>, pull: &PullRecord, issue: u64) -> Result<bool> {
    let pair = Pair {
        issue,
        pull: pull.number,
    };
    let previous = super::super::load(context, pair)?;
    if !previous.active && previous.version != 0 {
        return Ok(false);
    }
    let closes_on_merge = previous
        .active
        .then_some(previous.closes_on_merge)
        .unwrap_or(pull.state == PullState::Open);
    let result = SetLink::execute(
        context,
        Selection {
            pair,
            active: true,
            version: previous.version,
            actor: pull.author.clone(),
            closes_on_merge: Some(closes_on_merge),
        },
    )?;
    let MutationResult::Success(Outcome::Applied(mut link)) = result else {
        return Ok(false);
    };
    link.from_branch = true;
    context.put_document(&pair.key(), &link)?;
    Ok(true)
}
