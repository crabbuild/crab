use super::*;

#[derive(Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(test, derive(ts_rs::TS))]
pub(crate) enum ReviewAvailabilityChoice {
    #[default]
    Available,
    ThirtyMinutes,
    OneHour,
    FourHours,
    OneDay,
    OneWeek,
    UntilCleared,
}

impl ReviewAvailabilityChoice {
    fn minutes(self) -> Option<i64> {
        match self {
            Self::Available | Self::UntilCleared => None,
            Self::ThirtyMinutes => Some(30),
            Self::OneHour => Some(60),
            Self::FourHours => Some(240),
            Self::OneDay => Some(1_440),
            Self::OneWeek => Some(10_080),
        }
    }
}

#[derive(Deserialize, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub(crate) struct ReviewAvailability {
    pub version: u64,
    pub busy: bool,
    pub expires_at_ms: Option<i64>,
}

#[derive(Default, Deserialize, Serialize)]
struct Preference {
    version: u64,
    choice: ReviewAvailabilityChoice,
    expires_at_ms: Option<i64>,
}

impl Preference {
    fn view(&self, now_ms: i64) -> ReviewAvailability {
        let busy = self.choice != ReviewAvailabilityChoice::Available
            && self.expires_at_ms.is_none_or(|expires| expires > now_ms);
        ReviewAvailability {
            version: self.version,
            busy,
            expires_at_ms: self.expires_at_ms.filter(|_| busy),
        }
    }
}

fn key(actor: Uuid) -> String {
    format!("directory/v1/review-availability/{actor}")
}

fn load(context: &Transaction<'_>, actor: Uuid) -> Result<Preference> {
    Ok(context.document(&key(actor))?.unwrap_or_default())
}

pub(super) fn busy(context: &Transaction<'_>, actor: Uuid) -> Result<bool> {
    Ok(load(context, actor)?.view(context.now_ms()).busy)
}

pub(crate) struct GetAvailability;
impl Read for GetAvailability {
    const CLASS: &'static str = CLASS;
    const ID: u32 = 31;
    type Input = IdentityKey;
    type Output = ReviewAvailability;

    fn execute(context: &mut Transaction<'_>, identity: IdentityKey) -> Result<Self::Output> {
        let actor = LookupAccounts::execute(context, vec![identity])?
            .into_iter()
            .next()
            .flatten();
        let preference = actor
            .map(|actor| load(context, actor))
            .transpose()?
            .unwrap_or_default();
        Ok(preference.view(context.now_ms()))
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Change {
    pub version: u64,
    pub choice: ReviewAvailabilityChoice,
}

#[derive(Deserialize, Serialize)]
pub(crate) struct Input {
    pub identity: IdentityKey,
    pub change: Change,
}

pub(crate) struct SetAvailability;
impl Mutation for SetAvailability {
    const CLASS: &'static str = CLASS;
    const ID: u32 = 31;
    type Input = Input;
    type Output = Option<ReviewAvailability>;

    fn execute(
        context: &mut Transaction<'_>,
        input: Input,
    ) -> Result<MutationResult<Self::Output>> {
        let next = input
            .change
            .version
            .checked_add(1)
            .filter(|version| *version <= MAX_VERSION)
            .ok_or(Error::Invalid("Review availability version is invalid"))?;
        let (actor, _) = accounts::ensure(context, &input.identity)?;
        let current = load(context, actor)?;
        let now = context.now_ms();
        if current.version == next && current.choice == input.change.choice {
            // A retry preserves the original expiry even when it arrives later.
            return Ok(MutationResult::Success(Some(current.view(now))));
        }
        if current.version != input.change.version {
            return Ok(MutationResult::Rejected(None));
        }
        let current = Preference {
            version: next,
            choice: input.change.choice,
            expires_at_ms: input
                .change
                .choice
                .minutes()
                .map(|minutes| now.checked_add(minutes * 60_000))
                .map(|expiry| expiry.ok_or(Error::Invalid("Review availability expiry is invalid")))
                .transpose()?,
        };
        context.put_document(&key(actor), &current)?;
        advance(context)?;
        Ok(MutationResult::Success(Some(current.view(now))))
    }
}

#[cfg(test)]
#[test]
fn review_availability_expires_without_changing_the_saved_version() {
    let preference = Preference {
        version: 2,
        choice: ReviewAvailabilityChoice::ThirtyMinutes,
        expires_at_ms: Some(1_800_000),
    };
    assert!(preference.view(1_799_999).busy);
    let expired = preference.view(1_800_000);
    assert!(!expired.busy);
    assert_eq!(expired.version, 2);
    assert_eq!(expired.expires_at_ms, None);
}
