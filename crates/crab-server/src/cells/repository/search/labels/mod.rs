use super::{
    Expression, Metadata, Query, Term, index,
    navigation::{self, Choice},
};
use crate::cells::repository::labels::label_from_row;
use crate::cells::{
    Error, Result,
    application::{Read, SqlValue, Transaction},
};
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct OptionInput {
    pub query: Option<String>,
    #[serde(default)]
    pub search: String,
}
impl OptionInput {
    pub(crate) fn validate(&self) -> std::result::Result<(), &'static str> {
        Query::parse(self.query.as_deref())?;
        if self.search.chars().count() > 256 || self.search.chars().any(char::is_control) {
            return Err("Label search must contain at most 256 characters without controls");
        }
        Ok(())
    }
}
#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct LabelOption {
    pub number: u64,
    pub title: String,
    pub color: String,
    pub description: Option<String>,
    #[serde(flatten)]
    pub choice: Choice,
    pub excluded: bool,
    pub exclude_query: Option<String>,
}
#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct Options {
    pub items: Vec<LabelOption>,
    pub clear: Choice,
    pub missing: Choice,
    pub truncated: bool,
}

pub(crate) struct LabelOptions;
impl Read for LabelOptions {
    const CLASS: &'static str = crate::cells::repository::CLASS;
    const ID: u32 = 262;
    type Input = OptionInput;
    type Output = Options;
    fn execute(context: &mut Transaction<'_>, input: Self::Input) -> Result<Self::Output> {
        input.validate().map_err(Error::Invalid)?;
        index::ensure_ready(context)?;
        let query = Query::parse(input.query.as_deref()).map_err(Error::Invalid)?;
        let rows = context.query("SELECT number, name, color, description, version, created_at_ms, updated_at_ms FROM repository_labels WHERE deleted_version IS NULL AND instr(name_key, ?1) > 0 ORDER BY name_key = ?1 DESC, name_key, number LIMIT 21", &[SqlValue::Text(input.search.to_lowercase())])?;
        let truncated = rows.rows.len() > 20;
        let items = rows
            .rows
            .iter()
            .take(20)
            .map(|row| {
                let label = label_from_row(row)?;
                let (selected, excluded) = selection(&query, &label.name);
                Ok(LabelOption {
                    choice: Choice {
                        query: change(
                            &query,
                            Filter::Label {
                                name: &label.name,
                                exclude: false,
                            },
                        )?,
                        selected,
                    },
                    exclude_query: change(
                        &query,
                        Filter::Label {
                            name: &label.name,
                            exclude: true,
                        },
                    )?,
                    excluded,
                    number: label.number,
                    title: label.name,
                    color: label.color,
                    description: label.description,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let (_, removed) = navigation::split_globals(&query, label_atom);
        Ok(Options {
            items,
            clear: Choice {
                query: change(&query, Filter::Clear)?,
                selected: removed.is_empty(),
            },
            missing: Choice {
                query: change(&query, Filter::Missing)?,
                selected: removed.iter().any(|part| {
                    matches!(
                        part,
                        Expression::Term {
                            value: Term::Missing(Metadata::Label),
                            ..
                        }
                    )
                }),
            },
            truncated,
        })
    }
}

fn label_atom(expression: &Expression) -> bool {
    match expression {
        Expression::Term {
            value: Term::Label(_) | Term::Missing(Metadata::Label),
            ..
        } => true,
        Expression::Not(inner) => label_atom(inner),
        _ => false,
    }
}
fn selection(query: &Query, name: &str) -> (bool, bool) {
    let (_, removed) = navigation::split_globals(query, label_atom);
    let mut included = false;
    let mut excluded = false;
    let name = name.to_lowercase();
    for part in removed {
        match part {
            Expression::Term {
                value: Term::Label(names),
                ..
            } => included |= names.contains(&name),
            Expression::Not(inner) => {
                if let Expression::Term {
                    value: Term::Label(names),
                    ..
                } = *inner
                {
                    excluded |= names.contains(&name);
                }
            }
            _ => (),
        }
    }
    (included, excluded)
}
enum Filter<'a> {
    Clear,
    Missing,
    Label { name: &'a str, exclude: bool },
}
fn label_term(names: Vec<String>, exclude: bool) -> Result<Expression> {
    let source = format!(
        "label:{}",
        names
            .iter()
            .map(serde_json::to_string)
            .collect::<std::result::Result<Vec<_>, _>>()?
            .join(",")
    );
    let term = Expression::Term {
        value: Term::Label(names.into_iter().map(|name| name.to_lowercase()).collect()),
        source,
    };
    Ok(if exclude {
        Expression::Not(Box::new(term))
    } else {
        term
    })
}
fn change(query: &Query, filter: Filter<'_>) -> Result<Option<String>> {
    let (mut expression, removed) = navigation::split_globals(query, label_atom);
    match filter {
        Filter::Clear => (),
        Filter::Missing => {
            if !removed.iter().any(|part| {
                matches!(
                    part,
                    Expression::Term {
                        value: Term::Missing(Metadata::Label),
                        ..
                    }
                )
            }) {
                expression = navigation::conjunction(
                    expression,
                    Expression::Term {
                        value: Term::Missing(Metadata::Label),
                        source: "no:label".into(),
                    },
                );
            }
        }
        Filter::Label { name, exclude } => {
            let (included, excluded) = selection(query, name);
            let name_key = name.to_lowercase();
            // Editing one name retains other members of a comma-OR clause and
            // all unrelated clauses. Missing-label predicates are replaced.
            for part in removed {
                let (atom, negative) = match &part {
                    Expression::Not(inner) => (inner.as_ref(), true),
                    _ => (&part, false),
                };
                let Expression::Term {
                    value: Term::Label(names),
                    ..
                } = atom
                else {
                    continue;
                };
                let remaining: Vec<_> = names
                    .iter()
                    .filter(|value| **value != name_key)
                    .cloned()
                    .collect();
                let retained = if remaining.len() == names.len() {
                    Some(part)
                } else if remaining.is_empty() {
                    None
                } else {
                    Some(label_term(remaining, negative)?)
                };
                if let Some(retained) = retained {
                    expression = navigation::conjunction(expression, retained);
                }
            }
            if !(if exclude { excluded } else { included }) {
                expression =
                    navigation::conjunction(expression, label_term(vec![name.into()], exclude)?);
            }
        }
    }
    Ok(navigation::destination(&expression))
}

#[cfg(test)]
mod tests;
