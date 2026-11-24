use super::*;
use crate::cells::repository::collaboration::links::branches::Branch;

const CANDIDATE_PAGE: usize = 32;

fn prefix(branch: &Branch) -> Result<String> {
    branch.validate()?;
    let reference = blake3::hash(branch.reference.as_bytes()).to_hex();
    Ok(format!(
        "{PREFIX}/branch-issues/{}/{reference}/",
        branch.repository
    ))
}

#[derive(Deserialize, Serialize)]
pub(crate) struct TrackInput {
    pub rows: Vec<(Destination, Branch)>,
    pub active: bool,
}

pub(crate) struct Track;
impl Mutation for Track {
    const CLASS: &'static str = CLASS;
    const ID: u32 = 88;
    type Input = TrackInput;
    type Output = ();

    fn execute(context: &mut Transaction<'_>, input: TrackInput) -> Result<MutationResult<()>> {
        if input.rows.len() > 160 {
            return Err(Error::Invalid("Too many development branch candidates"));
        }
        for (issue, branch) in input.rows {
            validate(issue)?;
            let key = format!("{}{}", prefix(&branch)?, endpoint_key(issue));
            if input.active {
                context.put_document(&key, &issue)?;
            } else {
                context.delete_document(&key)?;
            }
        }
        Ok(MutationResult::Success(()))
    }
}

pub(crate) struct Candidates;
impl Read for Candidates {
    const CLASS: &'static str = CLASS;
    const ID: u32 = 88;
    type Input = (Branch, Option<Destination>);
    type Output = (Vec<Destination>, bool);

    fn execute(
        context: &mut Transaction<'_>,
        (branch, after): Self::Input,
    ) -> Result<Self::Output> {
        let prefix = prefix(&branch)?;
        let after = after.map(|issue| format!("{prefix}{}", endpoint_key(issue)));
        let mut rows = context.documents_after::<Destination>(
            &prefix,
            after.as_deref(),
            CANDIDATE_PAGE + 1,
        )?;
        let more = rows.len() > CANDIDATE_PAGE;
        rows.truncate(CANDIDATE_PAGE);
        Ok((rows.into_iter().map(|(_, issue)| issue).collect(), more))
    }
}

pub(crate) struct Promote;
impl Mutation for Promote {
    const CLASS: &'static str = CLASS;
    const ID: u32 = 89;
    type Input = Pair;
    type Output = bool;

    fn execute(context: &mut Transaction<'_>, pair: Pair) -> Result<MutationResult<bool>> {
        let prior = Get::execute(context, pair)?;
        if prior.version != 0 {
            // An existing manual decision owns the pair, including a removal.
            return Ok(MutationResult::Rejected(prior.active));
        }
        let edit = Edit {
            pair,
            active: true,
            version: 0,
            closes_on_merge: Some(true),
        };
        Ok(match set(context, edit, true)? {
            MutationResult::Success(Decision::Applied(_)) => MutationResult::Success(true),
            _ => MutationResult::Rejected(false),
        })
    }
}
