use super::*;
use crate::cells::repository::{
    RepositoryAuthor, collaboration::SubjectState, result_text, result_u64_from_row,
    validate_number,
};

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct After {
    pub value: u64,
    pub number: u64,
    pub kind: SubjectKind,
    pub inclusive: bool,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Input {
    pub search: SearchInput,
    pub kind: Option<SubjectKind>,
    pub after: Option<After>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Item {
    pub kind: SubjectKind,
    pub number: u64,
    pub title: String,
    pub state: SubjectState,
    pub draft: bool,
    pub created_at: u64,
    pub updated_at: u64,
    pub sort_value: u64,
    pub comments: u64,
    pub author: RepositoryAuthor,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Page {
    pub items: Vec<Item>,
    pub filter_snapshot: Option<[u8; 32]>,
}

impl Item {
    pub(crate) fn key(&self) -> (u64, u64, bool) {
        (self.sort_value, self.number, self.kind == SubjectKind::Pull)
    }
}

pub(crate) struct ListWorkspaceWork;

impl Read for ListWorkspaceWork {
    const CLASS: &'static str = crate::cells::repository::CLASS;
    const ID: u32 = 455;
    type Input = Input;
    type Output = Page;

    fn execute(context: &mut Transaction<'_>, input: Input) -> crate::cells::Result<Self::Output> {
        input
            .search
            .validate()
            .map_err(crate::cells::Error::Invalid)?;
        if !input.search.sort.is_workspace_sort() || input.search.before.is_some() {
            return Err(crate::cells::Error::Invalid("Invalid workspace page"));
        }
        let direction = input.search.direction;
        let order = direction.sql();
        let generation = ensure_ready(context)?;
        let criteria = criteria(context, &input.search, input.kind)?;
        let mut items = Vec::new();
        // Each kind uses its existing sort index before joining the canonical title.
        for kind in [SubjectKind::Issue, SubjectKind::Pull] {
            if input.kind.is_some_and(|selected| selected != kind) {
                continue;
            }
            let mut bindings =
                Bindings::new(&input.search, &criteria, context.now_ms(), Some(kind))?;
            let by_kind = kind_filter(Some(kind), &mut bindings);
            let predicate = expression(&criteria.query.expression, &mut bindings)?;
            let column =
                sort_expression(input.search.sort, &criteria.query.expression, &mut bindings);
            let boundary = if let Some(after) = input.after {
                validate_number(after.number)?;
                let value = bindings.bind(integer(after.value)?);
                let number = bindings.bind(integer(after.number)?);
                // The kind breaks equal sort-value and number ties across the
                // independent issue and pull request number sequences.
                let inclusive = if kind == after.kind {
                    after.inclusive
                } else {
                    direction
                        .compare(kind == SubjectKind::Pull, after.kind == SubjectKind::Pull)
                        .is_gt()
                };
                let operator = direction.seek(inclusive);
                format!("({column}, d.number) {operator} ({value}, {number})")
            } else {
                "1".into()
            };
            let limit = bindings.bind(SqlValue::Integer(i64::from(input.search.limit) + 1));
            let table = match kind {
                SubjectKind::Issue => "repository_issues",
                SubjectKind::Pull => "repository_pulls",
            };
            let rows = context.query(
                &format!(
                    "{} SELECT work.number, item.title, work.state, work.draft,
                        work.created_at_ms, work.updated_at_ms, work.sort_value,
                        work.comments, item.author_issuer, item.author_subject, item.author_name
                     FROM (SELECT d.number, d.state, d.draft, d.created_at_ms, d.updated_at_ms,
                           d.comments, {column} AS sort_value
                           FROM discussion_index d {} WHERE {by_kind} AND ({predicate}) AND {boundary}
                           ORDER BY {column} {order}, d.number {order} LIMIT {limit}) work
                     JOIN {table} item ON item.number = work.number",
                    bindings.head_cte(), bindings.head_joins()
                ),
                &bindings.values,
            )?;
            for row in &rows.rows {
                let state = match result_u64_from_row(row, 2)? {
                    0 => SubjectState::Open,
                    1 => SubjectState::Closed,
                    2 => SubjectState::Merged,
                    _ => return Err(crate::cells::Error::Invalid("Invalid discussion state")),
                };
                items.push(Item {
                    kind,
                    number: result_u64_from_row(row, 0)?,
                    title: result_text(row, 1)?,
                    state,
                    draft: result_u64_from_row(row, 3)? != 0,
                    created_at: result_u64_from_row(row, 4)?,
                    updated_at: result_u64_from_row(row, 5)?,
                    sort_value: result_u64_from_row(row, 6)?,
                    comments: result_u64_from_row(row, 7)?,
                    author: RepositoryAuthor {
                        issuer: result_text(row, 8)?,
                        subject: result_text(row, 9)?,
                        name: result_text(row, 10)?,
                    },
                });
            }
        }
        items.sort_unstable_by(|left, right| direction.compare(left.key(), right.key()));
        items.truncate(usize::from(input.search.limit) + 1);
        let filter_snapshot = if criteria.query.needs_pull_heads()
            || criteria.query.uses_review_teams()
            || criteria.query.has_dates()
            || criteria.query.uses_merge_queue()
            || matches!(input.search.sort, Sort::Comments | Sort::Reactions)
        {
            // Workspace continuations merge repository pages. A team roster
            // or a change to dates, counts, or queue membership can move the boundary.
            let data = serde_json::to_vec(&(
                generation,
                revision(context)?,
                &input.search.pull_heads,
                &criteria.review,
                criteria.check_revision,
                &input.search.review_teams,
            ))?;
            Some(*blake3::hash(&data).as_bytes())
        } else {
            None
        };
        Ok(Page {
            items,
            filter_snapshot,
        })
    }
}
