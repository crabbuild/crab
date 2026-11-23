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
const MAX_EXCLUSIONS: usize = 256;

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(rename = "ReviewRoutingOptions"))]
pub(crate) struct Options {
    pub count_existing: bool,
    pub keep_team: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(rename = "ReviewRoutingSettings"))]
pub(crate) struct Settings {
    pub version: u64,
    pub algorithm: Algorithm,
    pub reviewers: u8,
    #[serde(default)]
    pub excluded: Vec<Uuid>,
    #[serde(default)]
    pub options: Options,
}

fn key(team: Uuid) -> String {
    format!("organization/v1/teams/{team}/review-routing")
}

fn load(context: &Transaction<'_>, team: Uuid) -> Result<Settings> {
    Ok(context.document(&key(team))?.unwrap_or(Settings {
        version: 0,
        algorithm: Algorithm::Disabled,
        reviewers: 1,
        excluded: Vec::new(),
        options: Options::default(),
    }))
}

fn members(context: &Transaction<'_>, team: Uuid) -> Result<Vec<Uuid>> {
    Ok(repository_access::team_members(context, vec![team])?
        .into_iter()
        .map(|(_, actor)| actor)
        .collect())
}

pub(super) struct Update<'a> {
    pub version: u64,
    pub algorithm: Algorithm,
    pub reviewers: u8,
    pub excluded: Option<&'a [Uuid]>,
    pub options: Option<Options>,
}

pub(super) fn set(
    context: &Transaction<'_>,
    who: Uuid,
    id: Uuid,
    update: Update<'_>,
) -> Result<Outcome> {
    let Update {
        version,
        algorithm,
        reviewers,
        excluded,
        options,
    } = update;
    if !(1..=MAX_REVIEWERS).contains(&reviewers) {
        return Ok(Outcome::InvalidRouting);
    }
    if team(context, id)?.is_none() {
        return Ok(Outcome::Missing);
    }
    let old = load(context, id)?;
    if old.version != version {
        return Ok(Outcome::Conflict);
    }
    // An omitted field retains the policy and the digest of older submissions.
    let excluded = match excluded {
        Some(ids) if ids.len() > MAX_EXCLUSIONS => return Ok(Outcome::InvalidRoutingExclusions),
        Some([]) => Vec::new(),
        Some(ids) => {
            let roster = members(context, id)?;
            if ids.iter().any(|actor| roster.binary_search(actor).is_err()) {
                return Ok(Outcome::InvalidRoutingExclusions);
            }
            let mut sorted = ids.to_vec();
            sorted.sort_unstable();
            sorted.dedup();
            sorted
        }
        None => old.excluded.clone(),
    };
    let mut settings = Settings {
        version,
        algorithm,
        reviewers,
        excluded,
        options: options.unwrap_or(old.options),
    };
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
        let mut settings = load(context, key.team)?;
        if !settings.excluded.is_empty() {
            let roster = members(context, key.team)?;
            settings
                .excluded
                .retain(|actor| roster.binary_search(actor).is_ok());
        }
        Ok(settings)
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
        Ok(TeamReviewRoster {
            settings: load(context, team)?,
            members: members(context, team)?,
        })
    }
}
