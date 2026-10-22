use serde::{Deserialize, Serialize};

use super::{Expression, Query, SubjectState, Term};

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct Choice {
    pub query: Option<String>,
    pub selected: bool,
}

// A facet edits global conjunctions only. An OR subtree is a conditional
// predicate and must keep its grouping when another filter is selected.
pub(super) fn split_globals(
    query: &Query,
    matches: fn(&Expression) -> bool,
) -> (Expression, Vec<Expression>) {
    let mut removed = Vec::new();
    let base = without_globals(&query.expression, matches, &mut removed).unwrap_or(Expression::All);
    (base, removed)
}
fn without_globals(
    expression: &Expression,
    matches: fn(&Expression) -> bool,
    removed: &mut Vec<Expression>,
) -> Option<Expression> {
    if matches(expression) {
        removed.push(expression.clone());
        return None;
    }
    if let Expression::Scope { fields, expression } = expression {
        let expression = without_globals(expression, matches, removed).unwrap_or(Expression::All);
        return Some(Expression::Scope {
            fields: *fields,
            expression: Box::new(expression),
        });
    }
    if let Expression::And(parts) = expression {
        let mut parts: Vec<_> = parts
            .iter()
            .filter_map(|part| without_globals(part, matches, removed))
            .collect();
        return match parts.len() {
            0 => None,
            1 => parts.pop(),
            _ => Some(Expression::And(parts)),
        };
    }
    Some(expression.clone())
}
pub(super) fn conjunction(base: Expression, atom: Expression) -> Expression {
    if matches!(base, Expression::All) {
        atom
    } else {
        Expression::And(vec![base, atom])
    }
}
pub(super) fn destination(expression: &Expression) -> Option<String> {
    let value = source(expression, 0);
    // Empty browser queries select default lists. Spell the universal predicate
    // explicitly so clearing a final facet preserves the unrestricted search.
    let value = if value.is_empty() {
        "is:issue OR is:pr".into()
    } else {
        value
    };
    Query::parse(Some(&value)).ok().map(|_| value)
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum StateSelection {
    Open,
    Closed,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum StateNavigation {
    Results,
    State {
        selected: StateSelection,
        open: Option<String>,
        closed: Option<String>,
    },
}

impl Query {
    pub(super) fn state_navigation(&self) -> (Expression, StateNavigation) {
        let Some(selected) = global_state(&self.expression).ok().flatten() else {
            return (self.expression.clone(), StateNavigation::Results);
        };
        let (base, _) = split_globals(self, |expression| {
            matches!(
                expression,
                Expression::Term {
                    value: Term::State(_),
                    ..
                }
            )
        });
        let with_state = |state: SubjectState| {
            let spelling = match state {
                SubjectState::Open => "is:open",
                SubjectState::Closed => "is:closed",
                SubjectState::Merged => "is:merged",
            }
            .to_owned();
            let state = Expression::Term {
                value: Term::State(state),
                source: spelling,
            };
            let expression = if matches!(base, Expression::All) {
                state
            } else {
                Expression::And(vec![base.clone(), state])
            };
            let query = source(&expression, 0);
            // A valid maximum-sized query may have no room for another term.
            // Do not truncate user filters or emit an unusable navigation URL.
            Query::parse(Some(&query)).ok().map(|_| query)
        };
        let navigation = StateNavigation::State {
            selected,
            open: with_state(SubjectState::Open),
            closed: with_state(SubjectState::Closed),
        };
        (base, navigation)
    }
}

// Only a global positive Open/Closed conjunction is a state facet. A state
// inside OR, an exclusion, or a merge-state query stays an arbitrary search;
// manufacturing tabs for it would change the user's boolean predicate.
fn global_state(expression: &Expression) -> Result<Option<StateSelection>, ()> {
    match expression {
        Expression::Scope { expression, .. } => global_state(expression),
        Expression::Term {
            value: Term::State(SubjectState::Open),
            ..
        } => Ok(Some(StateSelection::Open)),
        Expression::Term {
            value: Term::State(SubjectState::Closed),
            ..
        } => Ok(Some(StateSelection::Closed)),
        Expression::And(parts) => parts.iter().try_fold(None, |current, part| {
            let next = global_state(part)?;
            match (current, next) {
                (Some(current), Some(next)) if current != next => Err(()),
                (Some(current), _) => Ok(Some(current)),
                (_, next) => Ok(next),
            }
        }),
        _ if has_state(expression) => Err(()),
        _ => Ok(None),
    }
}
fn has_state(expression: &Expression) -> bool {
    match expression {
        Expression::Scope { expression, .. } => has_state(expression),
        Expression::Term {
            value: Term::State(_) | Term::Unmerged,
            ..
        } => true,
        Expression::Not(inner) => has_state(inner),
        Expression::And(parts) | Expression::Or(parts) => parts.iter().any(has_state),
        _ => false,
    }
}

pub(super) fn source(expression: &Expression, parent: u8) -> String {
    match expression {
        Expression::All => String::new(),
        Expression::Term { source, .. } => source.clone(),
        Expression::Scope { fields, expression } => {
            let inner = source(expression, 0);
            let fields = fields.source();
            if inner.is_empty() {
                format!("(in:{fields})")
            } else {
                format!("({inner} in:{fields})")
            }
        }
        Expression::Not(inner) => format!("-{}", source(inner, 3)),
        Expression::And(parts) | Expression::Or(parts) => {
            let (precedence, operator) = if matches!(expression, Expression::And(_)) {
                (2, " ")
            } else {
                (1, " OR ")
            };
            let joined = parts
                .iter()
                .map(|part| source(part, precedence))
                .collect::<Vec<_>>()
                .join(operator);
            if precedence < parent {
                format!("({joined})")
            } else {
                joined
            }
        }
    }
}
#[cfg(test)]
mod tests;
