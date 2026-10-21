use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use super::{
    Expression, Query, Term,
    navigation::{self, Choice},
};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Binding {
    pub name: String,
    pub identity: Option<[u8; 32]>,
}

pub(crate) fn identity(issuer: &str, subject: &str) -> crate::cells::Result<[u8; 32]> {
    // Length-delimited JSON preserves the qualified pair without concatenation
    // collisions. This derived key is versioned with the discussion index.
    Ok(*blake3::hash(&serde_json::to_vec(&(issuer, subject))?).as_bytes())
}

pub(super) fn name(value: &str) -> Result<String, &'static str> {
    let value = value.to_ascii_lowercase();
    if value == "@me" {
        return Ok(value);
    }
    crate::cells::directory::validate_handle(&value)?;
    Ok(value)
}

impl Query {
    pub(crate) fn authors(&self) -> BTreeSet<String> {
        fn collect(expression: &Expression, names: &mut BTreeSet<String>) {
            match expression {
                Expression::Term {
                    value: Term::Author(name),
                    ..
                } => {
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

    pub(super) fn validate_authors(&self, bindings: &[Binding]) -> Result<(), &'static str> {
        let names = self.authors();
        if bindings.len() != names.len()
            || !bindings
                .iter()
                .map(|binding| &binding.name)
                .eq(names.iter())
        {
            return Err("Author filters require ordered, resolved identity bindings");
        }
        if bindings
            .iter()
            .any(|binding| binding.name == "@me" && binding.identity.is_none())
        {
            return Err("Current-account author filters require an authenticated identity");
        }
        Ok(())
    }

    pub(crate) fn author_choice(&self, handle: Option<&str>) -> Result<Choice, &'static str> {
        fn author_atom(expression: &Expression) -> bool {
            match expression {
                Expression::Term {
                    value: Term::Author(_),
                    ..
                } => true,
                Expression::Not(inner) => author_atom(inner),
                _ => false,
            }
        }
        // Only global conjunctions belong to this facet. Conditional authors
        // inside OR keep their grouping when another author is selected.
        let (base, removed) = navigation::split_globals(self, author_atom);
        let handle = handle.map(name).transpose()?;
        let selected = match &handle {
            None => removed.is_empty(),
            Some(handle) => {
                matches!(removed.as_slice(), [Expression::Term { value: Term::Author(value), .. }] if value == handle)
            }
        };
        let expression = match handle {
            None => base,
            Some(handle) => navigation::conjunction(
                base,
                Expression::Term {
                    source: format!("author:{handle}"),
                    value: Term::Author(handle),
                },
            ),
        };
        Ok(Choice {
            query: navigation::destination(&expression),
            selected,
        })
    }

    pub(super) fn authored_by_me(&self) -> bool {
        fn selected(expression: &Expression) -> bool {
            match expression {
                Expression::Term {
                    value: Term::Author(name),
                    ..
                } => name == "@me",
                Expression::And(parts) => parts.iter().any(selected),
                _ => false,
            }
        }
        selected(&self.expression)
    }
}

#[cfg(test)]
mod tests;
