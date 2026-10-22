use serde::{Deserialize, Serialize};

use super::Query;
use crate::cells::application::{Read, Transaction};
use crate::cells::repository::{LabelRecord, ListLabels};

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CompletionInput {
    pub query: String,
    // Browser selection offsets count UTF-16 units, not UTF-8 bytes.
    pub cursor: u16,
}

impl CompletionInput {
    pub(crate) fn validate(&self) -> Result<(), &'static str> {
        replacement(self).map(|_| ())
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Suggestions {
    pub start: u16,
    pub end: u16,
    pub items: Vec<Suggestion>,
    pub error: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Suggestion {
    pub value: String,
    pub description: String,
}

pub(crate) struct CompleteSearch;

impl Read for CompleteSearch {
    const CLASS: &'static str = crate::cells::repository::CLASS;
    const ID: u32 = 260;
    type Input = CompletionInput;
    type Output = Suggestions;

    fn execute(
        context: &mut Transaction<'_>,
        input: Self::Input,
    ) -> crate::cells::Result<Self::Output> {
        let (start, end, cursor) = replacement(&input).map_err(crate::cells::Error::Invalid)?;
        let prefix = &input.query[start..cursor];
        let labels = if prefix
            .trim_start_matches('-')
            .to_ascii_lowercase()
            .starts_with("label:")
        {
            ListLabels::execute(context, ())?.labels
        } else {
            Vec::new()
        };
        let milestones = if let Some(value) = prefix
            .trim_start_matches('-')
            .to_lowercase()
            .strip_prefix("milestone:")
        {
            super::milestones::matching(context, &value_prefix(value), None)?
        } else {
            Vec::new()
        };
        suggestions(&input, start, end, cursor, &labels, &milestones)
    }
}

fn replacement(input: &CompletionInput) -> Result<(usize, usize, usize), &'static str> {
    if input.query.chars().count() > 256 || input.query.chars().any(char::is_control) {
        return Err("Search query must be at most 256 characters without controls");
    }
    let mut units = 0;
    let mut cursor = None;
    for (offset, character) in input.query.char_indices() {
        if units == usize::from(input.cursor) {
            cursor = Some(offset);
        }
        units += character.len_utf16();
    }
    if units == usize::from(input.cursor) {
        cursor = Some(input.query.len());
    }
    let cursor = cursor.ok_or("Search cursor must be a character boundary within the query")?;
    let mut start = 0;
    let mut quoted = false;
    let mut escaped = false;
    for (offset, character) in input.query.char_indices() {
        if !quoted && (character.is_whitespace() || character == '(' || character == ')') {
            if start < offset && (start..=offset).contains(&cursor) {
                return Ok((start, offset, cursor));
            }
            start = offset + character.len_utf8();
        } else if escaped {
            escaped = false;
        } else if quoted && character == '\\' {
            escaped = true;
        } else if character == '"' {
            quoted = !quoted;
        }
    }
    if start <= cursor {
        return Ok((start, input.query.len(), cursor));
    }
    Ok((cursor, cursor, cursor))
}

pub(crate) fn person_prefix(
    input: &CompletionInput,
) -> Result<Option<(String, String)>, &'static str> {
    let (start, _, cursor) = replacement(input)?;
    let prefix = input.query[start..cursor]
        .trim_start_matches('-')
        .to_ascii_lowercase();
    Ok(["author", "assignee"].into_iter().find_map(|kind| {
        prefix
            .strip_prefix(&format!("{kind}:"))
            .map(|value| (kind.into(), value_prefix(value)))
    }))
}

pub(crate) fn append_people(
    input: &CompletionInput,
    result: &mut Suggestions,
    qualifier: &str,
    handles: impl Iterator<Item = (String, String)>,
) -> Result<(), &'static str> {
    let (start, end, _) = replacement(input)?;
    let sign = if input.query[start..].starts_with('-') {
        "-"
    } else {
        ""
    };
    let retained = input.query[..start].chars().count() + input.query[end..].chars().count();
    for (handle, description) in handles {
        let value = format!("{sign}{qualifier}:{handle}");
        if retained + value.chars().count() <= 256 && result.items.len() < 20 {
            result.items.push(Suggestion { value, description });
        }
    }
    Ok(())
}

fn suggestions(
    input: &CompletionInput,
    start: usize,
    end: usize,
    cursor: usize,
    labels: &[LabelRecord],
    milestones: &[super::milestones::Summary],
) -> crate::cells::Result<Suggestions> {
    let prefix = &input.query[start..cursor];
    let negative = prefix.starts_with('-');
    let sign = if negative { "-" } else { "" };
    let prefix = prefix.trim_start_matches('-').to_lowercase();
    let mut items = Vec::new();
    if let Some(value) = prefix.strip_prefix("label:") {
        let (previous, prefix) = label_prefix(value);
        for label in labels {
            if !label.name.to_lowercase().contains(&prefix) {
                continue;
            }
            // JSON quoting matches the search grammar's quote/backslash escapes.
            let quoted = serde_json::to_string(&label.name)?;
            items.push(Suggestion {
                value: format!("{sign}label:{previous}{quoted}"),
                description: label.description.clone().unwrap_or_default(),
            });
            if items.len() == 20 {
                break;
            }
        }
    } else if prefix.starts_with("milestone:") {
        for milestone in milestones.iter().take(20) {
            items.push(Suggestion {
                value: format!(
                    "{sign}milestone:{}",
                    serde_json::to_string(&milestone.title)?
                ),
                description: match milestone.state {
                    crate::cells::repository::collaboration::milestones::MilestoneState::Open => {
                        "Open milestone"
                    }
                    crate::cells::repository::collaboration::milestones::MilestoneState::Closed => {
                        "Closed milestone"
                    }
                }
                .into(),
            });
        }
    } else if prefix.starts_with("author:") || prefix.starts_with("assignee:") {
        for (value, description) in [
            ("author:@me", "Work created by you"),
            ("assignee:@me", "Work assigned to you"),
            ("assignee:*", "Work with any assignee"),
        ] {
            if value.starts_with(&prefix) {
                items.push(Suggestion {
                    value: format!("{sign}{value}"),
                    description: description.into(),
                });
            }
        }
    } else {
        for (value, description) in [
            ("is:open", "Open work"),
            ("is:closed", "Closed work, including merged pull requests"),
            ("is:issue", "Issues"),
            ("is:pr", "Pull requests"),
            ("is:merged", "Merged pull requests"),
            ("is:unmerged", "Pull requests that have not merged"),
            ("draft:true", "Draft pull requests"),
            ("draft:false", "Non-draft pull requests"),
            ("label:", "Filter by repository label"),
            ("no:label", "Work without labels"),
            ("no:assignee", "Unassigned work"),
            ("milestone:", "Filter by repository milestone"),
            ("no:milestone", "Work without a milestone"),
            ("author:", "Filter by public username"),
            ("author:@me", "Work created by you"),
            ("assignee:", "Filter by assignee"),
            ("assignee:@me", "Work assigned to you"),
            ("assignee:*", "Work with any assignee"),
        ] {
            if negative && value.starts_with("no:") || !value.starts_with(&prefix) {
                continue;
            }
            items.push(Suggestion {
                value: format!("{sign}{value}"),
                description: description.into(),
            });
        }
    }
    items.retain(|item| {
        input.query[..start].chars().count()
            + item.value.chars().count()
            + input.query[end..].chars().count()
            <= 256
    });
    let error = Query::parse((!input.query.trim().is_empty()).then(|| input.query.trim()))
        .err()
        .map(str::to_owned);
    let start = u16::try_from(input.query[..start].encode_utf16().count())
        .map_err(|_| crate::cells::Error::Invalid("Search cursor exceeds its bound"))?;
    let end = u16::try_from(input.query[..end].encode_utf16().count())
        .map_err(|_| crate::cells::Error::Invalid("Search cursor exceeds its bound"))?;
    Ok(Suggestions {
        start,
        end,
        items,
        error,
    })
}

fn label_prefix(value: &str) -> (&str, String) {
    let mut quoted = false;
    let mut escaped = false;
    let mut start = 0;
    for (offset, character) in value.char_indices() {
        if escaped {
            escaped = false;
        } else if quoted && character == '\\' {
            escaped = true;
        } else if character == '"' {
            quoted = !quoted;
        } else if character == ',' && !quoted {
            start = offset + 1;
        }
    }
    (&value[..start], value_prefix(&value[start..]))
}

fn value_prefix(typed: &str) -> String {
    let Some(quoted) = typed.strip_prefix('"') else {
        return typed.to_owned();
    };
    let mut prefix = String::new();
    let mut characters = quoted.chars();
    while let Some(character) = characters.next() {
        match character {
            '"' => break,
            '\\' => {
                if let Some(escaped) = characters.next() {
                    prefix.push(escaped);
                }
            }
            _ => prefix.push(character),
        }
    }
    prefix
}

#[cfg(test)]
mod tests;
