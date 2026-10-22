use std::collections::BTreeSet;

use crate::cells::repository::assignments::Assignee;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{
    Expression, Metadata, Query, Term, index,
    navigation::{self, Choice},
};
use crate::cells::{
    Error, Result,
    application::{Read, SqlValue, Transaction},
};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Binding {
    pub name: String,
    pub id: Option<Uuid>,
}

pub(super) fn name(value: &str) -> std::result::Result<String, &'static str> {
    let value = value.to_ascii_lowercase();
    if value == "*" {
        return Ok(value);
    }
    if let Some(id) = value.strip_prefix("actor/") {
        let id = Uuid::parse_str(id).map_err(|_| "Assignee actor ID is invalid")?;
        if id.is_nil() {
            return Err("Assignee actor ID is invalid");
        }
        return Ok(format!("actor/{id}"));
    }
    super::people::name(&value)
}

pub(crate) enum Filter<'a> {
    Clear,
    Missing,
    Person(&'a str),
}

impl Query {
    pub(crate) fn assignees(&self) -> BTreeSet<String> {
        fn collect(expression: &Expression, names: &mut BTreeSet<String>) {
            match expression {
                Expression::Term {
                    value: Term::Assignee(name),
                    ..
                } if name != "*" && !name.starts_with("actor/") => {
                    names.insert(name.clone());
                }
                Expression::Not(inner) => collect(inner, names),
                Expression::And(parts) | Expression::Or(parts) => {
                    for part in parts {
                        collect(part, names);
                    }
                }
                _ => (),
            }
        }
        let mut names = BTreeSet::new();
        collect(&self.expression, &mut names);
        names
    }
    pub(super) fn validate_assignees(
        &self,
        bindings: &[Binding],
    ) -> std::result::Result<(), &'static str> {
        let names = self.assignees();
        if bindings.len() != names.len()
            || !bindings
                .iter()
                .map(|binding| &binding.name)
                .eq(names.iter())
            || bindings
                .iter()
                .any(|binding| binding.id.is_some_and(|id| id.is_nil()))
        {
            return Err("Assignee filters require ordered, resolved actor bindings");
        }
        Ok(())
    }
    pub(crate) fn assignee_choice(
        &self,
        value: Filter<'_>,
    ) -> std::result::Result<Choice, &'static str> {
        fn atom(expression: &Expression) -> bool {
            match expression {
                Expression::Term {
                    value: Term::Assignee(_) | Term::Missing(Metadata::Assignee),
                    ..
                } => true,
                Expression::Not(inner) => atom(inner),
                _ => false,
            }
        }
        let (base, removed) = navigation::split_globals(self, atom);
        let term = match value {
            Filter::Clear => None,
            Filter::Missing => Some(Term::Missing(Metadata::Assignee)),
            Filter::Person(value) => Some(Term::Assignee(name(value)?)),
        };
        let selected = match &term {
            None => removed.is_empty(),
            Some(Term::Missing(_)) => matches!(
                removed.as_slice(),
                [Expression::Term {
                    value: Term::Missing(Metadata::Assignee),
                    ..
                }]
            ),
            Some(Term::Assignee(value)) => {
                matches!(removed.as_slice(), [Expression::Term { value: Term::Assignee(current), .. }] if current == value)
            }
            _ => false,
        };
        let expression = match term {
            None => base,
            Some(term) => {
                let source = match &term {
                    Term::Assignee(value) => format!("assignee:{value}"),
                    _ => "no:assignee".into(),
                };
                navigation::conjunction(
                    base,
                    Expression::Term {
                        source,
                        value: term,
                    },
                )
            }
        };
        Ok(Choice {
            query: navigation::destination(&expression),
            selected,
        })
    }
    pub(super) fn assigned_to_me(&self) -> bool {
        fn selected(expression: &Expression) -> bool {
            match expression {
                Expression::Term {
                    value: Term::Assignee(name),
                    ..
                } => name == "@me",
                Expression::And(parts) => parts.iter().any(selected),
                _ => false,
            }
        }
        selected(&self.expression)
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
#[derive(Default)]
pub(crate) struct OptionInput {
    pub query: Option<String>,
    pub search: String,
}
impl OptionInput {
    pub(crate) fn validate(&self) -> std::result::Result<(), &'static str> {
        Query::parse(self.query.as_deref())?;
        if self.search.chars().count() > 256 || self.search.chars().any(char::is_control) {
            return Err("Assignee search must contain at most 256 characters without controls");
        }
        Ok(())
    }
}
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Matches {
    pub items: Vec<Assignee>,
    pub truncated: bool,
}
pub(crate) struct AssigneeOptions;
impl Read for AssigneeOptions {
    const CLASS: &'static str = crate::cells::repository::CLASS;
    const ID: u32 = 263;
    type Input = OptionInput;
    type Output = Matches;
    fn execute(context: &mut Transaction<'_>, input: Self::Input) -> Result<Self::Output> {
        input.validate().map_err(Error::Invalid)?;
        index::ensure_ready(context)?;
        // The projection keeps snapshots searchable after a grant is removed.
        // Public profiles may provide newer presentation in the HTTP owner.
        let rows = context.query("SELECT actor, MIN(name) FROM discussion_index_assignees WHERE instr(name_key, ?1) > 0 OR actor = ?1 GROUP BY actor ORDER BY actor LIMIT 21", &[SqlValue::Text(input.search.trim().to_lowercase())])?;
        let truncated = rows.rows.len() > 20;
        let items = rows
            .rows
            .iter()
            .take(20)
            .map(|row| {
                let id = Uuid::parse_str(&crate::cells::repository::result_text(row, 0)?)
                    .map_err(Error::sdk)?;
                if id.is_nil() {
                    return Err(Error::Invalid("Assignment index actor is invalid"));
                }
                Ok(Assignee {
                    id,
                    name: crate::cells::repository::result_text(row, 1)?,
                })
            })
            .collect::<Result<_>>()?;
        Ok(Matches { items, truncated })
    }
}

#[cfg(test)]
mod tests;
