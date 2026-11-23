use super::*;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS repository_issue_type_index (
    issue INTEGER PRIMARY KEY REFERENCES repository_issues(number) ON DELETE CASCADE,
    organization BLOB NOT NULL CHECK(length(organization) = 16),
    ownership_epoch INTEGER NOT NULL CHECK(ownership_epoch >= 0),
    type_id BLOB NOT NULL CHECK(length(type_id) = 16)
) STRICT;
CREATE INDEX IF NOT EXISTS repository_issue_type_lookup
    ON repository_issue_type_index(organization, ownership_epoch, type_id, issue);";

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
pub(crate) enum Filter {
    Type(Uuid),
    Missing,
}

impl Filter {
    pub(crate) fn parse(value: &str) -> Result<Self, &'static str> {
        if value == "none" {
            return Ok(Self::Missing);
        }
        Uuid::parse_str(value)
            .ok()
            .filter(|value| !value.is_nil())
            .map(Self::Type)
            .ok_or("Project issue type must be a type ID or none")
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Scope {
    pub organization: Uuid,
    pub ownership_epoch: u32,
    pub filter: Filter,
    pub types: Vec<Uuid>,
}

impl Scope {
    pub(in crate::cells::repository) fn predicate(
        &self,
        context: &Transaction<'_>,
    ) -> crate::cells::Result<(String, Vec<SqlValue>)> {
        if self.organization.is_nil() || self.types.len() > 25 {
            return Err(crate::cells::Error::Invalid(
                "Project issue type scope is invalid",
            ));
        }
        super::super::super::search::index::ensure_ready(context)?;
        let (operator, types) = match self.filter {
            Filter::Type(id) if self.types.contains(&id) => ("IN", vec![id]),
            Filter::Type(_) => return Ok(("0".into(), Vec::new())),
            Filter::Missing if self.types.is_empty() => {
                return Ok(("i.number IS NOT NULL".into(), Vec::new()));
            }
            Filter::Missing => ("NOT IN", self.types.clone()),
        };
        let placeholders = vec!["?"; types.len()].join(",");
        let mut values = vec![
            SqlValue::Blob(self.organization.as_bytes().to_vec()),
            SqlValue::Integer(i64::from(self.ownership_epoch)),
        ];
        values.extend(
            types
                .into_iter()
                .map(|id| SqlValue::Blob(id.as_bytes().to_vec())),
        );
        Ok((
            format!(
                "i.number IS NOT NULL AND i.number {operator} (SELECT issue FROM repository_issue_type_index WHERE organization=? AND ownership_epoch=? AND type_id IN ({placeholders}))"
            ),
            values,
        ))
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct MatchInput {
    pub numbers: Vec<u64>,
    pub scope: Scope,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Matches {
    pub numbers: Vec<u64>,
    pub generation: Uuid,
    pub revision: u64,
}

pub(crate) struct Match;
impl Read for Match {
    const CLASS: &'static str = crate::cells::repository::CLASS;
    const ID: u32 = 292;
    type Input = MatchInput;
    type Output = Matches;

    fn execute(context: &mut Transaction<'_>, input: MatchInput) -> crate::cells::Result<Matches> {
        if input.numbers.len() > 5000
            || input
                .numbers
                .iter()
                .any(|number| validate_number(*number).is_err())
        {
            return Err(crate::cells::Error::Invalid(
                "Project issue type candidates are invalid",
            ));
        }
        let index =
            &crate::cells::repository::search::index::GetSearchIndexStatus::execute(context, ())?;
        let crate::cells::repository::search::index::SearchIndexStatus::Ready {
            generation,
            revision,
        } = index
        else {
            return Err(crate::cells::Error::IndexUnavailable);
        };
        let (predicate, conditions) = input.scope.predicate(context)?;
        let mut values = vec![SqlValue::Text(serde_json::to_string(&input.numbers)?)];
        values.extend(conditions);
        let rows = context.query(
            &format!("SELECT i.number FROM json_each(?) wanted JOIN repository_issues i ON i.number=wanted.value WHERE {predicate} ORDER BY i.number"),
            &values,
        )?;
        let numbers = rows
            .rows
            .iter()
            .map(|row| result_u64_from_row(row, 0))
            .collect::<crate::cells::Result<_>>()?;
        Ok(Matches {
            numbers,
            generation: *generation,
            revision: *revision,
        })
    }
}

pub(in crate::cells::repository) fn install(context: &Transaction<'_>) -> crate::cells::Result<()> {
    context.install_schema(SCHEMA)
}

pub(super) fn project(
    context: &Transaction<'_>,
    number: u64,
    selection: Option<Selection>,
) -> crate::cells::Result<()> {
    if let Some(selection) = selection {
        context.query(
            "INSERT INTO repository_issue_type_index(issue, organization, ownership_epoch, type_id)
             VALUES (?, ?, ?, ?) ON CONFLICT(issue) DO UPDATE SET
             organization = excluded.organization, ownership_epoch = excluded.ownership_epoch,
             type_id = excluded.type_id",
            &[
                integer(number)?,
                SqlValue::Blob(selection.organization.as_bytes().to_vec()),
                SqlValue::Integer(i64::from(selection.ownership_epoch)),
                SqlValue::Blob(selection.id.as_bytes().to_vec()),
            ],
        )?;
    } else {
        context.query(
            "DELETE FROM repository_issue_type_index WHERE issue = ?",
            &[integer(number)?],
        )?;
    }
    Ok(())
}

pub(in crate::cells::repository) fn backfill(
    context: &Transaction<'_>,
    after: &mut u64,
    limit: usize,
) -> crate::cells::Result<bool> {
    let cursor = (*after != 0).then(|| key(*after));
    // Only issues with a stored assignment can contribute an index row.
    // The prepared index remains unavailable until every page commits.
    let rows = context.documents_after::<Assignment>(PREFIX, cursor.as_deref(), limit)?;
    for (key, assignment) in &rows {
        let number = key
            .strip_prefix(PREFIX)
            .and_then(|key| u64::from_str_radix(key, 16).ok())
            .ok_or(crate::cells::Error::Invalid("Issue type key is invalid"))?;
        project(context, number, assignment.selection)?;
        *after = number;
    }
    Ok(rows.len() < limit)
}
