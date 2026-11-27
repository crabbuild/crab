use super::*;

const PREFIX: &str = "directory/v1/organization-candidates/";
const ORG_HANDLE_PREFIX: &str = "directory/v1/org-handle/";
const ORG_ID_HANDLE_PREFIX: &str = "directory/v1/org-id-handle/";
pub(crate) const PAGE_SIZE: usize = 40;

fn org_handle_key(handle: &str) -> String {
    format!("{ORG_HANDLE_PREFIX}{handle}")
}

fn org_id_handle_key(id: Uuid) -> String {
    format!("{ORG_ID_HANDLE_PREFIX}{}", id.simple())
}

pub(super) fn handle_owner(context: &Transaction<'_>, handle: &str) -> Result<Option<Uuid>> {
    context.document(&org_handle_key(handle))
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct HandleClaimInput {
    pub organization: Uuid,
    pub handle: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) enum HandleClaimOutcome {
    Claimed,
    Taken,
}

pub(crate) struct ClaimOrganizationHandle;
impl Mutation for ClaimOrganizationHandle {
    const CLASS: &'static str = CLASS;
    const ID: u32 = 83;
    type Input = HandleClaimInput;
    type Output = HandleClaimOutcome;

    fn execute(
        context: &mut Transaction<'_>,
        input: Self::Input,
    ) -> Result<MutationResult<Self::Output>> {
        if input.organization.is_nil() {
            return Err(Error::Invalid("Organization ID is invalid"));
        }
        let handle = input.handle.to_ascii_lowercase();
        if !crate::catalog::valid_organization_handle(&handle) {
            return Err(Error::Invalid(
                "Organization handle must contain 1–39 lowercase letters, digits or single hyphens and cannot use a reserved name",
            ));
        }
        let current = context.document::<String>(&org_id_handle_key(input.organization))?;
        if current.as_deref() == Some(handle.as_str()) {
            return Ok(MutationResult::Success(HandleClaimOutcome::Claimed));
        }
        let taken_by_user = !context
            .query(
                "SELECT 1 FROM profiles WHERE handle = ?",
                &[SqlValue::Text(handle.clone())],
            )?
            .rows
            .is_empty();
        if taken_by_user {
            return Ok(MutationResult::Rejected(HandleClaimOutcome::Taken));
        }
        if let Some(existing) = handle_owner(context, &handle)?
            && existing != input.organization
        {
            return Ok(MutationResult::Rejected(HandleClaimOutcome::Taken));
        }
        if let Some(previous) = current
            && previous != handle
        {
            let _ = context.delete_document(&org_handle_key(&previous))?;
        }
        context.put_document(&org_handle_key(&handle), &input.organization)?;
        context.put_document(&org_id_handle_key(input.organization), &handle)?;
        advance(context)?;
        Ok(MutationResult::Success(HandleClaimOutcome::Claimed))
    }
}

pub(crate) struct ResolveOrganizationHandle;
impl Read for ResolveOrganizationHandle {
    const CLASS: &'static str = CLASS;
    const ID: u32 = 84;
    type Input = String;
    type Output = Option<Uuid>;

    fn execute(context: &mut Transaction<'_>, handle: String) -> Result<Option<Uuid>> {
        let handle = handle.trim().to_ascii_lowercase();
        if handle.is_empty() {
            return Ok(None);
        }
        handle_owner(context, &handle)
    }
}

pub(crate) struct GetOrganizationHandle;
impl Read for GetOrganizationHandle {
    const CLASS: &'static str = CLASS;
    const ID: u32 = 85;
    type Input = Uuid;
    type Output = Option<String>;

    fn execute(context: &mut Transaction<'_>, organization: Uuid) -> Result<Option<String>> {
        if organization.is_nil() {
            return Err(Error::Invalid("Organization ID is invalid"));
        }
        context.document(&org_id_handle_key(organization))
    }
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Candidate {
    pub actor: Uuid,
    pub organization: Uuid,
}

fn key(candidate: &Candidate) -> String {
    format!(
        "{PREFIX}{}/{}",
        candidate.actor.simple(),
        candidate.position()
    )
}

impl Candidate {
    pub(crate) fn position(&self) -> String {
        let digest = blake3::derive_key(
            "crab organization candidate v1",
            &[
                self.actor.as_bytes().as_slice(),
                self.organization.as_bytes(),
            ]
            .concat(),
        );
        blake3::Hash::from_bytes(digest).to_hex().to_string()
    }
}

pub(crate) struct RecordCandidate;
impl Mutation for RecordCandidate {
    const CLASS: &'static str = CLASS;
    const ID: u32 = 80;
    type Input = Candidate;
    type Output = ();

    fn execute(
        context: &mut Transaction<'_>,
        candidate: Self::Input,
    ) -> Result<MutationResult<()>> {
        if candidate.actor.is_nil() || candidate.organization.is_nil() {
            return Err(Error::Invalid("Invalid organization candidate"));
        }
        let key = key(&candidate);
        if context.document::<Candidate>(&key)?.is_none() {
            context.put_document(&key, &candidate)?;
        }
        Ok(MutationResult::Success(()))
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Query {
    pub actor: Option<Uuid>,
    pub after: Option<String>,
}

#[derive(Deserialize, Serialize)]
pub(crate) struct Page {
    pub items: Vec<Candidate>,
    pub next: Option<String>,
}

pub(crate) struct Candidates;
impl Read for Candidates {
    const CLASS: &'static str = CLASS;
    const ID: u32 = 80;
    type Input = Query;
    type Output = Page;

    fn execute(context: &mut Transaction<'_>, input: Self::Input) -> Result<Page> {
        let prefix = input.actor.map_or_else(
            || PREFIX.to_owned(),
            |actor| format!("{PREFIX}{}/", actor.simple()),
        );
        let after = input
            .after
            .map(|cursor| {
                let valid = if input.actor.is_some() {
                    blake3::Hash::from_hex(&cursor).is_ok()
                } else {
                    cursor.split_once('/').is_some_and(|(actor, digest)| {
                        actor.len() == 32
                            && Uuid::parse_str(actor).is_ok()
                            && blake3::Hash::from_hex(digest).is_ok()
                    })
                };
                if !valid {
                    return Err(Error::Invalid("Invalid organization continuation"));
                }
                Ok(format!("{prefix}{cursor}"))
            })
            .transpose()?;
        let mut rows =
            context.documents_after::<Candidate>(&prefix, after.as_deref(), PAGE_SIZE + 1)?;
        let next = if rows.len() > PAGE_SIZE {
            rows.pop();
            rows.last()
                .and_then(|(key, _)| key.strip_prefix(&prefix))
                .map(str::to_owned)
        } else {
            None
        };
        Ok(Page {
            items: rows.into_iter().map(|(_, entry)| entry).collect(),
            next,
        })
    }
}
