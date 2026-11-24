use serde::{Deserialize, Serialize};

use crate::cells::{
    Error, Result,
    application::{Mutation, MutationResult, Read, Transaction},
    directory::IdentityKey,
};

const WINDOW_MS: i64 = 24 * 60 * 60 * 1000;
const MAX_PUSHES: usize = 4;
pub(crate) const MAX_BRANCHES: usize = 8;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Branch {
    pub name: String,
    pub oid: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Intent {
    pub plan: String,
    pub branches: Vec<Branch>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RecordInput {
    pub actor: IdentityKey,
    pub intent: Intent,
}

#[derive(Serialize, Deserialize)]
struct Entry {
    intent: Intent,
    recorded_at: i64,
}

fn key(actor: &IdentityKey) -> Result<String> {
    actor.validate()?;
    let hash = blake3::hash(&serde_json::to_vec(actor)?);
    Ok(format!("recent-pushes/v1/{hash}"))
}

fn retained(context: &Transaction<'_>, key: &str) -> Result<Vec<Entry>> {
    let mut entries: Vec<Entry> = context.document(key)?.unwrap_or_default();
    let earliest = context.now_ms().saturating_sub(WINDOW_MS);
    entries.retain(|entry| entry.recorded_at >= earliest);
    Ok(entries)
}

pub(crate) struct Record;
impl Mutation for Record {
    const CLASS: &'static str = super::CLASS;
    const ID: u32 = 452;
    type Input = RecordInput;
    type Output = ();

    fn execute(context: &mut Transaction<'_>, input: RecordInput) -> Result<MutationResult<()>> {
        if input.intent.plan.len() != 64
            || blake3::Hash::from_hex(&input.intent.plan).is_err()
            || input.intent.branches.is_empty()
            || input.intent.branches.len() > MAX_BRANCHES
            || input.intent.branches.iter().any(|branch| {
                !branch.name.starts_with("refs/heads/")
                    || branch.name.len() > 1_024
                    || crab_git::validate_push_refname(&branch.name).is_err()
                    || gix_hash::ObjectId::from_hex(branch.oid.as_bytes()).is_err()
                    || branch.oid.len() != 40
            })
        {
            return Err(Error::Invalid("Recent push intent is invalid"));
        }
        let key = key(&input.actor)?;
        let mut entries = retained(context, &key)?;
        if !entries
            .iter()
            .any(|entry| entry.intent.plan == input.intent.plan)
        {
            entries.insert(
                0,
                Entry {
                    intent: input.intent,
                    recorded_at: context.now_ms(),
                },
            );
            entries.truncate(MAX_PUSHES);
            context.put_document(&key, &entries)?;
        }
        Ok(MutationResult::Success(()))
    }
}

pub(crate) struct List;
impl Read for List {
    const CLASS: &'static str = super::CLASS;
    const ID: u32 = 452;
    type Input = IdentityKey;
    type Output = Vec<Intent>;

    fn execute(context: &mut Transaction<'_>, actor: IdentityKey) -> Result<Self::Output> {
        Ok(retained(context, &key(&actor)?)?
            .into_iter()
            .map(|entry| entry.intent)
            .collect())
    }
}
