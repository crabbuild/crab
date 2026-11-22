use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{SubjectKind, activity, next_version};
use crate::cells::{
    Error, Result,
    application::{Mutation, MutationResult, Read, Transaction},
    repository::CLASS,
};

const PREFERENCES: &str = "collaboration/v1/watches/preferences/";
const WATCHERS: &str = "collaboration/v1/watches/active/";
const PAGE_SIZE: usize = 32;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Mode {
    #[default]
    Direct,
    Issues,
    PullRequests,
    IssuesAndPullRequests,
    Muted,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Preference {
    pub mode: Mode,
    pub version: u64,
}

#[derive(Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Subscription {
    preference: Preference,
    issues_after: Option<u64>,
    pulls_after: Option<u64>,
}

fn subscription(context: &Transaction<'_>, actor: Uuid) -> Result<Subscription> {
    if actor.is_nil() {
        return Err(Error::Invalid("Watch account is invalid"));
    }
    Ok(context
        .document(&format!("{PREFERENCES}{actor}"))?
        .unwrap_or_default())
}

pub(crate) struct Get;
impl Read for Get {
    const CLASS: &'static str = CLASS;
    const ID: u32 = 416;
    type Input = Uuid;
    type Output = Preference;

    fn execute(context: &mut Transaction<'_>, actor: Uuid) -> Result<Preference> {
        Ok(subscription(context, actor)?.preference)
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Change {
    pub actor: Uuid,
    pub desired: Preference,
}

pub(crate) struct Set;
impl Mutation for Set {
    const CLASS: &'static str = CLASS;
    const ID: u32 = 416;
    type Input = Change;
    type Output = Option<Preference>;

    fn execute(
        context: &mut Transaction<'_>,
        input: Change,
    ) -> Result<MutationResult<Self::Output>> {
        let mut current = subscription(context, input.actor)?;
        let mode = input.desired.mode;
        if current.preference.mode == mode {
            return Ok(MutationResult::Success(Some(current.preference)));
        }
        if current.preference.version != input.desired.version {
            return Ok(MutationResult::Rejected(None));
        }
        let latest = activity::latest(context)?;
        current.issues_after = matches!(mode, Mode::Issues | Mode::IssuesAndPullRequests)
            .then(|| current.issues_after.unwrap_or(latest));
        current.pulls_after = matches!(mode, Mode::PullRequests | Mode::IssuesAndPullRequests)
            .then(|| current.pulls_after.unwrap_or(latest));
        current.preference = Preference {
            mode,
            version: next_version(current.preference.version)?,
        };
        let watcher = format!("{WATCHERS}{}", input.actor);
        // The sparse index is updated with the preference. Version tombstones
        // fence stale writes after unwatching without keeping an active row.
        if current.issues_after.is_some() || current.pulls_after.is_some() {
            context.put_document(&watcher, &true)?;
        } else {
            context.delete_document(&watcher)?;
        }
        context.put_document(&format!("{PREFERENCES}{}", input.actor), &current)?;
        Ok(MutationResult::Success(Some(current.preference)))
    }
}

pub(super) fn active(context: &Transaction<'_>) -> Result<bool> {
    Ok(!context.documents::<bool>(WATCHERS, None, 1)?.is_empty())
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Query {
    pub kind: SubjectKind,
    pub activity: u64,
    pub before: Option<Uuid>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct WatchPage {
    pub recipients: Vec<Uuid>,
    pub next: Option<Uuid>,
}

pub(crate) struct ListWatchers;
impl Read for ListWatchers {
    const CLASS: &'static str = CLASS;
    const ID: u32 = 417;
    type Input = Query;
    type Output = WatchPage;

    fn execute(context: &mut Transaction<'_>, input: Query) -> Result<WatchPage> {
        let before = input.before.map(|actor| format!("{WATCHERS}{actor}"));
        let rows = context.documents::<bool>(WATCHERS, before.as_deref(), PAGE_SIZE + 1)?;
        let more = rows.len() > PAGE_SIZE;
        let mut last = None;
        let mut recipients = Vec::new();
        for (key, _) in rows.into_iter().take(PAGE_SIZE) {
            let actor = key
                .strip_prefix(WATCHERS)
                .and_then(|key| Uuid::parse_str(key).ok())
                .ok_or(Error::Invalid("Watch index key is invalid"))?;
            last = Some(actor);
            let current = subscription(context, actor)?;
            let after = match input.kind {
                SubjectKind::Issue => current.issues_after,
                SubjectKind::Pull => current.pulls_after,
            };
            if after.is_some_and(|after| after < input.activity) {
                recipients.push(actor);
            }
        }
        // Advance over the scanned keys, including late subscriptions that do
        // not qualify for the event. A filtered page can still have successors.
        Ok(WatchPage {
            recipients,
            next: last.filter(|_| more),
        })
    }
}

pub(crate) struct Unmuted;
impl Read for Unmuted {
    const CLASS: &'static str = CLASS;
    const ID: u32 = 418;
    type Input = Vec<Uuid>;
    type Output = Vec<Uuid>;

    fn execute(context: &mut Transaction<'_>, actors: Vec<Uuid>) -> Result<Vec<Uuid>> {
        if actors.len() > crate::cells::directory::notifications::MAX_RECIPIENTS {
            return Err(Error::Invalid("Notification preference batch is too large"));
        }
        let mut unmuted = Vec::with_capacity(actors.len());
        for actor in actors {
            if subscription(context, actor)?.preference.mode != Mode::Muted {
                unmuted.push(actor);
            }
        }
        Ok(unmuted)
    }
}
