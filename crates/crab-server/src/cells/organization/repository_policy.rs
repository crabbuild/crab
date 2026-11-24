use super::*;
use crate::{
    RepositoryAccess,
    config::{default_repository_branch, valid_repository_branch},
};

const KEY: &str = "organization/v1/repository-policy";

#[derive(Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct Policy {
    pub version: u64,
    pub member_creation: bool,
    #[serde(default)]
    pub member_transfer: bool,
    #[serde(default)]
    pub member_deletion: bool,
    #[serde(default)]
    pub private_forks: bool,
    #[serde(default)]
    pub member_visibility: bool,
    pub baseline: Option<RepositoryAccess>,
    #[serde(default = "default_repository_branch")]
    pub default_branch: String,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            version: 0,
            member_creation: false,
            member_transfer: false,
            member_deletion: false,
            private_forks: false,
            member_visibility: false,
            baseline: None,
            default_branch: default_repository_branch(),
        }
    }
}

impl Policy {
    pub(crate) fn can_create(&self, role: Role) -> bool {
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
        if !valid_repository_branch(&policy.default_branch) {
            return Err(Error::Invalid("Enter a valid short Git branch name"));
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
