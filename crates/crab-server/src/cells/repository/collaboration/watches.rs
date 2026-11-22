use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{Subject, SubjectKind, activity, next_version};
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

fn thread_index(subject: Subject) -> String {
    format!(
        "collaboration/v1/watches/discussions/{}/active/",
        subject.key()
    )
}

pub(super) fn active(context: &Transaction<'_>, subject: Subject) -> Result<bool> {
    Ok(!context.documents::<bool>(WATCHERS, None, 1)?.is_empty()
        || !context
            .documents::<bool>(&thread_index(subject), None, 1)?
            .is_empty())
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ThreadMode {
    #[default]
    Inherit,
    Follow,
    StateChanges,
    Unsubscribed,
}

impl ThreadMode {
    fn follows(self) -> bool {
        matches!(self, Self::Follow | Self::StateChanges)
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ThreadPreference {
    pub mode: ThreadMode,
    pub version: u64,
}

#[derive(Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ThreadSubscription {
    preference: ThreadPreference,
    after: u64,
}

fn thread_key(subject: Subject, actor: Uuid) -> Result<String> {
    if actor.is_nil() {
        return Err(Error::Invalid("Watch account is invalid"));
    }
    Ok(format!(
        "collaboration/v1/watches/discussions/{}/preferences/{actor}",
        subject.key()
    ))
}

fn thread_subscription(
    context: &Transaction<'_>,
    subject: Subject,
    actor: Uuid,
) -> Result<ThreadSubscription> {
    Ok(context
        .document(&thread_key(subject, actor)?)?
        .unwrap_or_default())
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ThreadAccount {
    pub subject: Subject,
    pub actor: Option<Uuid>,
}

pub(crate) struct GetThread;
impl Read for GetThread {
    const CLASS: &'static str = CLASS;
    const ID: u32 = 419;
    type Input = ThreadAccount;
    type Output = Option<ThreadPreference>;

    fn execute(context: &mut Transaction<'_>, input: ThreadAccount) -> Result<Self::Output> {
        if input.subject.closed(context)?.is_none() {
            return Ok(None);
        }
        let preference = match input.actor {
            Some(actor) => thread_subscription(context, input.subject, actor)?.preference,
            None => ThreadPreference::default(),
        };
        Ok(Some(preference))
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ThreadChange {
    pub subject: Subject,
    pub actor: Uuid,
    pub desired: ThreadPreference,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ThreadOutcome {
    Saved(ThreadPreference),
    Missing,
    Stale,
}

pub(crate) struct SetThread;
impl Mutation for SetThread {
    const CLASS: &'static str = CLASS;
    const ID: u32 = 419;
    type Input = ThreadChange;
    type Output = ThreadOutcome;

    fn execute(
        context: &mut Transaction<'_>,
        input: ThreadChange,
    ) -> Result<MutationResult<Self::Output>> {
        if input.subject.closed(context)?.is_none() {
            return Ok(MutationResult::Rejected(ThreadOutcome::Missing));
        }
        let mut current = thread_subscription(context, input.subject, input.actor)?;
        let mode = input.desired.mode;
        if current.preference.mode == mode {
            return Ok(MutationResult::Success(ThreadOutcome::Saved(
                current.preference,
            )));
        }
        if current.preference.version != input.desired.version {
            return Ok(MutationResult::Rejected(ThreadOutcome::Stale));
        }
        // Narrowing an existing follow keeps already selected state changes.
        // Other transitions start at the current activity boundary.
        if (current.preference.mode, mode) != (ThreadMode::Follow, ThreadMode::StateChanges) {
            current.after = activity::latest(context)?;
        }
        current.preference = ThreadPreference {
            mode,
            version: next_version(current.preference.version)?,
        };
        let index = format!(
            "{}{actor}",
            thread_index(input.subject),
            actor = input.actor
        );
        if mode.follows() {
            context.put_document(&index, &true)?;
        } else {
            context.delete_document(&index)?;
        }
        context.put_document(&thread_key(input.subject, input.actor)?, &current)?;
        Ok(MutationResult::Success(ThreadOutcome::Saved(
            current.preference,
        )))
    }
}

fn watcher_keys(
    context: &Transaction<'_>,
    prefix: &str,
    before: Option<Uuid>,
) -> Result<Vec<Uuid>> {
    let before = before.map(|actor| format!("{prefix}{actor}"));
    context
        .documents::<bool>(prefix, before.as_deref(), PAGE_SIZE + 1)?
        .into_iter()
        .map(|(key, _)| {
            key.strip_prefix(prefix)
                .and_then(|key| Uuid::parse_str(key).ok())
                .ok_or(Error::Invalid("Watch index key is invalid"))
        })
        .collect()
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Query {
    pub subject: Subject,
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
        let event = activity::event(context, input.activity, Some(input.subject))?
            .ok_or(Error::Invalid("Watch activity is missing"))?;
        let state_changed = matches!(
            event.action,
            activity::Action::Closed
                | activity::Action::IssueClosed { .. }
                | activity::Action::Reopened
                | activity::Action::Merged
        );
        // Merge both index pages before picking a cursor. A reader following
        // this discussion and the repository can receive only one notice.
        let rows: BTreeSet<_> = watcher_keys(context, WATCHERS, input.before)?
            .into_iter()
            .chain(watcher_keys(
                context,
                &thread_index(input.subject),
                input.before,
            )?)
            .collect();
        let more = rows.len() > PAGE_SIZE;
        let mut last = None;
        let mut recipients = Vec::new();
        for actor in rows.into_iter().rev().take(PAGE_SIZE) {
            last = Some(actor);
            let thread = thread_subscription(context, input.subject, actor)?;
            if thread.after >= input.activity {
                continue;
            }
            let enabled = match thread.preference.mode {
                ThreadMode::Unsubscribed => false,
                ThreadMode::Follow => true,
                ThreadMode::StateChanges => state_changed,
                ThreadMode::Inherit => {
                    let repo = subscription(context, actor)?;
                    let after = match input.subject.kind {
                        SubjectKind::Issue => repo.issues_after,
                        SubjectKind::Pull => repo.pulls_after,
                    };
                    after.is_some_and(|after| after < input.activity)
                }
            };
            if enabled {
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
