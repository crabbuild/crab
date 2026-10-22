use super::*;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct ActorSummary {
    pub id: Uuid,
    pub handle: String,
    pub name: String,
}

impl From<ActorProfile> for ActorSummary {
    fn from(profile: ActorProfile) -> Self {
        Self {
            id: profile.id,
            handle: profile.handle,
            name: profile.name,
        }
    }
}

pub(crate) struct LookupActors;
impl Read for LookupActors {
    const CLASS: &'static str = super::CLASS;
    const ID: u32 = 6;
    type Input = Vec<IdentityKey>;
    type Output = Vec<Option<ActorSummary>>;

    fn execute(context: &mut Transaction<'_>, identities: Self::Input) -> Result<Self::Output> {
        if identities.len() > 100 {
            return Err(Error::Invalid("Actor lookup is limited to 100 identities"));
        }
        for identity in &identities {
            identity.validate()?;
        }
        // All names come from one Directory snapshot. Qualified identity lookup
        // prevents two providers with the same subject from sharing attribution.
        identities
            .iter()
            .map(|identity| own_profile(context, identity).map(|profile| profile.map(Into::into)))
            .collect()
    }
}

pub(crate) struct LookupAccounts;
impl Read for LookupAccounts {
    const CLASS: &'static str = super::CLASS;
    const ID: u32 = 12;
    type Input = Vec<IdentityKey>;
    type Output = Vec<Option<Uuid>>;
    fn execute(context: &mut Transaction<'_>, identities: Self::Input) -> Result<Self::Output> {
        if identities.len() > 100 {
            return Err(Error::Invalid(
                "Account lookup is limited to 100 identities",
            ));
        }
        identities
            .iter()
            .map(|identity| {
                identity.validate()?;
                let rows = context.query(
                    "SELECT id FROM accounts WHERE issuer = ? AND subject = ?",
                    &[
                        SqlValue::Text(identity.issuer.clone()),
                        SqlValue::Text(identity.subject.clone()),
                    ],
                )?;
                rows.rows
                    .first()
                    .map(|row| match row.as_slice() {
                        [SqlValue::Blob(id)] => Uuid::from_slice(id).map_err(Error::sdk),
                        _ => Err(Error::Invalid("Account identity record is invalid")),
                    })
                    .transpose()
            })
            .collect()
    }
}
pub(crate) struct LookupProfiles;
impl Read for LookupProfiles {
    const CLASS: &'static str = super::CLASS;
    const ID: u32 = 13;
    type Input = Vec<Uuid>;
    type Output = Vec<Option<ActorSummary>>;
    fn execute(context: &mut Transaction<'_>, ids: Self::Input) -> Result<Self::Output> {
        if ids.len() > 100 || ids.iter().any(Uuid::is_nil) {
            return Err(Error::Invalid(
                "Profile lookup requires at most 100 valid actor IDs",
            ));
        }
        ids.into_iter()
            .map(|id| profile(context, id).map(|profile| profile.map(Into::into)))
            .collect()
    }
}

#[cfg(test)]
mod tests;
