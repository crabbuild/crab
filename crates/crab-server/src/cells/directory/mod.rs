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
const COLUMNS: &str = "p.id, p.handle, p.name, p.version, p.created_at_ms, p.updated_at_ms";

mod accounts;
pub(crate) use accounts::RegisterAccounts;
mod migration;
pub use migration::DirectoryState;
pub(crate) use migration::{GetDirectoryState, MigrateDirectory};
mod lookup;
mod search;
pub(crate) use search::{CompleteHandles, ResolveHandles, validate_handle_prefix};
mod profiles;
pub(crate) use lookup::{
    AccountsExist, ActorSummary, LookupAccounts, LookupActors, LookupProfiles,
};
pub(crate) use profiles::{
    CreateProfile, CreateProfileInput, EditProfile, EditProfileInput, ProfileOutcome,
    validate_handle, validate_profile,
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

pub(in crate::cells) fn initialize(
    context: &Transaction<'_>,
    create: bool,
    operation: u32,
    mutation: bool,
) -> Result<()> {
    let tables = context.query(
        "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'directory_identity'",
        &[],
    )?;
    if tables.rows.is_empty() {
        if !create {
            return Err(Error::NotInitialized);
        }
        context.install_schema(include_str!("identity.sql"))?;
        context.install_schema(include_str!("schema.sql"))?;
        context.install_schema(include_str!("accounts/schema.sql"))?;
        context.install_schema(include_str!("profiles/schema.sql"))?;
    }
    let maintenance = matches!(
        (operation, mutation),
        (GetDirectoryState::ID, false) | (MigrateDirectory::ID, true)
    );
    #[cfg(test)]
    let maintenance =
        maintenance || (operation == migration::tests::InspectSource::ID && !mutation);
    if format(context)? != 2 && !maintenance {
        return Err(Error::SchemaUnavailable);
    }
    Ok(())
}

fn format(context: &Transaction<'_>) -> Result<u64> {
    let rows = context.query(
        "SELECT format FROM directory_identity WHERE singleton = 1",
        &[],
    )?;
    match rows.rows.first().map(Vec::as_slice) {
        Some([SqlValue::Integer(format)]) => u64::try_from(*format)
            .map_err(|_| Error::Invalid("directory schema identity is invalid")),
        _ => Err(Error::Invalid("directory schema identity is invalid")),
    }
}

fn profile(context: &Transaction<'_>, id: Uuid) -> Result<Option<ActorProfile>> {
    let rows = context.query(
        &format!("SELECT {COLUMNS} FROM profiles p WHERE p.id = ?"),
        &[SqlValue::Blob(id.as_bytes().to_vec())],
    )?;
    rows.rows
        .first()
        .map(|row| ActorProfile::try_from(row.as_slice()))
        .transpose()
}
fn own_profile(context: &Transaction<'_>, identity: &IdentityKey) -> Result<Option<ActorProfile>> {
    let rows = context.query(
        &format!("SELECT {COLUMNS} FROM profiles p JOIN accounts a ON a.id = p.id WHERE a.issuer = ? AND a.subject = ?"),
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
        <AccountsExist as Read>::ID => invoke_read::<AccountsExist>(context, input),
        <LookupAccounts as Read>::ID => invoke_read::<LookupAccounts>(context, input),
        <LookupProfiles as Read>::ID => invoke_read::<LookupProfiles>(context, input),
        #[cfg(test)]
        migration::tests::InspectSource::ID => {
            invoke_read::<migration::tests::InspectSource>(context, input)
        }
        GetDirectoryState::ID => invoke_read::<GetDirectoryState>(context, input),
        GetOwnProfile::ID => invoke_read::<GetOwnProfile>(context, input),
        GetActor::ID => invoke_read::<GetActor>(context, input),
        GetDirectoryRevision::ID => invoke_read::<GetDirectoryRevision>(context, input),
        LookupActors::ID => invoke_read::<LookupActors>(context, input),
        ResolveHandles::ID => invoke_read::<ResolveHandles>(context, input),
        CompleteHandles::ID => invoke_read::<CompleteHandles>(context, input),
        _ => Err(Error::Invalid("unknown directory query")),
    }
}
pub(in crate::cells) fn dispatch_mutation(
    context: &mut Transaction<'_>,
    id: u32,
    input: Value,
) -> Result<MutationResult<Value>> {
    match id {
        #[cfg(test)]
        migration::tests::SeedVersionOne::ID => {
            invoke_mutation::<migration::tests::SeedVersionOne>(context, input)
        }
        MigrateDirectory::ID => invoke_mutation::<MigrateDirectory>(context, input),
        RegisterAccounts::ID => invoke_mutation::<RegisterAccounts>(context, input),
        CreateProfile::ID => invoke_mutation::<CreateProfile>(context, input),
        EditProfile::ID => invoke_mutation::<EditProfile>(context, input),
        _ => Err(Error::Invalid("unknown directory command")),
    }
}

#[cfg(test)]
mod tests;
