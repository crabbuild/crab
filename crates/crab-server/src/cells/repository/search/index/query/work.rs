use std::cmp::Reverse;

use super::*;
use crate::cells::repository::{
    collaboration::SubjectState, result_text, result_u64_from_row, validate_number,
};

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct After {
    pub time: u64,
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
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Page {
    pub items: Vec<Item>,
    pub filter_snapshot: Option<[u8; 32]>,
}

impl Item {
    pub(crate) fn key(&self, sort: Sort) -> (u64, u64, bool) {
        let time = if sort == Sort::Updated {
            self.updated_at
        } else {
            self.created_at
        };
        (time, self.number, self.kind == SubjectKind::Pull)
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
        let column = match input.search.sort {
            Sort::Created => "d.created_at_ms",
            Sort::Updated => "d.updated_at_ms",
            _ => return Err(crate::cells::Error::Invalid("Invalid workspace sort")),
        };
        if input.search.before.is_some() || input.search.direction != Direction::Desc {
            return Err(crate::cells::Error::Invalid("Invalid workspace page"));
        }
        let generation = ensure_ready(context)?;
        let criteria = criteria(context, &input.search, input.kind)?;
        let mut items = Vec::new();
        // Each kind uses its existing time index before joining the canonical title.
        for kind in [SubjectKind::Issue, SubjectKind::Pull] {
            if input.kind.is_some_and(|selected| selected != kind) {
                continue;
            }
            let mut bindings = Bindings::new(&input.search, &criteria, context.now_ms())?;
            let by_kind = kind_filter(Some(kind), &mut bindings);
            let predicate = expression(&criteria.query.expression, &mut bindings)?;
            let boundary = if let Some(after) = input.after {
                validate_number(after.number)?;
                let time = bindings.bind(integer(after.time)?);
                let number = bindings.bind(integer(after.number)?);
                let inclusive = if kind == after.kind {
                    after.inclusive
                } else {
                    kind == SubjectKind::Issue
                };
                let operator = if inclusive { "<=" } else { "<" };
                format!("({column}, d.number) {operator} ({time}, {number})")
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
                        work.created_at_ms, work.updated_at_ms
                     FROM (SELECT d.number, d.state, d.draft, d.created_at_ms, d.updated_at_ms
                           FROM discussion_index d {} WHERE {by_kind} AND ({predicate}) AND {boundary}
                           ORDER BY {column} DESC, d.number DESC LIMIT {limit}) work
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
                });
            }
        }
        items.sort_unstable_by_key(|item| Reverse(item.key(input.search.sort)));
        items.truncate(usize::from(input.search.limit) + 1);
        let filter_snapshot =
            if criteria.query.needs_pull_heads() || criteria.query.uses_review_teams() {
                // Workspace continuations merge repository pages. A team roster
                // or a fulfilled request can change membership without moving a Git ref.
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
