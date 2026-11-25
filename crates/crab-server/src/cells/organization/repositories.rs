use super::*;
use crate::{
    catalog::visibility::Visibility,
    config::{DEFAULT_REPOSITORY_BRANCH, default_repository_branch},
};

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Creation {
    pub actor: Uuid,
    pub request_id: Uuid,
    pub name: String,
    pub description: String,
    pub visibility: Visibility,
}

#[derive(Deserialize, Serialize)]
pub(crate) enum Authorized {
    Accepted,
    Conflict,
}

pub(crate) struct AuthorizeCreation;
impl Mutation for AuthorizeCreation {
    const CLASS: &'static str = CLASS;
    const ID: u32 = 40;
    type Input = Creation;
    type Output = CreationOutcome;

    fn execute(
        context: &mut Transaction<'_>,
        input: Creation,
    ) -> Result<MutationResult<CreationOutcome>> {
        if input.request_id.is_nil() || !valid_creation(&input.name, &input.description) {
            return Err(Error::Invalid("invalid repository creation request"));
        }
        let current = member(context, input.actor)?.ok_or(Error::AccessDenied)?;
        let policy = repository_policy::load(context)?;
        if !policy
            .creation_visibilities(current.role)
            .contains(&input.visibility)
        {
            return Ok(MutationResult::Rejected(
                CreationOutcome::VisibilityRestricted,
            ));
        }
        let key = format!("organization/v1/repository-creations/{}", input.request_id);
        let mut digest = blake3::Hasher::new();
        digest.update(&serde_json::to_vec(&(
            input.actor,
            input.name,
            input.description,
        ))?);
        // Private is the original request domain. A public reservation must
        // have a different digest even if all other fields match.
        if input.visibility.is_public() {
            digest.update(b"\0public");
        }
        let digest = *digest.finalize().as_bytes();
        // Keep the original ticket digest. A later promotion must not change
        // which direct grant an already authorized reservation carries.
        let creator = format!("{key}/member-creator");
        let branch = format!("{key}/default-branch");
        let (grant_creator, default_branch) =
            if let Some(existing) = context.document::<[u8; 32]>(&key)? {
                if existing != digest {
                    return Ok(MutationResult::Rejected(CreationOutcome::Conflict));
                }
                (
                    context.document::<bool>(&creator)?.unwrap_or(false),
                    context
                        .document::<String>(&branch)?
                        .unwrap_or_else(default_repository_branch),
                )
            } else {
                let member_creating = current.role == Role::Member;
                context.put_document(&key, &digest)?;
                if member_creating {
                    context.put_document(&creator, &true)?;
                }
                // The authorized branch must survive retries across policy changes.
                let default_branch = policy.default_branch;
                if default_branch != DEFAULT_REPOSITORY_BRANCH {
                    context.put_document(&branch, &default_branch)?;
                }
                (member_creating, default_branch)
            };
        Ok(MutationResult::Success(CreationOutcome::Accepted {
            grant_creator,
            default_branch,
        }))
    }
}

#[derive(Deserialize, Serialize)]
pub(crate) enum CreationOutcome {
    Accepted {
        grant_creator: bool,
        default_branch: String,
    },
    Conflict,
    VisibilityRestricted,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ImportPublication {
    pub actor: Uuid,
    pub repository: Uuid,
    pub snapshot: [u8; 32],
    pub expected_policy: u64,
}

pub(crate) struct AuthorizeImportPublication;
impl Mutation for AuthorizeImportPublication {
    const CLASS: &'static str = CLASS;
    const ID: u32 = 41;
    type Input = ImportPublication;
    type Output = Authorized;

    fn execute(
        context: &mut Transaction<'_>,
        input: ImportPublication,
    ) -> Result<MutationResult<Authorized>> {
        if input.repository.is_nil() {
            return Err(Error::Invalid("invalid import destination"));
        }
        if input.expected_policy != super::rulesets::version(context)? {
            return Err(Error::Invalid(
                "organization rules changed during import publication",
            ));
        }
        // Another current owner can recover this decision for the exact same
        // repository and verified refs after the original owner leaves.
        let key = format!("organization/v1/import-publications/{}", input.repository);
        authorize_ticket(context, &key, input.snapshot)
    }
}

fn authorize_ticket(
    context: &mut Transaction<'_>,
    key: &str,
    digest: [u8; 32],
) -> Result<MutationResult<Authorized>> {
    if let Some(existing) = context.document::<[u8; 32]>(key)? {
        return Ok(if existing == digest {
            MutationResult::Success(Authorized::Accepted)
        } else {
            MutationResult::Rejected(Authorized::Conflict)
        });
    }
    // The dispatcher checks current owner authority in this transaction,
    // before the durable decision or a replay can authorize publication.
    context.put_document(key, &digest)?;
    Ok(MutationResult::Success(Authorized::Accepted))
}

pub(crate) fn valid_creation(name: &str, description: &str) -> bool {
    crate::config::valid_repository_component(name)
        && crate::config::valid_repository_description(description)
}

pub(crate) struct Owners;
impl Read for Owners {
    const CLASS: &'static str = CLASS;
    const ID: u32 = 40;
    type Input = ();
    type Output = Vec<Uuid>;

    fn execute(context: &mut Transaction<'_>, _: ()) -> Result<Self::Output> {
        context.query("SELECT actor FROM organization_members WHERE role = 'owner' AND active = 1 ORDER BY actor", &[])?
            .rows.iter().map(|row| {
                let [id] = row.as_slice() else {
                    return Err(Error::Invalid("invalid organization owner record"));
                };
                uuid(id)
            }).collect()
    }
}
