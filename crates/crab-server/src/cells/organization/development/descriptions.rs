use super::*;
use crate::cells::repository::collaboration::links::keywords::projection::Version;

fn issue_prefix(issue: Destination) -> String {
    format!("{PREFIX}/description-issues/{}/", endpoint_key(issue))
}

#[derive(Deserialize, Serialize)]
pub(crate) struct Project {
    pub pull: Destination,
    pub version: Version,
    pub issues: Vec<Destination>,
}

pub(crate) struct Replace;
impl Mutation for Replace {
    const CLASS: &'static str = CLASS;
    const ID: u32 = 87;
    type Input = Project;
    type Output = ();

    fn execute(context: &mut Transaction<'_>, input: Project) -> Result<MutationResult<()>> {
        validate(input.pull)?;
        if input.issues.len() > LIMIT {
            return Err(Error::Invalid("Too many qualified closing references"));
        }
        for issue in &input.issues {
            validate(*issue)?;
        }
        let pull_key = endpoint_key(input.pull);
        let key = format!("{PREFIX}/descriptions/{pull_key}");
        if let Some(previous) = context.document::<Project>(&key)? {
            if previous.version > input.version
                || previous.version == input.version && previous.issues == input.issues
            {
                return Ok(MutationResult::Rejected(()));
            }
            for issue in previous.issues {
                if !input.issues.contains(&issue) {
                    context.delete_document(&format!("{}{pull_key}", issue_prefix(issue)))?;
                }
            }
        }
        for issue in &input.issues {
            context.put_document(&format!("{}{pull_key}", issue_prefix(*issue)), &input.pull)?;
        }
        context.put_document(&key, &input)?;
        Ok(MutationResult::Success(()))
    }
}

pub(crate) struct Pulls;
impl Read for Pulls {
    const CLASS: &'static str = CLASS;
    const ID: u32 = 87;
    type Input = Destination;
    type Output = Vec<Destination>;

    fn execute(context: &mut Transaction<'_>, issue: Destination) -> Result<Self::Output> {
        validate(issue)?;
        Ok(context
            .documents_after::<Destination>(&issue_prefix(issue), None, 50)?
            .into_iter()
            .map(|(_, pull)| pull)
            .collect())
    }
}
