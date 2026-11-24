use super::*;
use crate::cells::repository::issues::transfer::publication::Destination;

const PREFIX: &str = "organization/v1/development-links";
const LIMIT: usize = 10;

pub(crate) mod descriptions;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Pair {
    pub issue: Destination,
    pub pull: Destination,
}

impl Pair {
    fn key(self) -> String {
        format!(
            "{PREFIX}/pairs/{}:{}",
            endpoint_key(self.issue),
            endpoint_key(self.pull)
        )
    }

    fn ends(self) -> [(Side, Destination); 2] {
        [
            (Side::Issue(self.issue), self.pull),
            (Side::Pull(self.pull), self.issue),
        ]
    }
}

fn validate(endpoint: Destination) -> Result<()> {
    if endpoint.repository.is_nil() || !(1..=MAX_VERSION).contains(&endpoint.number) {
        return Err(Error::Invalid("Invalid development link address"));
    }
    Ok(())
}

fn endpoint_key(endpoint: Destination) -> String {
    format!("{}/{:016x}", endpoint.repository, endpoint.number)
}

#[derive(Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Side {
    Issue(Destination),
    Pull(Destination),
}

impl Side {
    fn prefix(self) -> Result<String> {
        let (kind, address) = match self {
            Self::Issue(address) => ("issues", address),
            Self::Pull(address) => ("pulls", address),
        };
        validate(address)?;
        Ok(format!("{PREFIX}/{kind}/{}/", endpoint_key(address)))
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct State {
    pub active: bool,
    pub version: u64,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub closes_on_merge: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Link {
    pub pair: Pair,
    pub version: u64,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub closes_on_merge: bool,
}

pub(crate) struct List;
impl Read for List {
    const CLASS: &'static str = CLASS;
    const ID: u32 = 85;
    type Input = Side;
    type Output = Vec<Link>;

    fn execute(context: &mut Transaction<'_>, side: Side) -> Result<Vec<Link>> {
        Ok(context
            .documents_after::<Link>(&side.prefix()?, None, LIMIT)?
            .into_iter()
            .map(|(_, link)| link)
            .collect())
    }
}

pub(crate) struct Get;
impl Read for Get {
    const CLASS: &'static str = CLASS;
    const ID: u32 = 86;
    type Input = Pair;
    type Output = State;

    fn execute(context: &mut Transaction<'_>, pair: Pair) -> Result<State> {
        validate(pair.issue)?;
        validate(pair.pull)?;
        Ok(context.document(&pair.key())?.unwrap_or_default())
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Edit {
    pub pair: Pair,
    pub active: bool,
    pub version: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub closes_on_merge: Option<bool>,
}

#[derive(Deserialize, Serialize)]
pub(crate) enum Decision {
    Applied(State),
    Conflict,
    Limit,
}

pub(crate) struct Set;
impl Mutation for Set {
    const CLASS: &'static str = CLASS;
    const ID: u32 = 85;
    type Input = Edit;
    type Output = Decision;

    fn execute(context: &mut Transaction<'_>, input: Edit) -> Result<MutationResult<Decision>> {
        let prior = Get::execute(context, input.pair)?;
        let closes_on_merge =
            input.active && input.closes_on_merge.unwrap_or(prior.closes_on_merge);
        if prior.active == input.active
            && prior.closes_on_merge == closes_on_merge
            && (prior.version == input.version
                || input.version.checked_add(1) == Some(prior.version))
        {
            return Ok(MutationResult::Rejected(Decision::Applied(prior)));
        }
        if prior.version != input.version {
            return Ok(MutationResult::Rejected(Decision::Conflict));
        }
        let ends = input.pair.ends();
        if input.active && !prior.active {
            for (side, _) in ends {
                if context
                    .documents_after::<Link>(&side.prefix()?, None, LIMIT)?
                    .len()
                    == LIMIT
                {
                    return Ok(MutationResult::Rejected(Decision::Limit));
                }
            }
        }
        let version = prior
            .version
            .checked_add(1)
            .filter(|next| *next <= MAX_VERSION)
            .ok_or(Error::Invalid("Development link version exhausted"))?;
        let state = State {
            active: input.active,
            version,
            closes_on_merge,
        };
        let link = Link {
            pair: input.pair,
            version,
            closes_on_merge,
        };
        context.put_document(&input.pair.key(), &state)?;
        // Both endpoints share one receipt and become visible atomically.
        for (side, other) in ends {
            let key = format!("{}{}", side.prefix()?, endpoint_key(other));
            if input.active {
                context.put_document(&key, &link)?;
            } else {
                context.delete_document(&key)?;
            }
        }
        Ok(MutationResult::Success(Decision::Applied(state)))
    }
}
