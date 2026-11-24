use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{
    CLASS, Error, IdentityKey, Mutation, MutationResult, Notice, Read, Result, Transaction,
    active_account,
};
use crate::cells::directory::RegisterAccounts;

const HORIZON_MS: u64 = 23 * 60 * 60 * 1_000;
const LEASE_MS: u64 = 60_000;
const DUE: &str = "notifications/v1/email/due/";
const RECEIPTS: &str = "notifications/v1/email/receipts/";

pub(crate) fn valid_address(value: &str) -> bool {
    let Some((local, domain)) = value.split_once('@') else {
        return false;
    };
    value.len() <= 254
        && local.split('.').all(|segment| !segment.is_empty())
        && local.len() <= 64
        && local
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b".!#$%&'*+-/=?^_`{|}~".contains(&byte))
        && domain.contains('.')
        && domain.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        })
}

#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Preference {
    pub address: Option<String>,
    pub enabled: bool,
    pub version: u64,
}

fn preference_key(actor: Uuid) -> String {
    format!("notifications/v1/email/preference/{actor}")
}

fn preference(context: &Transaction<'_>, actor: Uuid) -> Result<Preference> {
    Ok(context
        .document(&preference_key(actor))?
        .unwrap_or_default())
}

fn save(context: &Transaction<'_>, actor: Uuid, current: &mut Preference) -> Result<()> {
    current.version = current
        .version
        .checked_add(1)
        .filter(|version| *version <= super::MAX_VERSION)
        .ok_or(Error::Invalid("Email preference version is exhausted"))?;
    context.put_document(&preference_key(actor), current)
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Binding {
    pub identity: IdentityKey,
    pub address: Option<String>,
}

pub(crate) struct BindEmail;

impl Mutation for BindEmail {
    const CLASS: &'static str = CLASS;
    const ID: u32 = 26;
    type Input = Binding;
    type Output = ();

    fn execute(context: &mut Transaction<'_>, input: Binding) -> Result<MutationResult<()>> {
        if input
            .address
            .as_deref()
            .is_some_and(|value| !valid_address(value))
        {
            return Err(Error::Invalid(
                "Verified notification email address is invalid",
            ));
        }
        let actor = match active_account(context, input.identity.clone())? {
            Some(actor) => actor,
            None if input.address.is_none() => return Ok(MutationResult::Success(())),
            None => match RegisterAccounts::execute(context, vec![input.identity])? {
                MutationResult::Success(actors) => actors.into_iter().next().ok_or(
                    Error::Invalid("Notification account could not be registered"),
                )?,
                MutationResult::Rejected(_) => {
                    return Err(Error::Invalid("Notification account was rejected"));
                }
            },
        };
        let mut current = preference(context, actor)?;
        if current.address != input.address {
            // Address changes stop delivery until the account explicitly opts
            // in again. Old jobs retain the earlier preference version.
            current.address = input.address;
            current.enabled = false;
            save(context, actor, &mut current)?;
        }
        Ok(MutationResult::Success(()))
    }
}

pub(crate) struct ReadEmail;

impl Read for ReadEmail {
    const CLASS: &'static str = CLASS;
    const ID: u32 = 26;
    type Input = IdentityKey;
    type Output = Preference;

    fn execute(context: &mut Transaction<'_>, identity: IdentityKey) -> Result<Preference> {
        active_account(context, identity)?.map_or_else(
            || Ok(Preference::default()),
            |actor| preference(context, actor),
        )
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PreferenceChange {
    pub identity: IdentityKey,
    pub enabled: bool,
    pub version: u64,
}

pub(crate) struct SetEmail;

impl Mutation for SetEmail {
    const CLASS: &'static str = CLASS;
    const ID: u32 = 27;
    type Input = PreferenceChange;
    type Output = Option<Preference>;

    fn execute(
        context: &mut Transaction<'_>,
        input: PreferenceChange,
    ) -> Result<MutationResult<Self::Output>> {
        let Some(actor) = active_account(context, input.identity)? else {
            return Ok(MutationResult::Rejected(None));
        };
        let mut current = preference(context, actor)?;
        if input.enabled && current.address.is_none() {
            return Ok(MutationResult::Rejected(None));
        }
        if current.enabled != input.enabled {
            if current.version != input.version {
                return Ok(MutationResult::Rejected(None));
            }
            current.enabled = input.enabled;
            save(context, actor, &mut current)?;
        }
        Ok(MutationResult::Success(Some(current)))
    }
}

#[derive(Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct JobId {
    pub actor: Uuid,
    pub notice: Uuid,
}

impl JobId {
    pub(crate) fn provider_key(self) -> String {
        format!("crab-notice/{}/{}", self.actor, self.notice)
    }

    fn key(self) -> String {
        format!("notifications/v1/email/jobs/{}/{}", self.actor, self.notice)
    }

    fn due(self, at: u64) -> String {
        format!("{DUE}{at:016x}/{}/{}", self.actor, self.notice)
    }

    fn receipt(self, expires: u64) -> String {
        format!("{RECEIPTS}{expires:016x}/{}/{}", self.actor, self.notice)
    }
}

#[derive(Clone, Copy, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TransferSource {
    pub offer: Uuid,
    pub created_at_ms: u64,
}

#[derive(Clone, Copy, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct InvitationSource {
    pub organization: Uuid,
    pub version: u64,
    pub created_at_ms: u64,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Job {
    pub id: JobId,
    pub repository: Uuid,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transfer: Option<TransferSource>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub invitation: Option<InvitationSource>,
    pub preference_version: u64,
    pub expires: u64,
    due: u64,
    pub attempt: u64,
}

fn now(context: &Transaction<'_>) -> Result<u64> {
    u64::try_from(context.now_ms())
        .map_err(|_| Error::Invalid("Notification email time is invalid"))
}

fn store_job(context: &Transaction<'_>, job: &Job) -> Result<()> {
    context.put_document(&job.id.key(), job)?;
    context.put_document(&job.id.due(job.due), &job.id)
}

fn delete_job(context: &Transaction<'_>, job: &Job) -> Result<()> {
    context.delete_document(&job.id.due(job.due))?;
    context.delete_document(&job.id.key())?;
    Ok(())
}

pub(super) fn enqueue(context: &Transaction<'_>, actor: Uuid, notice: &Notice) -> Result<()> {
    queue(
        context,
        JobId {
            actor,
            notice: notice.id,
        },
        notice.repository,
        notice.created_at_ms,
        None,
        None,
    )
}

fn queue(
    context: &Transaction<'_>,
    id: JobId,
    repository: Uuid,
    created_at_ms: u64,
    transfer: Option<TransferSource>,
    invitation: Option<InvitationSource>,
) -> Result<()> {
    let preference = preference(context, id.actor)?;
    if !preference.enabled && transfer.is_none() && invitation.is_none() {
        return Ok(());
    }
    let now = now(context)?;
    let expires = created_at_ms.saturating_add(HORIZON_MS);
    if expires <= now || created_at_ms > now.saturating_add(LEASE_MS) {
        return Ok(());
    }
    if context.document::<Job>(&id.key())?.is_some()
        || context.document::<bool>(&id.receipt(expires))?.is_some()
    {
        return Ok(());
    }
    if !preference.enabled {
        // Reconciliation must preserve the first delivery decision after opt-in changes.
        return context.put_document(&id.receipt(expires), &true);
    }
    store_job(
        context,
        &Job {
            id,
            repository,
            transfer,
            invitation,
            preference_version: preference.version,
            expires,
            due: now,
            attempt: 0,
        },
    )
}

pub(crate) const TRANSFER_BATCH: usize = 32;

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TransferMail {
    pub recipient: IdentityKey,
    pub repository: Uuid,
    pub source: TransferSource,
}

pub(crate) struct QueueTransferMail;

impl Mutation for QueueTransferMail {
    const CLASS: &'static str = CLASS;
    const ID: u32 = 30;
    type Input = Vec<TransferMail>;
    type Output = ();

    fn execute(context: &mut Transaction<'_>, input: Self::Input) -> Result<MutationResult<()>> {
        if input.len() > TRANSFER_BATCH {
            return Err(Error::Invalid("Too many transfer emails in one batch"));
        }
        for item in input {
            if item.repository.is_nil() || item.source.offer.is_nil() {
                return Err(Error::Invalid("Invalid transfer email source"));
            }
            let Some(actor) = active_account(context, item.recipient)? else {
                continue;
            };
            // The proposal UUID comes from the client. Bind it to this source
            // and lifetime without colliding with an ordinary inbox notice.
            let mut hash = blake3::Hasher::new_derive_key("crab transfer email v1");
            hash.update(item.repository.as_bytes());
            hash.update(item.source.offer.as_bytes());
            hash.update(&item.source.created_at_ms.to_be_bytes());
            let mut bytes = [0; 16];
            bytes.copy_from_slice(&hash.finalize().as_bytes()[..16]);
            let id = JobId {
                actor,
                notice: Uuid::from_bytes(bytes),
            };
            queue(
                context,
                id,
                item.repository,
                item.source.created_at_ms,
                Some(item.source),
                None,
            )?;
        }
        Ok(MutationResult::Success(()))
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct InvitationMail {
    pub actor: Uuid,
    pub source: InvitationSource,
}

pub(crate) struct QueueInvitationMail;
impl Mutation for QueueInvitationMail {
    const CLASS: &'static str = CLASS;
    const ID: u32 = 81;
    type Input = Vec<InvitationMail>;
    type Output = ();

    fn execute(context: &mut Transaction<'_>, input: Self::Input) -> Result<MutationResult<()>> {
        if input.len() > super::super::organizations::PAGE_SIZE {
            return Err(Error::Invalid("Invalid invitation email batch"));
        }
        for InvitationMail { actor, source } in input {
            if actor.is_nil() || source.organization.is_nil() || source.version == 0 {
                return Err(Error::Invalid("Invalid invitation email source"));
            }
            let mut hash = blake3::Hasher::new_derive_key("crab organization invitation email v1");
            hash.update(source.organization.as_bytes());
            hash.update(&source.version.to_be_bytes());
            hash.update(&source.created_at_ms.to_be_bytes());
            let mut bytes = [0; 16];
            bytes.copy_from_slice(&hash.finalize().as_bytes()[..16]);
            queue(
                context,
                JobId {
                    actor,
                    notice: Uuid::from_bytes(bytes),
                },
                source.organization,
                source.created_at_ms,
                None,
                Some(source),
            )?;
        }
        Ok(MutationResult::Success(()))
    }
}

pub(crate) struct MailPending;

impl Read for MailPending {
    const CLASS: &'static str = CLASS;
    const ID: u32 = 27;
    type Input = ();
    type Output = bool;

    fn execute(context: &mut Transaction<'_>, (): ()) -> Result<bool> {
        let now = now(context)?;
        // A future lease is also due for cleanup if its delivery window ended.
        if let Some((_, id)) = context
            .documents_after::<JobId>(DUE, None, 1)?
            .into_iter()
            .next()
        {
            let job: Job = context
                .document(&id.key())?
                .ok_or(Error::Invalid("Notification email job is missing"))?;
            if job.due <= now || job.expires <= now {
                return Ok(true);
            }
        }
        let limit = format!("{RECEIPTS}{now:016x}/~");
        Ok(!context
            .documents::<bool>(RECEIPTS, Some(&limit), 1)?
            .is_empty())
    }
}

pub(crate) struct ClaimMail;

impl Mutation for ClaimMail {
    const CLASS: &'static str = CLASS;
    const ID: u32 = 28;
    type Input = ();
    type Output = Option<Job>;

    fn execute(context: &mut Transaction<'_>, (): ()) -> Result<MutationResult<Self::Output>> {
        let now = now(context)?;
        let limit = format!("{RECEIPTS}{now:016x}/~");
        for (key, _) in context.documents::<bool>(RECEIPTS, Some(&limit), 16)? {
            context.delete_document(&key)?;
        }
        for (key, id) in context.documents_after::<JobId>(DUE, None, 16)? {
            let mut job: Job = context
                .document(&id.key())?
                .filter(|job: &Job| job.id == id && id.due(job.due) == key)
                .ok_or(Error::Invalid("Notification email job is inconsistent"))?;
            if job.expires <= now {
                delete_job(context, &job)?;
                continue;
            }
            if job.due > now {
                break;
            }
            context.delete_document(&key)?;
            job.attempt = job
                .attempt
                .checked_add(1)
                .ok_or(Error::Invalid("Email attempt limit reached"))?;
            job.due = now.saturating_add(LEASE_MS);
            store_job(context, &job)?;
            return Ok(MutationResult::Success(Some(job)));
        }
        Ok(MutationResult::Success(None))
    }
}

#[derive(Clone, Copy, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct MailAttempt {
    pub id: JobId,
    pub attempt: u64,
}

pub(crate) struct MailDestination;

impl Read for MailDestination {
    const CLASS: &'static str = CLASS;
    const ID: u32 = 28;
    type Input = MailAttempt;
    type Output = Option<String>;

    fn execute(context: &mut Transaction<'_>, input: MailAttempt) -> Result<Option<String>> {
        let Some(job) = context.document::<Job>(&input.id.key())? else {
            return Ok(None);
        };
        let now = now(context)?;
        if job.attempt != input.attempt || job.due <= now || job.expires <= now {
            return Ok(None);
        }
        let preference = preference(context, job.id.actor)?;
        Ok(preference
            .address
            .filter(|_| preference.enabled && preference.version == job.preference_version))
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct MailFinish {
    pub attempt: MailAttempt,
    pub retry_after_ms: Option<u64>,
}

pub(crate) struct FinishMail;

impl Mutation for FinishMail {
    const CLASS: &'static str = CLASS;
    const ID: u32 = 29;
    type Input = MailFinish;
    type Output = ();

    fn execute(context: &mut Transaction<'_>, input: MailFinish) -> Result<MutationResult<()>> {
        let Some(mut job) = context
            .document::<Job>(&input.attempt.id.key())?
            .filter(|job: &Job| job.attempt == input.attempt.attempt)
        else {
            return Ok(MutationResult::Success(()));
        };
        let now = now(context)?;
        delete_job(context, &job)?;
        if now < job.expires {
            if let Some(retry_after_ms) = input.retry_after_ms {
                job.due = now
                    .saturating_add(retry_after_ms.max(LEASE_MS))
                    .min(job.expires);
                store_job(context, &job)?;
            } else {
                context.put_document(&job.id.receipt(job.expires), &true)?;
            }
        }
        Ok(MutationResult::Success(()))
    }
}
