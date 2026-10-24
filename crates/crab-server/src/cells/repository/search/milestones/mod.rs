use serde::{Deserialize, Serialize};

use super::{
    Expression, Metadata, Query, Term, index,
    navigation::{self, Choice},
};
use crate::cells::repository::{
    collaboration::milestones::{Milestone, MilestoneState},
    integer, result_text, result_u64_from_row,
};
use crate::cells::{
    Error, Result,
    application::{Read, SqlValue, Transaction},
};

pub(in crate::cells::repository) fn project(
    context: &Transaction<'_>,
    milestone: &Milestone,
) -> Result<()> {
    context.query("INSERT INTO discussion_index_milestone_catalog(number, title, title_key, state) VALUES (?, ?, ?, ?) ON CONFLICT(number) DO UPDATE SET title = excluded.title, title_key = excluded.title_key, state = excluded.state", &[
        integer(milestone.number)?, SqlValue::Text(milestone.title.clone()), SqlValue::Text(milestone.title.to_lowercase()), SqlValue::Text(state_key(milestone.state).into()),
    ])?;
    Ok(())
}
fn state_key(state: MilestoneState) -> &'static str {
    match state {
        MilestoneState::Open => "open",
        MilestoneState::Closed => "closed",
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct OptionInput {
    pub query: Option<String>,
    #[serde(default)]
    pub search: String,
    pub state: Option<MilestoneState>,
}
impl OptionInput {
    pub(crate) fn validate(&self) -> std::result::Result<(), &'static str> {
        Query::parse(self.query.as_deref())?;
        if self.search.chars().count() > 256 || self.search.chars().any(char::is_control) {
            return Err("Milestone search must contain at most 256 characters without controls");
        }
        Ok(())
    }
}
#[derive(Debug, Deserialize, Serialize)]
pub(super) struct Summary {
    pub number: u64,
    pub title: String,
    pub state: MilestoneState,
}
#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct MilestoneOption {
    #[serde(flatten)]
    milestone: Summary,
    #[serde(flatten)]
    pub choice: Choice,
}
#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct Options {
    pub items: Vec<MilestoneOption>,
    pub clear: Choice,
    pub missing: Choice,
    pub truncated: bool,
}

pub(crate) struct MilestoneOptions;
impl Read for MilestoneOptions {
    const CLASS: &'static str = crate::cells::repository::CLASS;
    const ID: u32 = 261;
    type Input = OptionInput;
    type Output = Options;
    fn execute(context: &mut Transaction<'_>, input: Self::Input) -> Result<Self::Output> {
        input.validate().map_err(Error::Invalid)?;
        index::ensure_ready(context)?;
        let query = Query::parse(input.query.as_deref()).map_err(Error::Invalid)?;
        let mut items = matching(context, &input.search, input.state)?;
        let truncated = items.len() > 20;
        items.truncate(20);
        let items = items
            .into_iter()
            .map(|milestone| {
                Ok(MilestoneOption {
                    choice: choice(&query, Filter::Title(&milestone.title))?,
                    milestone,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(Options {
            items,
            clear: choice(&query, Filter::Clear)?,
            missing: choice(&query, Filter::Missing)?,
            truncated,
        })
    }
}

pub(super) fn matching(
    context: &Transaction<'_>,
    search: &str,
    state: Option<MilestoneState>,
) -> Result<Vec<Summary>> {
    index::ensure_ready(context)?;
    let search = SqlValue::Text(search.to_lowercase());
    let state = state.map_or(SqlValue::Null, |state| {
        SqlValue::Text(state_key(state).into())
    });
    let rows = context.query("SELECT number, title, state FROM discussion_index_milestone_catalog WHERE instr(title_key, ?1) > 0 AND (?2 IS NULL OR state = ?2) ORDER BY title_key = ?1 DESC, title_key, number LIMIT 21", &[search, state])?;
    rows.rows
        .iter()
        .map(|row| {
            Ok(Summary {
                number: result_u64_from_row(row, 0)?,
                title: result_text(row, 1)?,
                state: match result_text(row, 2)?.as_str() {
                    "open" => MilestoneState::Open,
                    "closed" => MilestoneState::Closed,
                    _ => return Err(Error::Invalid("milestone index state is invalid")),
                },
            })
        })
        .collect()
}

enum Filter<'a> {
    Clear,
    Missing,
    Title(&'a str),
}
fn milestone_atom(expression: &Expression) -> bool {
    match expression {
        Expression::Term {
            value: Term::Milestone(_) | Term::Missing(Metadata::Milestone),
            ..
        } => true,
        Expression::Not(inner) => milestone_atom(inner),
        _ => false,
    }
}
fn choice(query: &Query, filter: Filter<'_>) -> Result<Choice> {
    // Replace only global milestone conjunctions. Predicates inside OR retain
    // their grouping; treating those as a selected facet would change meaning.
    let (base, removed) = navigation::split_globals(query, milestone_atom);
    let selected = match &filter {
        Filter::Clear => removed.is_empty(),
        Filter::Missing => matches!(
            removed.as_slice(),
            [Expression::Term {
                value: Term::Missing(Metadata::Milestone),
                ..
            }]
        ),
        Filter::Title(title) => {
            matches!(removed.as_slice(), [Expression::Term { value: Term::Milestone(value), .. }] if *value == title.to_lowercase())
        }
    };
    let added = match filter {
        Filter::Clear => None,
        Filter::Missing => Some((Term::Missing(Metadata::Milestone), "no:milestone".into())),
        Filter::Title(title) => Some((
            Term::Milestone(title.to_lowercase()),
            format!("milestone:{}", serde_json::to_string(title)?),
        )),
    };
    let expression = match added {
        None => base,
        Some((value, source)) => {
            let atom = Expression::Term { value, source };
            if matches!(base, Expression::All) {
                atom
            } else {
                Expression::And(vec![base, atom])
            }
        }
    };
    Ok(Choice {
        query: navigation::destination(&expression),
        selected,
    })
}

#[cfg(test)]
mod tests;
