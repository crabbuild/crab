use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{
    Expression, Metadata, Query, Term,
    navigation::{self, Choice},
};
use crate::cells::{Error, Result, organization::issue_types::Definition};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Binding {
    pub organization: Uuid,
    pub ownership_epoch: u32,
    pub version: u64,
    pub names: BTreeMap<String, Uuid>,
}

pub(super) fn is_type(term: &Term) -> bool {
    matches!(
        term,
        Term::IssueType(_) | Term::Missing(Metadata::IssueType)
    )
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct OptionInput {
    pub query: Option<String>,
    #[serde(default)]
    pub search: String,
}

#[derive(Serialize)]
pub(crate) struct OptionItem {
    id: Uuid,
    title: String,
    description: String,
    #[serde(flatten)]
    choice: Choice,
}

#[derive(Serialize)]
pub(crate) struct Options {
    items: Vec<OptionItem>,
    clear: Choice,
    missing: Choice,
    truncated: bool,
}

pub(crate) fn options(input: OptionInput, definitions: Vec<Definition>) -> Result<Options> {
    let query = Query::parse(input.query.as_deref()).map_err(Error::Invalid)?;
    if input.search.chars().count() > 256 || input.search.chars().any(char::is_control) {
        return Err(Error::Invalid(
            "Issue type search must be at most 256 characters without controls",
        ));
    }
    let (base, removed) = navigation::split_globals(&query, |expression| match expression {
        Expression::Term { value, .. } => is_type(value),
        Expression::Not(inner) => {
            matches!(inner.as_ref(), Expression::Term { value, .. } if is_type(value))
        }
        _ => false,
    });
    let choice = |filter: Option<(&str, Term, String)>| {
        let (selected, next) = match filter {
            Some((name, value, source)) => {
                let selected = match (removed.as_slice(), &value) {
                    (
                        [
                            Expression::Term {
                                value: Term::IssueType(key),
                                ..
                            },
                        ],
                        Term::IssueType(_),
                    ) => key == name,
                    (
                        [
                            Expression::Term {
                                value: Term::Missing(Metadata::IssueType),
                                ..
                            },
                        ],
                        Term::Missing(Metadata::IssueType),
                    ) => true,
                    _ => false,
                };
                (
                    selected,
                    navigation::conjunction(base.clone(), Expression::Term { value, source }),
                )
            }
            None => (removed.is_empty(), base.clone()),
        };
        Choice {
            query: navigation::destination(&next),
            selected,
        }
    };
    let search = input.search.to_lowercase();
    let items = definitions
        .into_iter()
        .filter_map(|definition| {
            let name = definition.name.to_lowercase();
            name.contains(&search).then_some((definition, name))
        })
        .map(|(definition, name)| {
            let source = format!("type:{}", serde_json::to_string(&definition.name)?);
            Ok(OptionItem {
                choice: choice(Some((&name, Term::IssueType(name.clone()), source))),
                id: definition.id,
                title: definition.name,
                description: definition.description,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(Options {
        items,
        clear: choice(None),
        missing: choice(Some((
            "",
            Term::Missing(Metadata::IssueType),
            "no:type".into(),
        ))),
        truncated: false,
    })
}
