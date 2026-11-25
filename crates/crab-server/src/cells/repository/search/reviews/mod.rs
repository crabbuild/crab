use serde::{Deserialize, Serialize};

use super::{Expression, Query, Term};

mod projection;
mod rebuild;
pub(crate) mod status;
pub(crate) mod teams;
pub(in crate::cells::repository) use projection::{requested, reviewed};
pub(in crate::cells::repository::search) use rebuild::{clear, requests, reset, reviews};

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Kind {
    ReviewRequested,
    ReviewedBy,
    ReviewInvolves,
    UserReviewRequested,
    TeamReviewRequestedUser,
}
impl Kind {
    pub(super) fn parse(value: &str) -> Result<Self, &'static str> {
        match value {
            "review-requested" => Ok(Self::ReviewRequested),
            "reviewed-by" => Ok(Self::ReviewedBy),
            "review-involves" => Ok(Self::ReviewInvolves),
            "user-review-requested" => Ok(Self::UserReviewRequested),
            "team-review-requested-user" => Ok(Self::TeamReviewRequestedUser),
            _ => Err("Unsupported reviewer qualifier"),
        }
    }
    pub(crate) const fn qualifier(self) -> &'static str {
        match self {
            Self::ReviewRequested => "review-requested",
            Self::ReviewedBy => "reviewed-by",
            Self::ReviewInvolves => "review-involves",
            Self::UserReviewRequested => "user-review-requested",
            Self::TeamReviewRequestedUser => "team-review-requested-user",
        }
    }
    pub(super) const fn direct_predicate(self) -> Option<&'static str> {
        match self {
            Self::ReviewRequested | Self::UserReviewRequested => Some("r.requested = 1"),
            Self::ReviewedBy => Some("r.reviewed = 1"),
            Self::ReviewInvolves => Some("r.requested = 1 OR r.reviewed = 1"),
            Self::TeamReviewRequestedUser => None,
        }
    }

    pub(super) const fn includes_teams(self) -> bool {
        matches!(self, Self::ReviewRequested | Self::TeamReviewRequestedUser)
    }
}

impl Query {
    pub(super) fn review_requested_from_me(&self) -> bool {
        fn selected(expression: &Expression) -> bool {
            match expression {
                Expression::Term {
                    value:
                        Term::Reviewer {
                            kind:
                                Kind::ReviewRequested
                                | Kind::UserReviewRequested
                                | Kind::TeamReviewRequestedUser,
                            name,
                        },
                    ..
                } => name == "@me",
                Expression::Scope { expression, .. } => selected(expression),
                Expression::And(parts) => parts.iter().any(selected),
                _ => false,
            }
        }
        selected(&self.expression)
    }
}

#[cfg(test)]
mod tests;
