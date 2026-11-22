use std::{collections::BTreeSet, ops::Range};

use pulldown_cmark::{Event as MarkdownEvent, LinkType, Options, Parser, Tag, TagEnd};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::cells::{
    Error, Result,
    application::{Mutation, MutationResult, Read, Transaction},
    directory::{IdentityKey, validate_handle},
    repository::{self, RepositoryAuthor, assignments::Assignee, timestamp, validate_number},
};

use super::{
    Target,
    mention_settings::{self, Settings},
    watches,
};

const OUTBOX: &str = "collaboration/v1/mentions/outbox/";
const MAX_MENTIONS: usize = 20;
const BATCH: usize = 32;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Mention {
    Actor(Uuid),
    Handle(String),
    Team { organization: Uuid, team: Uuid },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Event {
    pub id: Uuid,
    pub target: Target,
    pub version: u64,
    pub actor: RepositoryAuthor,
    pub created_at_ms: u64,
    pub previous: Vec<Mention>,
    pub current: Vec<Mention>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reviewers: Vec<IdentityKey>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub assignees: Vec<Uuid>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub watch: Option<Watch>,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct Watch {
    pub before: Option<Uuid>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct EventKey {
    pub id: Uuid,
    pub created_at_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub watch: Option<Watch>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next: Option<Uuid>,
}

fn key(id: Uuid, created_at_ms: u64) -> String {
    // Cellar document pages descend by key. Inverted time and UUID keep
    // queued events in oldest-first order, including same-clock writes.
    format!("{OUTBOX}{:016x}/{:032x}", !created_at_ms, !id.as_u128())
}

pub(crate) struct Pending;
impl Read for Pending {
    const CLASS: &'static str = repository::CLASS;
    const ID: u32 = 336;
    type Input = ();
    type Output = Vec<Event>;

    fn execute(context: &mut Transaction<'_>, (): ()) -> Result<Vec<Event>> {
        context
            .documents::<Event>(OUTBOX, None, BATCH)?
            .into_iter()
            .map(|(stored_key, event)| {
                if stored_key != key(event.id, event.created_at_ms) {
                    return Err(Error::Invalid(
                        "Mention event key does not match its record",
                    ));
                }
                Ok(event)
            })
            .collect()
    }
}

pub(crate) struct Acknowledge;
impl Mutation for Acknowledge {
    const CLASS: &'static str = repository::CLASS;
    const ID: u32 = 337;
    type Input = EventKey;
    type Output = ();

    fn execute(context: &mut Transaction<'_>, input: EventKey) -> Result<MutationResult<()>> {
        let key = key(input.id, input.created_at_ms);
        if let Some(expected) = input.watch {
            let Some(mut event) = context.document::<Event>(&key)? else {
                return Ok(MutationResult::Success(()));
            };
            if event.watch != Some(expected) {
                return Ok(MutationResult::Success(()));
            }
            if let Some(next) = input.next {
                // Another worker may have replayed this page. Advance only
                // from the cursor used to select its recipients.
                event.watch = Some(Watch { before: Some(next) });
                context.put_document(&key, &event)?;
                return Ok(MutationResult::Success(()));
            }
        }
        context.delete_document(&key)?;
        Ok(MutationResult::Success(()))
    }
}

pub(in crate::cells::repository) fn record(
    context: &Transaction<'_>,
    target: Target,
    actor: &RepositoryAuthor,
    version: u64,
    previous: Option<&str>,
    body: &str,
) -> Result<()> {
    if !body.contains('@') || previous == Some(body) {
        return Ok(());
    }
    let settings = mention_settings::read(context)?;
    let (Some(current), Some(previous)) = (
        mentions(body, &settings),
        mentions(previous.unwrap_or_default(), &settings),
    ) else {
        // Over-limit documents remain valid Markdown. Skip their delivery
        // rather than let truncation turn old mentions into new recipients.
        return Ok(());
    };
    if current.difference(&previous).next().is_none() {
        return Ok(());
    }
    target.validate()?;
    validate_number(version)?;
    let event = Event {
        id: Uuid::now_v7(),
        target,
        version,
        actor: actor.clone(),
        created_at_ms: timestamp(context.now_ms())?,
        previous: previous.into_iter().collect(),
        current: current.into_iter().collect(),
        reviewers: Vec::new(),
        assignees: Vec::new(),
        watch: None,
    };
    context.put_document(&key(event.id, event.created_at_ms), &event)
}

pub(in crate::cells::repository) fn review_requested(
    context: &Transaction<'_>,
    source: &super::activity::Event,
    issuer: &str,
    subjects: impl IntoIterator<Item = String>,
) -> Result<()> {
    let reviewers: Vec<_> = subjects
        .into_iter()
        .filter(|subject| issuer != source.actor.issuer || *subject != source.actor.subject)
        .map(|subject| IdentityKey {
            issuer: issuer.to_owned(),
            subject,
        })
        .collect();
    for recipient in &reviewers {
        recipient.validate()?;
    }
    // Keep the oldest-first outbox page bounded for long qualified identities.
    for reviewers in reviewers.chunks(16) {
        let event = Event {
            id: Uuid::now_v7(),
            target: source.target,
            version: source.number,
            actor: source.actor.clone(),
            created_at_ms: source.created_at,
            previous: Vec::new(),
            current: Vec::new(),
            reviewers: reviewers.to_vec(),
            assignees: Vec::new(),
            watch: None,
        };
        context.put_document(&key(event.id, event.created_at_ms), &event)?;
    }
    Ok(())
}

pub(in crate::cells::repository) fn assigned(
    context: &Transaction<'_>,
    target: Target,
    actor: &RepositoryAuthor,
    version: u64,
    previous: &[Assignee],
    current: &[Assignee],
) -> Result<()> {
    // Native assignment records are sorted by Directory ID. Name changes
    // and users retained by an assignment replacement do not notify again.
    let assignees: Vec<_> = current
        .iter()
        .map(|assignee| assignee.id)
        .filter(|id| {
            previous
                .binary_search_by_key(id, |assignee| assignee.id)
                .is_err()
        })
        .collect();
    if assignees.is_empty() {
        return Ok(());
    }
    let event = Event {
        id: Uuid::now_v7(),
        target,
        version,
        actor: actor.clone(),
        created_at_ms: timestamp(context.now_ms())?,
        previous: Vec::new(),
        current: Vec::new(),
        reviewers: Vec::new(),
        assignees,
        watch: None,
    };
    context.put_document(&key(event.id, event.created_at_ms), &event)
}

pub(super) fn watched(context: &Transaction<'_>, source: &super::activity::Event) -> Result<()> {
    if !watches::active(context)? {
        return Ok(());
    }
    let event = Event {
        id: Uuid::now_v7(),
        target: source.target,
        version: source.number,
        actor: source.actor.clone(),
        created_at_ms: source.created_at,
        previous: Vec::new(),
        current: Vec::new(),
        reviewers: Vec::new(),
        assignees: Vec::new(),
        watch: Some(Watch::default()),
    };
    context.put_document(&key(event.id, event.created_at_ms), &event)
}

fn mentions(body: &str, settings: &Settings) -> Option<BTreeSet<Mention>> {
    let mut result = BTreeSet::new();
    let mut skip = 0_usize;
    let mut html = Vec::new();
    let parser = Parser::new_ext(
        body,
        Options::ENABLE_TABLES
            | Options::ENABLE_STRIKETHROUGH
            | Options::ENABLE_FOOTNOTES
            | Options::ENABLE_TASKLISTS,
    );
    for (event, range) in parser.into_offset_iter() {
        match event {
            MarkdownEvent::Start(Tag::Link {
                dest_url,
                link_type,
                ..
            }) => {
                if skip == 0
                    && html.is_empty()
                    && link_type == LinkType::Inline
                    && let Some(mention) = selected(&body[range], &dest_url, settings)
                {
                    result.insert(mention);
                }
                skip += 1;
            }
            MarkdownEvent::Start(Tag::CodeBlock(_) | Tag::Image { .. }) => skip += 1,
            MarkdownEvent::End(TagEnd::CodeBlock | TagEnd::Image | TagEnd::Link) => skip -= 1,
            MarkdownEvent::InlineHtml(_) if skip == 0 => track_html(&mut html, &body[range]),
            MarkdownEvent::Text(_) if skip == 0 && html.is_empty() => {
                for mention in raw_mentions(body, range, settings) {
                    result.insert(mention);
                    if result.len() > MAX_MENTIONS {
                        return None;
                    }
                }
            }
            _ => {}
        }
        if result.len() > MAX_MENTIONS {
            return None;
        }
    }
    Some(result)
}

fn track_html<'a>(stack: &mut Vec<&'a str>, raw: &'a str) {
    let raw = raw.strip_prefix('<').unwrap_or_default();
    let closing = raw.starts_with('/');
    let tag = raw
        .trim_start_matches('/')
        .split(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '-'))
        .next()
        .unwrap_or_default();
    if tag.is_empty() {
        return;
    }
    if closing {
        if let Some(position) = stack
            .iter()
            .rposition(|open| open.eq_ignore_ascii_case(tag))
        {
            stack.truncate(position);
        }
    } else if !raw.ends_with("/>")
        && ![
            "area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "param",
            "source", "track", "wbr",
        ]
        .iter()
        .any(|void| void.eq_ignore_ascii_case(tag))
    {
        stack.push(tag);
    }
}

fn selected(source: &str, destination: &str, settings: &Settings) -> Option<Mention> {
    let (label, _) = source.strip_prefix("[@")?.split_once(']')?;
    if let Some(actor) = destination.strip_prefix("/-/users/") {
        validate_handle(&label.to_ascii_lowercase()).ok()?;
        return canonical_uuid(actor).map(Mention::Actor);
    }
    let (organization, team) = destination
        .strip_prefix("/-/organizations/")?
        .split_once("?tab=teams&team=")?;
    let (organization, team) = (canonical_uuid(organization)?, canonical_uuid(team)?);
    (Some(organization) == settings.organization
        && label.split_once('/').is_some()
        && settings.teams.iter().any(|alias| alias.id == team))
    .then_some(Mention::Team { organization, team })
}

fn canonical_uuid(value: &str) -> Option<Uuid> {
    let id = Uuid::parse_str(value).ok()?;
    (!id.is_nil() && id.hyphenated().to_string() == value).then_some(id)
}

fn token_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'/')
}

fn raw_mentions<'a>(
    body: &'a str,
    range: Range<usize>,
    settings: &'a Settings,
) -> impl Iterator<Item = Mention> + 'a {
    body[range.clone()]
        .match_indices('@')
        .filter_map(move |(offset, _)| {
            let start = range.start + offset;
            // Markdown events carry decoded text. Use the original bytes to keep
            // escaped punctuation and entities from turning into notifications.
            let prefix = &body[..start];
            let before = prefix.trim_end_matches('\\');
            if (prefix.len() - before.len()) % 2 != 0
                || before.chars().next_back().is_some_and(|ch| {
                    !(ch.is_whitespace() || matches!(ch, '(' | '[' | '{' | '"' | '\'' | '*' | '~'))
                })
            {
                return None;
            }
            let tail = &body[start + 1..range.end];
            let end = tail.bytes().take_while(|byte| token_byte(*byte)).count();
            let token = &tail[..end];
            let after = &body[start + 1 + end..];
            if after.chars().next().is_some_and(|ch| {
                ch == '@' || ch.is_alphanumeric() || (ch.is_ascii() && token_byte(ch as u8))
            }) {
                return None;
            }
            if let Some((namespace, slug)) = token.split_once('/') {
                let organization = settings.organization?;
                if !settings.namespace.eq_ignore_ascii_case(namespace) {
                    return None;
                }
                let team = settings
                    .teams
                    .iter()
                    .find(|alias| alias.slug.eq_ignore_ascii_case(slug))?
                    .id;
                return Some(Mention::Team { organization, team });
            }
            let handle = token.to_ascii_lowercase();
            validate_handle(&handle).ok()?;
            Some(Mention::Handle(handle))
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cells::repository::collaboration::mention_settings::Alias;

    fn settings() -> Settings {
        Settings {
            organization: Some(Uuid::from_u128(1)),
            namespace: "crab".into(),
            teams: vec![Alias {
                id: Uuid::from_u128(2),
                slug: "core".into(),
            }],
            version: 1,
        }
    }

    #[test]
    fn notices_follow_markdown_tokens_and_stable_picker_links() {
        let settings = settings();
        let organization = settings.organization.unwrap();
        let team = settings.teams[0].id;
        let actor = Uuid::from_u128(3);
        let body = format!(
            "Hi @BOB, @bob, and @crab/core. [@old](/-/users/{actor}) and [@crab/core](/-/organizations/{organization}?tab=teams&team={team}).\n\n[@ordinary](https://example.com) ![@image](img.png) <span>@inside</span> `@code` \\@escaped &#64;entity name@example.com @crab/unknown"
        );
        assert_eq!(
            mentions(&body, &settings).unwrap(),
            BTreeSet::from([
                Mention::Handle("bob".into()),
                Mention::Actor(actor),
                Mention::Team { organization, team },
            ])
        );
    }

    #[test]
    fn an_oversized_mention_set_is_not_truncated_to_a_false_diff() {
        let body = (0..=MAX_MENTIONS)
            .map(|index| format!("@person{index} "))
            .collect::<String>();
        assert_eq!(mentions(&body, &settings()), None);
    }
}
