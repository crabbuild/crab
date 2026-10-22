use super::people::name;

use super::{
    Expression, Query, Term,
    navigation::{self, Choice},
};

impl Query {
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
