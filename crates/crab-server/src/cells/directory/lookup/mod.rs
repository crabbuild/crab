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

#[cfg(test)]
mod tests;
