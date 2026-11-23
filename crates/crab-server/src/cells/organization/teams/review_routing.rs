use super::*;

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(rename = "ReviewRoutingAlgorithm"))]
pub(crate) enum Algorithm {
    #[default]
    Disabled,
    RoundRobin,
    FewestOpen,
}

pub(crate) const MAX_REVIEWERS: u8 = 10;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(rename = "ReviewRoutingSettings"))]
pub(crate) struct Settings {
    pub version: u64,
    pub algorithm: Algorithm,
    pub reviewers: u8,
}

fn key(team: Uuid) -> String {
    format!("organization/v1/teams/{team}/review-routing")
}

fn load(context: &Transaction<'_>, team: Uuid) -> Result<Settings> {
    Ok(context.document(&key(team))?.unwrap_or(Settings {
        version: 0,
        algorithm: Algorithm::Disabled,
        reviewers: 1,
    }))
}

pub(super) fn set(
    context: &Transaction<'_>,
    who: Uuid,
    id: Uuid,
    mut settings: Settings,
) -> Result<Outcome> {
    if !(1..=MAX_REVIEWERS).contains(&settings.reviewers) {
        return Ok(Outcome::InvalidRouting);
    }
    if team(context, id)?.is_none() {
        return Ok(Outcome::Missing);
    }
    let old = load(context, id)?;
    if old.version != settings.version {
        return Ok(Outcome::Conflict);
    }
    if old != settings {
        settings.version = next(settings.version)?;
        context.put_document(&key(id), &settings)?;
        advance(context, who, Action::TeamEdited { team: id })?;
    }
    Ok(Outcome::Applied { id })
}

pub(crate) struct GetSettings;
impl Read for GetSettings {
    const CLASS: &'static str = super::super::CLASS;
    const ID: u32 = 50;
    type Input = Key;
    type Output = Settings;

    fn execute(context: &mut Transaction<'_>, key: Key) -> Result<Settings> {
        load(context, key.team)
    }
}

#[derive(Deserialize, Serialize)]
pub(crate) struct TeamReviewRoster {
    pub settings: Settings,
    pub members: Vec<Uuid>,
}

pub(crate) struct ReviewRoster;
impl Read for ReviewRoster {
    const CLASS: &'static str = super::super::CLASS;
    const ID: u32 = 51;
    type Input = Uuid;
    type Output = TeamReviewRoster;

    fn execute(context: &mut Transaction<'_>, team: Uuid) -> Result<TeamReviewRoster> {
        let members = repository_access::team_members(context, vec![team])?;
        Ok(TeamReviewRoster {
            settings: load(context, team)?,
            members: members.into_iter().map(|(_, actor)| actor).collect(),
        })
    }
}
