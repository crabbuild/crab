use super::*;
use crate::cells::{Error, Result, repository::validate_number};
use std::collections::btree_map::Entry as MapEntry;

const PAGE_SIZE: usize = 50;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Position {
    comment: Option<u64>,
    entry: Entry,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum Entry {
    Actor { digest: [u8; 32], reaction: Kind },
    Counts,
}

impl Position {
    pub(crate) fn address(self) -> String {
        let suffix = match self.entry {
            Entry::Actor { digest, reaction } => {
                actor_suffix(blake3::Hash::from_bytes(digest), reaction)
            }
            Entry::Counts => "counts".into(),
        };
        match self.comment {
            Some(number) => format!("comment/{number:016x}/{suffix}"),
            None => suffix,
        }
    }

    fn parse(key: &str) -> Result<Self> {
        let (comment, suffix) = if let Some(rest) = key.strip_prefix("comment/") {
            let (number, suffix) = rest.split_once('/').ok_or_else(invalid)?;
            let number = u64::from_str_radix(number, 16).map_err(|_| invalid())?;
            validate_number(number)?;
            (Some(number), suffix)
        } else {
            (None, key)
        };
        let entry = if suffix == "counts" {
            Entry::Counts
        } else {
            let (digest, reaction) = suffix
                .strip_prefix("actors/")
                .and_then(|actor| actor.split_once('/'))
                .ok_or_else(invalid)?;
            let digest = *blake3::Hash::from_hex(digest)
                .map_err(|_| invalid())?
                .as_bytes();
            let reaction = Kind::ALL
                .into_iter()
                .find(|kind| kind.key() == reaction)
                .ok_or_else(invalid)?;
            Entry::Actor { digest, reaction }
        };
        let position = Self { comment, entry };
        if position.address() != key {
            return Err(invalid());
        }
        Ok(position)
    }

    fn validate(self) -> Result<()> {
        if let Some(number) = self.comment {
            validate_number(number)?;
        }
        Ok(())
    }
}

fn target(issue: u64, comment: Option<u64>) -> Target {
    match comment {
        Some(number) => Target::IssueComment { issue, number },
        None => Target::Issue { number: issue },
    }
}

fn invalid() -> Error {
    Error::Invalid("Issue transfer reaction ledger is invalid")
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Row {
    position: Position,
    counts: Option<Counts>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Page {
    pub after: Option<Position>,
    rows: Vec<Row>,
    pub next: Option<Position>,
}

impl Page {
    pub(in crate::cells::repository) fn validate(&self) -> Result<()> {
        if let Some(after) = self.after {
            after.validate()?;
        }
        if self.rows.len() > PAGE_SIZE
            || self.next.is_some_and(|next| {
                self.rows.len() != PAGE_SIZE
                    || self.rows.last().map(|row| row.position) != Some(next)
            })
        {
            return Err(invalid());
        }
        let mut previous = self.after.map(Position::address);
        for row in &self.rows {
            row.position.validate()?;
            let address = row.position.address();
            if previous.as_ref().is_some_and(|key| *key >= address) {
                return Err(invalid());
            }
            previous = Some(address);
            match (&row.position.entry, &row.counts) {
                (Entry::Actor { .. }, None) => {}
                (Entry::Counts, Some(counts))
                    if !counts.is_empty()
                        && counts
                            .values()
                            .all(|count| (1..=MAX_NUMBER).contains(count)) => {}
                _ => return Err(invalid()),
            }
        }
        Ok(())
    }
}

pub(in crate::cells::repository) fn capture(
    context: &Transaction<'_>,
    issue: u64,
    after: Option<Position>,
) -> Result<Page> {
    if let Some(after) = after {
        after.validate()?;
    }
    let prefix = target_prefix(Target::Issue { number: issue });
    let start = after.map(|position| format!("{prefix}{}", position.address()));
    let mut entries =
        context.documents_after::<serde_json::Value>(&prefix, start.as_deref(), PAGE_SIZE + 1)?;
    let more = entries.len() > PAGE_SIZE;
    entries.truncate(PAGE_SIZE);
    let rows = entries
        .into_iter()
        .map(|(key, value)| {
            let position = Position::parse(key.strip_prefix(&prefix).ok_or_else(invalid)?)?;
            let counts = match position.entry {
                Entry::Actor { .. } if value == serde_json::Value::Bool(true) => None,
                Entry::Actor { .. } => return Err(invalid()),
                Entry::Counts => Some(serde_json::from_value(value)?),
            };
            Ok(Row { position, counts })
        })
        .collect::<Result<Vec<_>>>()?;
    let next = rows.last().filter(|_| more).map(|row| row.position);
    let page = Page { after, rows, next };
    page.validate()?;
    Ok(page)
}

pub(in crate::cells::repository) fn install(
    context: &Transaction<'_>,
    issue: u64,
    page: Page,
) -> Result<()> {
    let prefix = target_prefix(Target::Issue { number: issue });
    let mut groups = BTreeMap::new();
    for row in page.rows {
        let derived = match groups.entry(row.position.comment) {
            MapEntry::Occupied(entry) => entry.into_mut(),
            MapEntry::Vacant(entry) => {
                let target = target(issue, row.position.comment);
                if !target.exists(context)? {
                    return Err(invalid());
                }
                entry.insert(counts(context, target)?)
            }
        };
        match row.position.entry {
            Entry::Actor { reaction, .. } => {
                // Actor hashes are the canonical membership keys. Preserve
                // those keys so a viewer can toggle their own reaction later.
                let key = format!("{prefix}{}", row.position.address());
                if membership(context, &key)? {
                    return Err(invalid());
                }
                context.put_document(&key, &true)?;
                let count = derived.entry(reaction).or_default();
                *count = count
                    .checked_add(1)
                    .filter(|count| *count <= MAX_NUMBER)
                    .ok_or_else(invalid)?;
            }
            Entry::Counts => {
                // Compare against the aggregate captured from the same native
                // snapshot, even when its actor members span multiple frames.
                if row.counts.as_ref() != Some(&*derived) {
                    return Err(invalid());
                }
            }
        }
    }
    for (comment, derived) in groups {
        context.put_document(&counts_key(target(issue, comment)), &derived)?;
    }
    Ok(())
}
