use super::*;
use crate::cells::repository::{GetPull, search};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(
    tag = "kind",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub(crate) enum Position {
    Latest,
    Before(Cursor),
    After(Cursor),
    Focus(Key),
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Query {
    pub subject: Subject,
    pub position: Position,
    pub limit: u8,
}
impl Query {
    pub(crate) fn validate(&self) -> std::result::Result<(), &'static str> {
        if self.subject.number == 0 || self.subject.number > repository::MAX_NUMBER {
            return Err("Conversation subject is invalid");
        }
        if !(1..=50).contains(&self.limit) {
            return Err("Conversation pages require 1–50 entries");
        }
        let key = match &self.position {
            Position::Latest => return Ok(()),
            Position::Focus(key) => key,
            Position::Before(cursor) | Position::After(cursor) => {
                if cursor.subject != self.subject
                    || cursor.generation.is_nil()
                    || cursor.created_at_ms > repository::MAX_NUMBER
                {
                    return Err("Conversation cursor belongs to another discussion or is invalid");
                }
                &cursor.key
            }
        };
        key.validate(self.subject)
            .map_err(|_| "Conversation position is invalid")
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReviewSource {
    pub head_ref: String,
    pub merged_head: Option<String>,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Page {
    pub items: Vec<Entry>,
    pub start: Option<Cursor>,
    pub end: Option<Cursor>,
    pub has_earlier: bool,
    pub has_later: bool,
    pub source: Option<ReviewSource>,
}

fn indexed(
    context: &Transaction<'_>,
    subject: Subject,
    generation: Uuid,
    key: Key,
) -> Result<Option<Cursor>> {
    key.validate(subject)?;
    let rows=context.query("SELECT created_at_ms FROM discussion_conversation WHERE kind = ? AND discussion = ? AND source = ? AND number = ?", &[SqlValue::Integer(kind(subject)),integer(subject.number)?,SqlValue::Integer(key.source.code()),integer(key.number)?])?;
    rows.rows
        .first()
        .map(|row| {
            Ok(Cursor {
                subject,
                generation,
                key,
                created_at_ms: result_u64_from_row(row, 0)?,
            })
        })
        .transpose()
}
fn cursor(
    context: &Transaction<'_>,
    subject: Subject,
    generation: Uuid,
    value: &Cursor,
) -> Result<()> {
    if value.subject != subject {
        return Err(Error::Invalid(
            "conversation cursor belongs to another discussion",
        ));
    }
    if value.generation != generation {
        return Err(Error::StaleSearch);
    }
    let current = indexed(context, subject, generation, value.key)?;
    if current.as_ref() != Some(value) {
        return Err(Error::StaleSearch);
    }
    Ok(())
}
fn parameters(subject: Subject, point: Option<&Cursor>) -> Result<Vec<SqlValue>> {
    let mut values = vec![SqlValue::Integer(kind(subject)), integer(subject.number)?];
    if let Some(point) = point {
        values.extend([
            integer(point.created_at_ms)?,
            SqlValue::Integer(point.key.source.code()),
            integer(point.key.number)?,
        ]);
    }
    Ok(values)
}
fn more(context: &Transaction<'_>, point: &Cursor, comparison: &str) -> Result<bool> {
    let sql = format!(
        "SELECT 1 FROM discussion_conversation WHERE kind = ? AND discussion = ? AND (created_at_ms, source, number) {comparison} (?, ?, ?) LIMIT 1"
    );
    Ok(!context
        .query(&sql, &parameters(point.subject, Some(point))?)?
        .rows
        .is_empty())
}

pub(crate) struct ListConversation;
impl Read for ListConversation {
    const CLASS: &'static str = repository::CLASS;
    const ID: u32 = 253;
    type Input = Query;
    type Output = Option<Page>;
    fn execute(context: &mut Transaction<'_>, input: Query) -> Result<Self::Output> {
        input.validate().map_err(Error::Invalid)?;
        let generation = search::index::ensure_ready(context)?;
        let source = if input.subject.kind == SubjectKind::Pull {
            let Some(pull) = GetPull::execute(context, input.subject.number)? else {
                return Ok(None);
            };
            Some(ReviewSource {
                head_ref: pull.head_ref,
                merged_head: pull.merge.map(|m| m.head_oid),
            })
        } else {
            if input.subject.closed(context)?.is_none() {
                return Ok(None);
            }
            None
        };
        let (point, comparison, ascending) = match &input.position {
            Position::Latest => (None, "", false),
            Position::Before(value) => {
                cursor(context, input.subject, generation, value)?;
                (Some(value.clone()), "<", false)
            }
            Position::After(value) => {
                cursor(context, input.subject, generation, value)?;
                (Some(value.clone()), ">", true)
            }
            Position::Focus(key) => {
                let Some(value) = indexed(context, input.subject, generation, *key)? else {
                    return Ok(None);
                };
                (Some(value), "<=", false)
            }
        };
        let mut values = parameters(input.subject, point.as_ref())?;
        let filter = if point.is_some() {
            format!(" AND (created_at_ms, source, number) {comparison} (?, ?, ?)")
        } else {
            String::new()
        };
        let order = if ascending { "ASC" } else { "DESC" };
        let sql = format!(
            "SELECT created_at_ms, source, number FROM discussion_conversation WHERE kind = ? AND discussion = ?{filter} ORDER BY created_at_ms {order}, source {order}, number {order} LIMIT ?"
        );
        values.push(SqlValue::Integer(i64::from(input.limit)));
        let rows = context.query(&sql, &values)?;
        let mut items = Vec::new();
        // Keep room for the two bound cursors, source metadata and reply envelope.
        // Load bodies by primary key only after the small ordered index seek.
        let mut used = 4096;
        for row in &rows.rows {
            let created_at_ms = result_u64_from_row(row, 0)?;
            let key = Key {
                source: Source::from_code(result_u64_from_row(row, 1)?)?,
                number: result_u64_from_row(row, 2)?,
            };
            let entry = load(context, input.subject, key)?;
            if entry.key() != key || entry.created_at_ms() != created_at_ms {
                return Err(Error::Invalid(
                    "conversation index does not match its source",
                ));
            }
            let bytes = serde_json::to_vec(&entry)?.len() + 1;
            if used + bytes > repository::MAX_LIST_OUTPUT_BYTES {
                if items.is_empty() {
                    return Err(Error::Invalid(
                        "conversation entry exceeds the page byte limit",
                    ));
                }
                break;
            }
            used += bytes;
            items.push(entry);
        }
        if !ascending {
            items.reverse();
        }
        let bound = |entry: &Entry| Cursor {
            subject: input.subject,
            generation,
            key: entry.key(),
            created_at_ms: entry.created_at_ms(),
        };
        let start = items.first().map(bound);
        let end = items.last().map(bound);
        let has_earlier = start
            .as_ref()
            .map(|point| more(context, point, "<"))
            .transpose()?
            .unwrap_or(false);
        let has_later = end
            .as_ref()
            .map(|point| more(context, point, ">"))
            .transpose()?
            .unwrap_or(false);
        Ok(Some(Page {
            items,
            start,
            end,
            has_earlier,
            has_later,
            source,
        }))
    }
}
