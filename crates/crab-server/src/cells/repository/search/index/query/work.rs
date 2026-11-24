use super::*;
use crate::cells::repository::{
    collaboration::SubjectState, result_text, result_u64_from_row, validate_number,
};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Relation {
    #[default]
    Authored,
    Assigned,
    ReviewRequested,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum StateFilter {
    #[default]
    Open,
    Closed,
    All,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Filter {
    pub relation: Relation,
    pub kind: SubjectKind,
    pub state: StateFilter,
}

impl Filter {
    pub(crate) fn valid(self) -> bool {
        self.relation != Relation::ReviewRequested
            || (self.kind == SubjectKind::Pull && self.state == StateFilter::Open)
    }

    fn query(self) -> String {
        let relation = match self.relation {
            Relation::Authored => "author:@me",
            Relation::Assigned => "assignee:@me",
            Relation::ReviewRequested => "review-requested:@me",
        };
        match self.state {
            StateFilter::Open => format!("is:open {relation}"),
            StateFilter::Closed => format!("is:closed {relation}"),
            StateFilter::All => relation.into(),
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct After {
    pub created_at: u64,
    pub number: u64,
    pub inclusive: bool,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Input {
    pub filter: Filter,
    pub identity: [u8; 32],
    pub account: Option<Uuid>,
    pub teams: Vec<Uuid>,
    pub after: Option<After>,
    pub limit: u8,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Item {
    pub number: u64,
    pub title: String,
    pub state: SubjectState,
    pub draft: bool,
    pub created_at: u64,
}

pub(crate) struct ListPersonalWork;
impl Read for ListPersonalWork {
    const CLASS: &'static str = crate::cells::repository::CLASS;
    const ID: u32 = 455;
    type Input = Input;
    type Output = Vec<Item>;

    fn execute(context: &mut Transaction<'_>, input: Input) -> crate::cells::Result<Self::Output> {
        if !input.filter.valid() || !(1..=51).contains(&input.limit) {
            return Err(crate::cells::Error::Invalid("Invalid personal work filter"));
        }
        ensure_ready(context)?;
        let search = SearchInput {
            query: Some(input.filter.query()),
            people: if input.filter.relation == Relation::Assigned {
                Vec::new()
            } else {
                vec![super::super::super::people::Binding {
                    name: "@me".into(),
                    identity: Some(input.identity),
                }]
            },
            assignees: if input.filter.relation == Relation::Assigned {
                vec![super::super::super::assignees::Binding {
                    name: "@me".into(),
                    id: input.account,
                }]
            } else {
                Vec::new()
            },
            review_teams: super::super::super::reviews::teams::Bindings {
                named: Vec::new(),
                mine: input.teams,
            },
            ..SearchInput::default()
        };
        let query = Query::parse(search.query.as_deref()).map_err(crate::cells::Error::Invalid)?;
        search
            .review_teams
            .validate(&query)
            .map_err(crate::cells::Error::Invalid)?;
        let mut bindings = Bindings::new(&search, None, false, context.now_ms())?;
        let kind = kind_filter(Some(input.filter.kind), &mut bindings);
        let predicate = expression(&query.expression, &mut bindings)?;
        let boundary = if let Some(after) = input.after {
            validate_number(after.number)?;
            let created = bindings.bind(integer(after.created_at)?);
            let number = bindings.bind(integer(after.number)?);
            let direction = if after.inclusive { "<=" } else { "<" };
            format!("(d.created_at_ms, d.number) {direction} ({created}, {number})")
        } else {
            "1".into()
        };
        let limit = bindings.bind(SqlValue::Integer(i64::from(input.limit)));
        let table = match input.filter.kind {
            SubjectKind::Issue => "repository_issues",
            SubjectKind::Pull => "repository_pulls",
        };
        // Reuse discussion predicates and the existing kind/creation index.
        // Read the canonical title after seeking because the index lowercases text.
        let rows = context.query(
            &format!(
                "SELECT work.number, item.title, work.state, work.draft, work.created_at_ms
                FROM (SELECT d.number, d.state, d.draft, d.created_at_ms FROM discussion_index d
                    WHERE {kind} AND ({predicate}) AND {boundary}
                    ORDER BY d.created_at_ms DESC, d.number DESC LIMIT {limit}) work
                JOIN {table} item ON item.number = work.number
                ORDER BY work.created_at_ms DESC, work.number DESC"
            ),
            &bindings.values,
        )?;
        rows.rows
            .iter()
            .map(|row| {
                let state = match result_u64_from_row(row, 2)? {
                    0 => SubjectState::Open,
                    1 => SubjectState::Closed,
                    2 => SubjectState::Merged,
                    _ => return Err(crate::cells::Error::Invalid("Invalid discussion state")),
                };
                Ok(Item {
                    number: result_u64_from_row(row, 0)?,
                    title: result_text(row, 1)?,
                    state,
                    draft: result_u64_from_row(row, 3)? != 0,
                    created_at: result_u64_from_row(row, 4)?,
                })
            })
            .collect()
    }
}
