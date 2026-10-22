use serde::{Deserialize, Serialize};

use super::{
    Expression, Query, Term,
    navigation::{self, Choice},
    people,
};

mod projection;
mod rebuild;
pub(in crate::cells::repository) use projection::{requested, reviewed};
pub(in crate::cells::repository::search) use rebuild::{clear, requests, reset, reviews};

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Kind {
    #[default]
    ReviewRequested,
    ReviewedBy,
    ReviewInvolves,
    UserReviewRequested,
}
impl Kind {
    pub(super) fn parse(value: &str) -> Result<Self, &'static str> {
        match value {
            "review-requested" => Ok(Self::ReviewRequested),
            "reviewed-by" => Ok(Self::ReviewedBy),
            "review-involves" => Ok(Self::ReviewInvolves),
            "user-review-requested" => Ok(Self::UserReviewRequested),
            _ => Err("Unsupported reviewer qualifier"),
        }
    }
    pub(crate) const fn qualifier(self) -> &'static str {
        match self {
            Self::ReviewRequested => "review-requested",
            Self::ReviewedBy => "reviewed-by",
            Self::ReviewInvolves => "review-involves",
            Self::UserReviewRequested => "user-review-requested",
        }
    }
    pub(super) const fn predicate(self) -> &'static str {
        match self {
            Self::ReviewRequested | Self::UserReviewRequested => "r.requested = 1",
            Self::ReviewedBy => "r.reviewed = 1",
            Self::ReviewInvolves => "r.requested = 1 OR r.reviewed = 1",
        }
    }
}

impl Query {
    pub(crate) fn reviewer_choice(
        &self,
        kind: Kind,
        handle: Option<&str>,
    ) -> Result<Choice, &'static str> {
        fn atom(expression: &Expression) -> bool {
            match expression {
                Expression::Term {
                    value: Term::Reviewer { .. },
                    ..
                } => true,
                Expression::Not(inner) => atom(inner),
                _ => false,
            }
        }
        let (base, removed) = navigation::split_globals(self, atom);
        let handle = handle.map(people::name).transpose()?;
        let selected = match &handle {
            None => removed.is_empty(),
            Some(handle) => {
                matches!(removed.as_slice(), [Expression::Term { value: Term::Reviewer { kind: selected, name }, .. }] if *selected == kind && name == handle)
            }
        };
        let expression = match handle {
            None => base,
            Some(name) => navigation::conjunction(
                base,
                Expression::Term {
                    source: format!("{}:{name}", kind.qualifier()),
                    value: Term::Reviewer { kind, name },
                },
            ),
        };
        Ok(Choice {
            query: navigation::destination(&expression),
            selected,
        })
    }
    pub(super) fn review_requested_from_me(&self) -> bool {
        fn selected(expression: &Expression) -> bool {
            match expression {
                Expression::Term {
                    value:
                        Term::Reviewer {
                            kind: Kind::ReviewRequested | Kind::UserReviewRequested,
                            name,
                        },
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
