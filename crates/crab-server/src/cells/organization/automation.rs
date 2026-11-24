use super::*;

const INDEX: &str = "organization/v1/automation";
const LIMIT: usize = 32;
const MAX_GRANTS: usize = 50;
const MAX_TOKENS: usize = 4;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(rename = "AutomationAccess"))]
pub(crate) enum Access {
    Read,
    Write,
}

impl From<Access> for crate::RepositoryAccess {
    fn from(access: Access) -> Self {
        match access {
            Access::Read => Self::Read,
            Access::Write => Self::Write,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct Grant {
    pub repository: Uuid,
    pub ownership_epoch: u32,
    pub access: Access,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Audit {
    Created,
    Edited,
    Enabled,
    Disabled,
    Deleted,
    TokenIssued,
    TokenRevoked,
}

#[derive(Default, Deserialize, Serialize)]
struct Index {
    version: u64,
    ids: Vec<Uuid>,
}

#[derive(Deserialize, Serialize)]
struct Identity {
    name: String,
    enabled: bool,
    grants: Vec<Grant>,
    tokens: Vec<Token>,
}

#[derive(Clone, Deserialize, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(rename = "AutomationToken"))]
pub(crate) struct TokenSummary {
    pub id: Uuid,
    pub expires_at: u64,
}

#[derive(Deserialize, Serialize)]
struct Token {
    summary: TokenSummary,
    digest: [u8; 32],
    grants: Vec<Grant>,
}

#[derive(Deserialize, Serialize)]
pub(crate) struct Summary {
    pub id: Uuid,
    pub name: String,
    pub enabled: bool,
    pub grants: Vec<Grant>,
    pub tokens: Vec<TokenSummary>,
}

#[derive(Deserialize, Serialize)]
pub(crate) struct Snapshot {
    pub version: u64,
    pub identities: Vec<Summary>,
}

fn index(context: &Transaction<'_>) -> Result<Index> {
    Ok(context.document(INDEX)?.unwrap_or_default())
}

fn key(id: Uuid) -> String {
    format!("{INDEX}/{id}")
}

fn epoch(context: &Transaction<'_>) -> Result<u64> {
    u64::try_from(context.now_ms() / 1000).map_err(Error::sdk)
}

pub(crate) struct List;
impl Read for List {
    const CLASS: &'static str = CLASS;
    const ID: u32 = 52;
    type Input = Caller;
    type Output = Snapshot;

    fn execute(context: &mut Transaction<'_>, _: Self::Input) -> Result<Snapshot> {
        let index = index(context)?;
        let now = epoch(context)?;
        let identities = index
            .ids
            .into_iter()
            .map(|id| {
                let value: Identity = context
                    .document(&key(id))?
                    .ok_or(Error::Invalid("Automation identity record is missing"))?;
                Ok(Summary {
                    id,
                    name: value.name,
                    enabled: value.enabled,
                    grants: value.grants,
                    tokens: value
                        .tokens
                        .into_iter()
                        .filter(|token| token.summary.expires_at > now)
                        .map(|token| token.summary)
                        .collect(),
                })
            })
            .collect::<Result<_>>()?;
        Ok(Snapshot {
            version: index.version,
            identities,
        })
    }
}

#[derive(Clone, Deserialize, Serialize)]
pub(crate) struct Proof {
    pub identity: Uuid,
    pub token: Uuid,
    pub digest: [u8; 32],
}

#[derive(Deserialize, Serialize)]
pub(crate) struct VerifyInput {
    pub proof: Proof,
    pub repository: Option<(Uuid, u32)>,
}

#[derive(Deserialize, Serialize)]
pub(crate) struct Verified {
    pub name: String,
    pub expires_at: u64,
    pub access: Option<Access>,
}

pub(crate) struct Verify;
impl Read for Verify {
    const CLASS: &'static str = CLASS;
    const ID: u32 = 53;
    type Input = VerifyInput;
    type Output = Option<Verified>;

    fn execute(context: &mut Transaction<'_>, input: VerifyInput) -> Result<Option<Verified>> {
        let now = epoch(context)?;
        let Some(identity) = context
            .document::<Identity>(&key(input.proof.identity))?
            .filter(|identity| identity.enabled)
        else {
            return Ok(None);
        };
        let proof = blake3::Hash::from_bytes(input.proof.digest);
        let Some(token) = identity.tokens.into_iter().find(|token| {
            token.summary.id == input.proof.token
                && token.summary.expires_at > now
                && proof == token.digest
        }) else {
            return Ok(None);
        };
        let access = input.repository.and_then(|(id, ownership_epoch)| {
            token
                .grants
                .iter()
                .find(|grant| grant.repository == id && grant.ownership_epoch == ownership_epoch)
                .map(|grant| grant.access)
        });
        if input.repository.is_some() && access.is_none() {
            return Ok(None);
        }
        Ok(Some(Verified {
            expires_at: token.summary.expires_at,
            name: identity.name,
            access,
        }))
    }
}

#[derive(Deserialize, Serialize)]
pub(crate) struct Input {
    pub actor: Uuid,
    pub expected_version: u64,
    pub identity: Uuid,
    pub edit: Edit,
}

#[derive(Deserialize, Serialize)]
pub(crate) enum Edit {
    Create {
        name: String,
        grants: Vec<Grant>,
    },
    Configure {
        name: String,
        grants: Vec<Grant>,
    },
    Enabled {
        enabled: bool,
    },
    Delete,
    Issue {
        token: Uuid,
        digest: [u8; 32],
        hours: u16,
        grants: Vec<Grant>,
    },
    Revoke {
        token: Uuid,
    },
}

#[derive(Deserialize, Serialize)]
pub(crate) enum Outcome {
    Saved,
    Stale,
    Missing,
    Full,
    Disabled,
    Enabled,
    NoGrants,
}

pub(crate) struct Change;
impl Mutation for Change {
    const CLASS: &'static str = CLASS;
    const ID: u32 = 49;
    type Input = Input;
    type Output = Outcome;

    fn execute(context: &mut Transaction<'_>, input: Input) -> Result<MutationResult<Outcome>> {
        let mut index = index(context)?;
        if index.version != input.expected_version || input.identity.is_nil() {
            return Ok(MutationResult::Rejected(Outcome::Stale));
        }
        let path = key(input.identity);
        let action = if let Edit::Create { name, grants } = input.edit {
            validate(&name, &grants)?;
            if index.ids.len() >= LIMIT || index.ids.contains(&input.identity) {
                return Ok(MutationResult::Rejected(Outcome::Full));
            }
            index.ids.push(input.identity);
            context.put_document(
                &path,
                &Identity {
                    name,
                    grants,
                    enabled: true,
                    tokens: vec![],
                },
            )?;
            Audit::Created
        } else {
            let Some(mut identity) = context.document::<Identity>(&path)? else {
                return Ok(MutationResult::Rejected(Outcome::Missing));
            };
            let now = epoch(context)?;
            identity
                .tokens
                .retain(|token| token.summary.expires_at > now);
            let action = match input.edit {
                Edit::Configure { name, grants } => {
                    validate(&name, &grants)?;
                    if identity.name == name && identity.grants == grants {
                        return Ok(MutationResult::Rejected(Outcome::Saved));
                    }
                    identity.name = name;
                    // Store the reduced ceiling on the credential so re-adding
                    // a grant later cannot restore an old token's authority.
                    for token in &mut identity.tokens {
                        token.grants.retain_mut(|grant| {
                            let current = grants.iter().find(|current| {
                                current.repository == grant.repository
                                    && current.ownership_epoch == grant.ownership_epoch
                            });
                            if let Some(current) = current {
                                grant.access = grant.access.min(current.access);
                            }
                            current.is_some()
                        });
                    }
                    identity.grants = grants;
                    Audit::Edited
                }
                Edit::Enabled { enabled } => {
                    if enabled == identity.enabled {
                        return Ok(MutationResult::Rejected(Outcome::Saved));
                    }
                    identity.enabled = enabled;
                    if enabled {
                        Audit::Enabled
                    } else {
                        identity.tokens.clear();
                        Audit::Disabled
                    }
                }
                Edit::Delete => {
                    if identity.enabled {
                        return Ok(MutationResult::Rejected(Outcome::Enabled));
                    }
                    index.ids.retain(|id| *id != input.identity);
                    context.delete_document(&path)?;
                    return save(context, index, input.actor, input.identity, Audit::Deleted);
                }
                Edit::Issue {
                    token,
                    digest,
                    hours,
                    grants,
                } => {
                    if token.is_nil() || !matches!(hours, 1 | 24 | 168 | 720) {
                        return Err(Error::Invalid("Invalid automation token"));
                    }
                    if !identity.enabled {
                        return Ok(MutationResult::Rejected(Outcome::Disabled));
                    }
                    if grants.is_empty()
                        || grants.iter().any(|grant| !identity.grants.contains(grant))
                    {
                        return Ok(MutationResult::Rejected(Outcome::NoGrants));
                    }
                    if identity.tokens.len() >= MAX_TOKENS
                        || identity.tokens.iter().any(|old| old.summary.id == token)
                    {
                        return Ok(MutationResult::Rejected(Outcome::Full));
                    }
                    identity.tokens.push(Token {
                        summary: TokenSummary {
                            id: token,
                            expires_at: now + u64::from(hours) * 3600,
                        },
                        digest,
                        grants,
                    });
                    Audit::TokenIssued
                }
                Edit::Revoke { token } => {
                    let count = identity.tokens.len();
                    identity.tokens.retain(|entry| entry.summary.id != token);
                    if count == identity.tokens.len() {
                        return Ok(MutationResult::Rejected(Outcome::Saved));
                    }
                    Audit::TokenRevoked
                }
                Edit::Create { .. } => return Err(Error::Invalid("Invalid automation change")),
            };
            context.put_document(&path, &identity)?;
            action
        };
        save(context, index, input.actor, input.identity, action)
    }
}

fn save(
    context: &Transaction<'_>,
    mut index: Index,
    actor: Uuid,
    identity: Uuid,
    action: Audit,
) -> Result<MutationResult<Outcome>> {
    index.version = index
        .version
        .checked_add(1)
        .filter(|version| *version <= MAX_VERSION)
        .ok_or(Error::Invalid("Automation revision limit reached"))?;
    context.put_document(INDEX, &index)?;
    advance(context, actor, Action::Automation { identity, action })?;
    Ok(MutationResult::Success(Outcome::Saved))
}

fn validate(name: &str, grants: &[Grant]) -> Result<()> {
    if name.is_empty()
        || name.trim() != name
        || name.chars().count() > 80
        || name.chars().any(char::is_control)
        || grants.len() > MAX_GRANTS
        || grants.iter().any(|grant| grant.repository.is_nil())
        || !grants
            .windows(2)
            .all(|pair| pair[0].repository < pair[1].repository)
    {
        return Err(Error::Invalid(
            "Choose a name and at most 50 distinct repositories",
        ));
    }
    Ok(())
}
