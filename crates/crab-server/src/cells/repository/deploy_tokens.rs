use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{collaboration::next_version, *};

const KEY: &str = "automation/v1/deploy-tokens";
const LIMIT: usize = 32;
const DAY_SECONDS: u64 = 24 * 60 * 60;

#[derive(Clone, Deserialize, Serialize)]
pub(crate) struct Summary {
    pub id: Uuid,
    pub name: String,
    pub created_at: u64,
    pub expires_at: u64,
    pub creator_name: String,
}

#[derive(Deserialize, Serialize)]
struct Token {
    summary: Summary,
    digest: [u8; 32],
}

#[derive(Default, Deserialize, Serialize)]
struct Ledger {
    version: u64,
    tokens: Vec<Token>,
}

fn ledger(context: &Transaction<'_>) -> crate::cells::Result<Ledger> {
    Ok(context.document(KEY)?.unwrap_or_default())
}

#[derive(Deserialize, Serialize)]
pub(crate) struct Snapshot {
    pub version: u64,
    pub tokens: Vec<Summary>,
}

pub(crate) struct List;
impl Read for List {
    const CLASS: &'static str = CLASS;
    const ID: u32 = 435;
    type Input = ();
    type Output = Snapshot;

    fn execute(context: &mut Transaction<'_>, (): ()) -> crate::cells::Result<Snapshot> {
        let state = ledger(context)?;
        let now = timestamp(context.now_ms())? / 1_000;
        Ok(Snapshot {
            version: state.version,
            tokens: state
                .tokens
                .into_iter()
                .rev()
                .map(|token| token.summary)
                .filter(|token| token.expires_at > now)
                .collect(),
        })
    }
}

#[derive(Deserialize, Serialize)]
pub(crate) struct Proof {
    pub id: Uuid,
    pub digest: [u8; 32],
}

pub(crate) struct Verify;
impl Read for Verify {
    const CLASS: &'static str = CLASS;
    const ID: u32 = 436;
    type Input = Proof;
    type Output = Option<u64>;

    fn execute(context: &mut Transaction<'_>, proof: Proof) -> crate::cells::Result<Option<u64>> {
        let now = timestamp(context.now_ms())? / 1_000;
        let digest = blake3::Hash::from_bytes(proof.digest);
        Ok(ledger(context)?
            .tokens
            .into_iter()
            .find(|token| token.summary.id == proof.id)
            .filter(|token| token.summary.expires_at > now && digest == token.digest)
            .map(|token| token.summary.expires_at))
    }
}

#[derive(Deserialize, Serialize)]
pub(crate) enum Edit {
    Create {
        proof: Proof,
        name: String,
        days: u16,
        creator: RepositoryAuthor,
    },
    Revoke {
        id: Uuid,
    },
}

#[derive(Deserialize, Serialize)]
pub(crate) struct Input {
    pub expected_version: u64,
    pub edit: Edit,
}

#[derive(Deserialize, Serialize)]
pub(crate) enum Outcome {
    Saved,
    Stale,
    Archived,
    Full,
}

pub(crate) struct Change;
impl Mutation for Change {
    const CLASS: &'static str = CLASS;
    const ID: u32 = 435;
    type Input = Input;
    type Output = Outcome;

    fn execute(
        context: &mut Transaction<'_>,
        input: Input,
    ) -> crate::cells::Result<MutationResult<Outcome>> {
        let mut state = ledger(context)?;
        // Removing an entry advances the version. A delayed create request
        // cannot recreate a credential after it has been revoked.
        if state.version != input.expected_version {
            return Ok(MutationResult::Rejected(Outcome::Stale));
        }
        let now = timestamp(context.now_ms())? / 1_000;
        state.tokens.retain(|token| token.summary.expires_at > now);
        match input.edit {
            Edit::Create {
                proof,
                name,
                days,
                creator,
            } => {
                validate_author(&creator)?;
                if proof.id.is_nil()
                    || !matches!(days, 1 | 7 | 30)
                    || name.is_empty()
                    || name != name.trim()
                    || name.chars().count() > 80
                    || name.chars().any(char::is_control)
                {
                    return Err(crate::cells::Error::Invalid("Invalid deploy token"));
                }
                if settings::archived(context)? {
                    return Ok(MutationResult::Rejected(Outcome::Archived));
                }
                if state.tokens.len() >= LIMIT {
                    return Ok(MutationResult::Rejected(Outcome::Full));
                }
                if state
                    .tokens
                    .iter()
                    .any(|token| token.summary.id == proof.id)
                {
                    return Ok(MutationResult::Rejected(Outcome::Stale));
                }
                state.tokens.push(Token {
                    summary: Summary {
                        id: proof.id,
                        name,
                        created_at: now,
                        expires_at: now + u64::from(days) * DAY_SECONDS,
                        creator_name: creator.name,
                    },
                    digest: proof.digest,
                });
            }
            Edit::Revoke { id } => {
                let count = state.tokens.len();
                state.tokens.retain(|token| token.summary.id != id);
                if state.tokens.len() == count {
                    return Ok(MutationResult::Rejected(Outcome::Saved));
                }
            }
        }
        state.version = next_version(state.version)?;
        context.put_document(KEY, &state)?;
        Ok(MutationResult::Success(Outcome::Saved))
    }
}
