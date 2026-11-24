use super::*;
use crate::cells::{
    Error, Result,
    repository::{
        collaboration::SubjectSummary,
        search::text::{self, Fields},
    },
};

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReferenceInput {
    pub search: String,
    pub exact: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub author: Option<[u8; 32]>,
}

impl ReferenceInput {
    pub(crate) fn validate(&self) -> std::result::Result<(), &'static str> {
        if self.search.chars().count() > 80 || self.search.chars().any(char::is_control) {
            return Err("Reference search must contain at most 80 characters without controls");
        }
        if self.author.is_some() && !self.exact {
            return Err("An author requires an exact discussion reference");
        }
        Ok(())
    }
}

pub(crate) struct FindReferences;

impl Read for FindReferences {
    const CLASS: &'static str = crate::cells::repository::CLASS;
    const ID: u32 = 264;
    type Input = ReferenceInput;
    type Output = Vec<SubjectSummary>;

    fn execute(context: &mut Transaction<'_>, input: Self::Input) -> Result<Self::Output> {
        input.validate().map_err(Error::Invalid)?;
        ensure_ready(context)?;
        let search = input.search.trim();
        let mut values = Vec::new();
        let rows = if input.exact {
            let Some(number) = reference_number(search) else {
                return Ok(Vec::new());
            };
            context.query(
                "SELECT kind, number FROM discussion_index d WHERE rowid >= ?1 AND rowid < ?2
                 AND (?3 IS NULL OR EXISTS (SELECT 1 FROM discussion_index_authors a WHERE a.document = d.rowid AND a.identity = ?3))
                 ORDER BY rowid DESC",
                &[integer(number * 2)?, integer((number + 1) * 2)?, input.author.map_or(SqlValue::Null, |value| SqlValue::Blob(value.to_vec()))],
            )?
        } else if search.is_empty() {
            // Issue and pull numbers have separate sequences. Read bounded
            // head pages through each kind's creation index before merging.
            context.query(
                "SELECT kind, number FROM (
                    SELECT * FROM (SELECT kind, number, created_at_ms FROM discussion_index WHERE kind = 0 ORDER BY created_at_ms DESC, number DESC LIMIT 12)
                    UNION ALL
                    SELECT * FROM (SELECT kind, number, created_at_ms FROM discussion_index WHERE kind = 1 ORDER BY created_at_ms DESC, number DESC LIMIT 12)
                ) ORDER BY created_at_ms DESC, number DESC, kind DESC LIMIT 12",
                &[],
            )?
        } else if search.bytes().all(|byte| byte.is_ascii_digit()) {
            let Some(number) = reference_number(search) else {
                return Ok(Vec::new());
            };
            let mut ranges = Vec::new();
            let (mut first, mut end) = (number, number + 1);
            // Each decimal suffix length maps to an indexed rowid range.
            // A text cast would scan the index for a rare or missing number.
            while first <= MAX_NUMBER {
                ranges.push("d.rowid >= ? AND d.rowid < ?");
                values.push(integer(first * 2)?);
                values.push(integer(end.min(MAX_NUMBER + 1) * 2)?);
                first *= 10;
                end *= 10;
            }
            let filter = format!("({})", ranges.join(") OR ("));
            values.push(integer(number)?);
            context.query(
                &format!("SELECT d.kind, d.number FROM discussion_index d WHERE {filter} ORDER BY d.number = ? DESC, d.rowid DESC LIMIT 12"),
                &values,
            )?
        } else {
            let filter = text::predicate(&search.to_lowercase(), Fields::TITLE, |term| {
                values.push(SqlValue::Text(term.into()));
                "?1".into()
            });
            context.query(
                &format!("SELECT d.kind, d.number FROM discussion_index d WHERE {filter} ORDER BY d.created_at_ms DESC, d.rowid DESC LIMIT 12"),
                &values,
            )?
        };
        rows.rows
            .iter()
            .map(|row| {
                subject(row)?
                    .summary(context)?
                    .ok_or(Error::Invalid("Discussion reference source is missing"))
            })
            .collect()
    }
}

fn reference_number(search: &str) -> Option<u64> {
    if !search.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    search
        .parse::<u64>()
        .ok()
        .filter(|number| (1..=MAX_NUMBER).contains(number))
}
