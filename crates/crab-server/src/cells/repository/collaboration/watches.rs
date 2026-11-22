use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::{Subject, SubjectKind, activity, mentions, next_version};
use crate::cells::{
    Error, Result,
    application::{Mutation, MutationResult, Read, Transaction},
    directory::IdentityKey,
    repository::CLASS,
};

mod discussions;
pub(crate) use discussions::{
    DiscussionQuery, DiscussionSubscription, ListDiscussions, SelectedThread, ThreadSelection,
    UnsubscribeDiscussions,
};

mod chronology;
#[cfg(test)]
pub(crate) use chronology::tests::SetFollowTimes;
pub(crate) use chronology::{ListTimedDiscussions, TIMED_PAGE, TimeBound, TimedQuery};

mod inbox;
pub(crate) use inbox::{
    Dismiss, DismissItem, DismissSelection, InboxActivity, InboxVisibility, InboxVisibilityInput,
};

mod publications;
pub(crate) use publications::{
    AcknowledgeRelease, AcknowledgeReleaseInput, PublishedReleases, ReleaseEvent, ReleaseSource,
    ReleaseWatchers,
};
pub(in crate::cells::repository) use publications::{pending as pending_releases, published};

const PREFERENCES: &str = "collaboration/v2/watches/preferences/";
const WATCHERS: &str = "collaboration/v2/watches/active/";
const PAGE_SIZE: usize = 32;

pub(crate) type Key = [u8; 32];

pub(crate) fn identity_key(actor: &IdentityKey) -> Result<Key> {
    actor.validate()?;
    let mut hash = blake3::Hasher::new_derive_key("crab repository notification account v1");
    hash.update(actor.issuer.as_bytes());
    hash.update(b"\0");
    hash.update(actor.subject.as_bytes());
    Ok(*hash.finalize().as_bytes())
}

fn key_text(key: Key) -> String {
    blake3::Hash::from_bytes(key).to_hex().to_string()
}

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
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub releases: bool,
}

#[derive(Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Subscription {
    preference: Preference,
    issues_after: Option<u64>,
    pulls_after: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    releases_after: Option<u64>,
}

fn subscription(context: &Transaction<'_>, actor: Key) -> Result<Subscription> {
    Ok(context
        .document(&format!("{PREFERENCES}{}", key_text(actor)))?
        .unwrap_or_default())
}

pub(crate) struct Get;
impl Read for Get {
    const CLASS: &'static str = CLASS;
    const ID: u32 = 416;
    type Input = IdentityKey;
    type Output = Preference;

    fn execute(context: &mut Transaction<'_>, actor: IdentityKey) -> Result<Preference> {
        Ok(subscription(context, identity_key(&actor)?)?.preference)
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Change {
    pub actor: IdentityKey,
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
        let key = identity_key(&input.actor)?;
        let mut current = subscription(context, key)?;
        let mode = input.desired.mode;
        let releases = mode != Mode::Muted && input.desired.releases;
        if current.preference.mode == mode && current.preference.releases == releases {
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
        let suffix = key_text(key);
        if current.preference.releases != releases {
            current.releases_after = if releases {
                Some(publications::watermark(context)?)
            } else {
                None
            };
            publications::update_index(context, &suffix, &input.actor, releases)?;
        }
        current.preference = Preference {
            mode,
            version: next_version(current.preference.version)?,
            releases,
        };
        let watcher = format!("{WATCHERS}{suffix}");
        // The sparse index is updated with the preference. Version tombstones
        // fence stale writes after unwatching without keeping an active row.
        if current.issues_after.is_some() || current.pulls_after.is_some() {
            context.put_document(&watcher, &input.actor)?;
        } else {
            context.delete_document(&watcher)?;
        }
        context.put_document(&format!("{PREFERENCES}{suffix}"), &current)?;
        Ok(MutationResult::Success(Some(current.preference)))
    }
}

fn thread_index(subject: Subject) -> String {
    format!(
        "collaboration/v2/watches/discussions/{}/active/",
        subject.key()
    )
}

pub(super) fn active(context: &Transaction<'_>, source: &activity::Event) -> Result<bool> {
    // An author-only subscription needs no outbox work. Two rows suffice to
    // find another watcher because each index holds at most one row per actor.
    let has_recipient = |prefix: &str| {
        Ok::<_, Error>(
            context
                .documents::<IdentityKey>(prefix, None, 2)?
                .into_iter()
                .any(|(_, actor)| {
                    actor.issuer != source.actor.issuer || actor.subject != source.actor.subject
                }),
        )
    };
    Ok(has_recipient(WATCHERS)? || has_recipient(&thread_index(source.target.subject()))?)
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

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SubscriptionReason {
    Manual,
    Participation,
    Mention,
    Assignment,
    Unknown,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct SubscriptionOrigin {
    pub reason: SubscriptionReason,
    pub subscribed_at_ms: u64,
}

#[derive(Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ThreadSubscription {
    preference: ThreadPreference,
    after: u64,
    #[serde(default)]
    choice_after: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    origin: Option<SubscriptionOrigin>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    dismissed: Option<inbox::Dismissal>,
}

fn thread_key(subject: Subject, actor: Key) -> String {
    format!(
        "collaboration/v2/watches/discussions/{}/preferences/{}",
        subject.key(),
        key_text(actor)
    )
}

fn thread_subscription(
    context: &Transaction<'_>,
    subject: Subject,
    actor: Key,
) -> Result<ThreadSubscription> {
    Ok(context
        .document(&thread_key(subject, actor))?
        .unwrap_or_default())
}

fn save_thread(
    context: &Transaction<'_>,
    subject: Subject,
    actor: &IdentityKey,
    key: Key,
    current: &mut ThreadSubscription,
    mode: ThreadMode,
    activation: Option<SubscriptionOrigin>,
) -> Result<ThreadPreference> {
    let origin = mode
        .follows()
        .then(|| {
            if current.preference.mode.follows() {
                current.origin
            } else {
                activation
            }
        })
        .flatten();
    if current.origin != origin {
        chronology::update(context, key, subject, current.origin, origin)?;
        current.origin = origin;
    }
    current.preference = ThreadPreference {
        mode,
        version: next_version(current.preference.version)?,
    };
    let index = format!("{}{}", thread_index(subject), key_text(key));
    if mode.follows() {
        context.put_document(&index, actor)?;
    } else {
        context.delete_document(&index)?;
    }
    context.put_document(&thread_key(subject, key), current)?;
    Ok(current.preference)
}

pub(super) fn participate(context: &Transaction<'_>, source: &activity::Event) -> Result<()> {
    use activity::Action;

    if source.actor.issuer.starts_with("urn:crab:")
        || !matches!(
            source.action,
            Action::Created
                | Action::Approved
                | Action::ChangesRequested
                | Action::Commented
                | Action::Closed
                | Action::IssueClosed { .. }
                | Action::Reopened
                | Action::Merged
        )
    {
        return Ok(());
    }
    let actor = IdentityKey {
        issuer: source.actor.issuer.clone(),
        subject: source.actor.subject.clone(),
    };
    follow_recipient(context, source, &actor, SubscriptionReason::Participation)
}

pub(super) fn follow_recipient(
    context: &Transaction<'_>,
    source: &activity::Event,
    actor: &IdentityKey,
    reason: SubscriptionReason,
) -> Result<()> {
    let can_resume = reason != SubscriptionReason::Assignment;
    let key = identity_key(actor)?;
    if subscription(context, key)?.preference.mode == Mode::Muted {
        return Ok(());
    }
    let subject = source.target.subject();
    let mut current = thread_subscription(context, subject, key)?;
    if !can_resume
        && current.preference.mode == ThreadMode::Unsubscribed
        && current.dismissed.is_some()
    {
        return Ok(());
    }
    // A mention can be processed after the reader makes a newer choice.
    // The source sequence, rather than delivery time, decides which wins.
    if current.preference.mode != ThreadMode::Follow
        && current.after.max(current.choice_after) < source.number
    {
        current.after = source.number;
        if can_resume && let Some(dismissal) = &mut current.dismissed {
            dismissal.resume();
        }
        save_thread(
            context,
            subject,
            actor,
            key,
            &mut current,
            ThreadMode::Follow,
            Some(SubscriptionOrigin {
                reason,
                subscribed_at_ms: source.created_at,
            }),
        )?;
    }
    Ok(())
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ThreadAccount {
    pub subject: Subject,
    pub actor: IdentityKey,
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
        let preference =
            thread_subscription(context, input.subject, identity_key(&input.actor)?)?.preference;
        Ok(Some(preference))
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ThreadChange {
    pub subject: Subject,
    pub actor: IdentityKey,
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
        let key = identity_key(&input.actor)?;
        let mut current = thread_subscription(context, input.subject, key)?;
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
        current.choice_after = activity::latest(context)?;
        if (current.preference.mode, mode) != (ThreadMode::Follow, ThreadMode::StateChanges) {
            current.after = current.choice_after;
        }
        if mode != ThreadMode::Unsubscribed
            && let Some(dismissal) = &mut current.dismissed
        {
            dismissal.resume();
        }
        let activation = if mode.follows() && !current.preference.mode.follows() {
            let subscribed_at_ms = u64::try_from(context.now_ms())
                .map_err(|_| Error::Invalid("Subscription time is invalid"))?;
            Some(SubscriptionOrigin {
                reason: SubscriptionReason::Manual,
                subscribed_at_ms,
            })
        } else {
            None
        };
        let saved = save_thread(
            context,
            input.subject,
            &input.actor,
            key,
            &mut current,
            mode,
            activation,
        )?;
        Ok(MutationResult::Success(ThreadOutcome::Saved(saved)))
    }
}

fn watcher_keys(
    context: &Transaction<'_>,
    prefix: &str,
    before: Option<Key>,
) -> Result<Vec<(Key, IdentityKey)>> {
    let before = before.map(|actor| format!("{prefix}{}", key_text(actor)));
    context
        .documents::<IdentityKey>(prefix, before.as_deref(), PAGE_SIZE + 1)?
        .into_iter()
        .map(|(key, actor)| {
            let expected = identity_key(&actor)?;
            if key.strip_prefix(prefix) != Some(key_text(expected).as_str()) {
                return Err(Error::Invalid("Watch index key is invalid"));
            }
            Ok((expected, actor))
        })
        .collect()
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Query {
    pub subject: Subject,
    pub activity: u64,
    pub before: Option<Key>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct WatchPage {
    pub recipients: Vec<IdentityKey>,
    pub next: Option<Key>,
}

pub(crate) struct ListWatchers;
impl Read for ListWatchers {
    const CLASS: &'static str = CLASS;
    const ID: u32 = 417;
    type Input = Query;
    type Output = Option<WatchPage>;

    fn execute(context: &mut Transaction<'_>, input: Query) -> Result<Self::Output> {
        if mentions::earlier_follow(context, input.subject, input.activity)? {
            return Ok(None);
        }
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
        let rows: BTreeMap<_, _> = watcher_keys(context, WATCHERS, input.before)?
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
        for (key, actor) in rows.into_iter().rev().take(PAGE_SIZE) {
            last = Some(key);
            let repo = subscription(context, key)?;
            if repo.preference.mode == Mode::Muted
                || (actor.issuer == event.actor.issuer && actor.subject == event.actor.subject)
            {
                continue;
            }
            let thread = thread_subscription(context, input.subject, key)?;
            if thread.after >= input.activity {
                continue;
            }
            let enabled = match thread.preference.mode {
                ThreadMode::Unsubscribed => false,
                ThreadMode::Follow => true,
                ThreadMode::StateChanges => state_changed,
                ThreadMode::Inherit => {
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
        Ok(Some(WatchPage {
            recipients,
            next: last.filter(|_| more),
        }))
    }
}

pub(crate) struct Unmuted;
impl Read for Unmuted {
    const CLASS: &'static str = CLASS;
    const ID: u32 = 418;
    type Input = Vec<IdentityKey>;
    type Output = Vec<bool>;

    fn execute(context: &mut Transaction<'_>, actors: Vec<IdentityKey>) -> Result<Vec<bool>> {
        if actors.len() > crate::cells::directory::notifications::MAX_RECIPIENTS {
            return Err(Error::Invalid("Notification preference batch is too large"));
        }
        actors
            .iter()
            .map(|actor| {
                Ok(subscription(context, identity_key(actor)?)?.preference.mode != Mode::Muted)
            })
            .collect()
    }
}
