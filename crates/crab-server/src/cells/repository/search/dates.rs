use serde::Serialize;
use time::{
    Date, OffsetDateTime,
    format_description::well_known::{Iso8601, Rfc3339},
};

use super::SubjectKind;

#[derive(Clone, Copy, Debug, Serialize)]
enum Field {
    Created,
    Updated,
    Closed,
    Merged,
}

#[derive(Clone, Copy, Debug, Serialize)]
enum Bounds {
    AtLeast(i64),
    AtMost(i64),
    Between(i64, i64),
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct Filter {
    field: Field,
    bounds: Bounds,
}

impl Filter {
    pub(super) fn parse(qualifier: &str, value: &str) -> Result<Self, &'static str> {
        let field = match qualifier {
            "created" => Field::Created,
            "updated" => Field::Updated,
            "closed" => Field::Closed,
            "merged" => Field::Merged,
            _ => return Err("Unsupported date qualifier"),
        };
        let bounds = if let Some((start, end)) = value.split_once("..") {
            match (start, end) {
                ("*", "*") => return Err("Date ranges need at least one bound"),
                ("*", end) => Bounds::AtMost(period(end)?.1),
                (start, "*") => Bounds::AtLeast(period(start)?.0),
                (start, end) => {
                    let (start, end) = (period(start)?.0, period(end)?.1);
                    if start > end {
                        return Err("Date range lower bound must not exceed its upper bound");
                    }
                    Bounds::Between(start, end)
                }
            }
        } else if let Some(value) = value.strip_prefix(">=") {
            Bounds::AtLeast(period(value)?.0)
        } else if let Some(value) = value.strip_prefix("<=") {
            Bounds::AtMost(period(value)?.1)
        } else if let Some(value) = value.strip_prefix('>') {
            Bounds::AtLeast(period(value)?.1 + 1)
        } else if let Some(value) = value.strip_prefix('<') {
            Bounds::AtMost(period(value)?.0 - 1)
        } else {
            let (start, end) = period(value)?;
            Bounds::Between(start, end)
        };
        Ok(Self { field, bounds })
    }

    pub(super) fn needs_pull_lifecycle(&self) -> bool {
        matches!(self.field, Field::Closed | Field::Merged)
    }

    pub(super) fn sql(
        &self,
        kind: Option<SubjectKind>,
        mut bind: impl FnMut(i64) -> String,
    ) -> String {
        let mut within = |column: &str| match self.bounds {
            Bounds::AtLeast(start) => format!("{column} >= {}", bind(start)),
            Bounds::AtMost(end) => format!("{column} <= {}", bind(end)),
            Bounds::Between(start, end) => {
                format!("{column} BETWEEN {} AND {}", bind(start), bind(end))
            }
        };
        match self.field {
            Field::Created => within("d.created_at_ms"),
            Field::Updated => within("d.updated_at_ms"),
            Field::Closed => {
                let mut closed = Vec::new();
                if kind != Some(SubjectKind::Pull) {
                    closed.push(format!(
                        "d.kind=0 AND EXISTS (SELECT 1 FROM repository_issue_lifecycle il WHERE il.issue=d.number AND {})",
                        within("il.closed_at_ms")
                    ));
                }
                if kind != Some(SubjectKind::Issue) {
                    closed.push(format!(
                        "d.kind=1 AND EXISTS (SELECT 1 FROM repository_pull_lifecycle pl WHERE pl.pull=d.number AND {})",
                        within("pl.closed_at_ms")
                    ));
                }
                closed.join(" OR ")
            }
            Field::Merged if kind == Some(SubjectKind::Issue) => "0".into(),
            Field::Merged => format!(
                "d.kind=1 AND EXISTS (SELECT 1 FROM repository_pull_lifecycle pl WHERE pl.pull=d.number AND {})",
                within("pl.merged_at_ms")
            ),
        }
    }
}

fn period(value: &str) -> Result<(i64, i64), &'static str> {
    const DAY_MS: i64 = 86_400_000;
    const INVALID: &str = "Use YYYY-MM-DD or an RFC 3339 timestamp with a UTC offset";
    let bytes = value.as_bytes();
    if let [_, _, _, _, b'-', _, _, b'-', _, _] = bytes {
        let date = Date::parse(value, &Iso8601::DATE).map_err(|_| INVALID)?;
        let start = date.midnight().assume_utc().unix_timestamp() * 1_000;
        return Ok((start, start + DAY_MS - 1));
    }
    if !matches!(bytes.get(10), Some(b'T' | b't')) {
        return Err(INVALID);
    }
    let fractional = bytes.get(19) == Some(&b'.');
    if fractional
        && bytes[20..]
            .iter()
            .take_while(|digit| digit.is_ascii_digit())
            .count()
            > 3
    {
        return Err("Search timestamps support millisecond precision");
    }
    let instant = OffsetDateTime::parse(value, &Rfc3339).map_err(|_| INVALID)?;
    if !instant.nanosecond().is_multiple_of(1_000_000) {
        return Err("Search timestamps support millisecond precision");
    }
    let start = instant.unix_timestamp() * 1_000 + i64::from(instant.millisecond());
    // Whole seconds include every stored millisecond in that second.
    let end = if fractional { start } else { start + 999 };
    Ok((start, end))
}

#[cfg(test)]
mod tests;
