use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use super::application::{
    Mutation, MutationResult, Read, SqlValue, Transaction, invoke_mutation, invoke_read,
};
use super::{Error, Result};

pub(in crate::cells) const CLASS: &str = "Directory";
pub(in crate::cells) const NAMESPACE: &str = "DIRECTORY";
const MAX_VERSION: u64 = 9_007_199_254_740_991;
const COLUMNS: &str = "id, handle, name, version, created_at_ms, updated_at_ms";

mod lookup;
mod profiles;
pub(crate) use lookup::{ActorSummary, LookupActors};
pub(crate) use profiles::{
    CreateProfile, CreateProfileInput, EditProfile, EditProfileInput, ProfileOutcome,
    validate_profile,
};

// Authentication keys stay private to native commands. Public records expose
// only the randomly allocated actor ID and the actor's chosen profile fields.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct IdentityKey {
    pub issuer: String,
    pub subject: String,
}
impl IdentityKey {
    pub(crate) fn validate(&self) -> Result<()> {
        if self.issuer.is_empty()
            || self.issuer.chars().count() > 4096
            || self.subject.is_empty()
            || self.subject.chars().count() > 1024
            || self.issuer.chars().any(char::is_control)
            || self.subject.chars().any(char::is_control)
        {
            return Err(Error::Invalid("actor identity is invalid"));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct ActorProfile {
    pub id: Uuid,
    pub handle: String,
    pub name: String,
    pub version: u64,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
}

impl TryFrom<&[SqlValue]> for ActorProfile {
    type Error = Error;
    fn try_from(row: &[SqlValue]) -> Result<Self> {
        let [
            SqlValue::Blob(id),
            SqlValue::Text(handle),
            SqlValue::Text(name),
            SqlValue::Integer(version),
            SqlValue::Integer(created),
            SqlValue::Integer(updated),
        ] = row
        else {
            return Err(Error::Invalid("actor profile record is invalid"));
        };
        Ok(Self {
            id: Uuid::from_slice(id).map_err(Error::sdk)?,
            handle: handle.clone(),
            name: name.clone(),
            version: u64::try_from(*version)
                .map_err(|_| Error::Invalid("actor version is negative"))?,
            created_at_ms: u64::try_from(*created)
                .map_err(|_| Error::Invalid("actor timestamp is negative"))?,
            updated_at_ms: u64::try_from(*updated)
                .map_err(|_| Error::Invalid("actor timestamp is negative"))?,
        })
    }
}

pub(crate) struct GetOwnProfile;
impl Read for GetOwnProfile {
    const CLASS: &'static str = CLASS;
    const ID: u32 = 3;
    type Input = IdentityKey;
    type Output = Option<ActorProfile>;
    fn execute(context: &mut Transaction<'_>, identity: IdentityKey) -> Result<Self::Output> {
        identity.validate()?;
        own_profile(context, &identity)
    }
}
pub(crate) struct GetActor;
impl Read for GetActor {
    const CLASS: &'static str = CLASS;
    const ID: u32 = 4;
    type Input = Uuid;
    type Output = Option<ActorProfile>;
    fn execute(context: &mut Transaction<'_>, id: Uuid) -> Result<Self::Output> {
        if id.is_nil() {
            return Err(Error::Invalid("actor ID is invalid"));
        }
        profile(context, id)
    }
}
pub(crate) struct GetDirectoryRevision;
impl Read for GetDirectoryRevision {
    const CLASS: &'static str = CLASS;
    const ID: u32 = 5;
    type Input = ();
    type Output = u64;
    fn execute(context: &mut Transaction<'_>, _: ()) -> Result<Self::Output> {
        let row = context.query(
            "SELECT app_revision FROM directory_identity WHERE singleton = 1",
            &[],
        )?;
        match row.rows.first().map(Vec::as_slice) {
            Some([SqlValue::Integer(value)]) => {
                u64::try_from(*value).map_err(|_| Error::Invalid("directory revision is negative"))
            }
            _ => Err(Error::Invalid("directory revision is missing")),
        }
    }
}

pub(in crate::cells) fn initialize(context: &Transaction<'_>, create: bool) -> Result<()> {
    let tables = context.query(
        "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'directory_identity'",
        &[],
    )?;
    if tables.rows.is_empty() {
        if !create {
            return Err(Error::NotInitialized);
        }
        context.install_schema(include_str!("schema.sql"))?;
    }
    let identity = context.query(
        "SELECT format FROM directory_identity WHERE singleton = 1",
        &[],
    )?;
    if identity.rows.as_slice() != [vec![SqlValue::Integer(1)]] {
        return Err(Error::Invalid("directory schema identity is invalid"));
    }
    Ok(())
}

fn profile(context: &Transaction<'_>, id: Uuid) -> Result<Option<ActorProfile>> {
    let rows = context.query(
        &format!("SELECT {COLUMNS} FROM actors WHERE id = ?"),
        &[SqlValue::Blob(id.as_bytes().to_vec())],
    )?;
    rows.rows
        .first()
        .map(|row| ActorProfile::try_from(row.as_slice()))
        .transpose()
}
fn own_profile(context: &Transaction<'_>, identity: &IdentityKey) -> Result<Option<ActorProfile>> {
    let rows = context.query(
        &format!("SELECT {COLUMNS} FROM actors WHERE issuer = ? AND subject = ?"),
        &[
            SqlValue::Text(identity.issuer.clone()),
            SqlValue::Text(identity.subject.clone()),
        ],
    )?;
    rows.rows
        .first()
        .map(|row| ActorProfile::try_from(row.as_slice()))
        .transpose()
}
fn advance(context: &Transaction<'_>) -> Result<()> {
    let result = context.query("UPDATE directory_identity SET app_revision = app_revision + 1 WHERE singleton = 1 AND app_revision < 9007199254740991", &[])?;
    if result.rows_affected != 1 {
        return Err(Error::Invalid("directory revision limit reached"));
    }
    Ok(())
}

pub(in crate::cells) fn dispatch_read(
    context: &mut Transaction<'_>,
    id: u32,
    input: Value,
) -> Result<Value> {
    match id {
        GetOwnProfile::ID => invoke_read::<GetOwnProfile>(context, input),
        GetActor::ID => invoke_read::<GetActor>(context, input),
        GetDirectoryRevision::ID => invoke_read::<GetDirectoryRevision>(context, input),
        LookupActors::ID => invoke_read::<LookupActors>(context, input),
        _ => Err(Error::Invalid("unknown directory query")),
    }
}
pub(in crate::cells) fn dispatch_mutation(
    context: &mut Transaction<'_>,
    id: u32,
    input: Value,
) -> Result<MutationResult<Value>> {
    match id {
        CreateProfile::ID => invoke_mutation::<CreateProfile>(context, input),
        EditProfile::ID => invoke_mutation::<EditProfile>(context, input),
        _ => Err(Error::Invalid("unknown directory command")),
    }
}

#[cfg(test)]
mod tests;
