use super::*;
use crate::RepositoryAccess;

const KEY: &str = "organization/v1/repository-policy";

#[derive(Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct Policy {
    pub version: u64,
    pub member_creation: bool,
    pub baseline: Option<RepositoryAccess>,
}

impl Policy {
    pub(crate) fn can_create(self, role: Role) -> bool {
        role == Role::Owner || self.member_creation
    }
}

pub(super) fn load(context: &Transaction<'_>) -> Result<Policy> {
    Ok(context.document(KEY)?.unwrap_or_default())
}

pub(crate) struct Get;
impl Read for Get {
    const CLASS: &'static str = CLASS;
    const ID: u32 = 82;
    type Input = Caller;
    type Output = Policy;

    fn execute(context: &mut Transaction<'_>, _: Caller) -> Result<Policy> {
        load(context)
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Update {
    pub actor: Uuid,
    pub policy: Policy,
}

pub(crate) struct Set;
impl Mutation for Set {
    const CLASS: &'static str = CLASS;
    const ID: u32 = 82;
    type Input = Update;
    type Output = Option<Policy>;

    fn execute(
        context: &mut Transaction<'_>,
        Update { actor, mut policy }: Update,
    ) -> Result<MutationResult<Self::Output>> {
        if !matches!(
            policy.baseline,
            None | Some(RepositoryAccess::Read | RepositoryAccess::Write)
        ) {
            return Err(Error::Invalid(
                "Choose None, Read, or Write baseline access",
            ));
        }
        let current = load(context)?;
        if current.version != policy.version {
            return Ok(MutationResult::Rejected(None));
        }
        if current == policy {
            return Ok(MutationResult::Success(Some(current)));
        }
        policy.version = current
            .version
            .checked_add(1)
            .filter(|value| *value <= MAX_VERSION)
            .ok_or(Error::Invalid("Repository policy version limit reached"))?;
        context.put_document(KEY, &policy)?;
        advance(context, actor, Action::RepositoryPolicyChanged)?;
        Ok(MutationResult::Success(Some(policy)))
    }
}
