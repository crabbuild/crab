use serde::{Deserialize, Serialize};

pub(in crate::cells::repository::search) const UNREVIEWED: &str = "d.kind=1 AND NOT EXISTS (SELECT 1 FROM repository_pull_review_decisions v WHERE v.pull_number=d.number)";

pub(in crate::cells::repository::search) const CHANGES_REQUESTED: &str = "d.kind=1 AND EXISTS (SELECT 1 FROM repository_pull_review_decisions v WHERE v.pull_number=d.number AND v.state=2)";

use super::super::{Expression, Query, Term, navigation};
use crate::cells::{
    Error, Result,
    application::{Read, SqlValue, Transaction},
    repository::{BranchProtectionRecord, GetBranchProtections, projection},
};

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Status {
    None,
    Required,
    Approved,
    ChangesRequested,
}
impl Status {
    pub(in crate::cells::repository::search) fn parse(
        value: &str,
    ) -> std::result::Result<Self, &'static str> {
        match value.to_ascii_lowercase().as_str() {
            "none" => Ok(Self::None),
            "required" => Ok(Self::Required),
            "approved" => Ok(Self::Approved),
            "changes_requested" => Ok(Self::ChangesRequested),
            _ => Err("Review status must be none, required, approved, or changes_requested"),
        }
    }
    fn name(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Required => "required",
            Self::Approved => "approved",
            Self::ChangesRequested => "changes_requested",
        }
    }
}

impl Query {
    fn review_statuses(&self) -> Vec<Status> {
        fn collect(expression: &Expression, values: &mut Vec<Status>) {
            match expression {
                Expression::Term {
                    value: Term::ReviewStatus(status),
                    ..
                } => values.push(*status),
                Expression::Not(inner)
                | Expression::Scope {
                    expression: inner, ..
                } => collect(inner, values),
                Expression::And(parts) | Expression::Or(parts) => {
                    for part in parts {
                        collect(part, values);
                    }
                }
                _ => (),
            }
        }
        let mut values = vec![];
        collect(&self.expression, &mut values);
        values
    }
    pub(crate) fn needs_review_source(&self) -> bool {
        self.review_statuses()
            .iter()
            .any(|status| matches!(status, Status::Required | Status::Approved))
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Context {
    pub source_token: Option<String>,
    pub configured_rules: Vec<BranchProtectionRecord>,
}

pub(crate) struct GetSourceState;
impl Read for GetSourceState {
    const CLASS: &'static str = crate::cells::repository::CLASS;
    const ID: u32 = 327;
    type Input = ();
    type Output = Option<projection::ProjectionStateView>;
    fn execute(context: &mut Transaction<'_>, (): ()) -> Result<Self::Output> {
        super::super::index::ensure_ready(context)?;
        if context
            .query("SELECT 1 FROM repository_pulls LIMIT 1", &[])?
            .rows
            .is_empty()
        {
            return Ok(None);
        }
        projection::GetProjectionState::execute(context, projection::ProjectionState).map(Some)
    }
}

pub(in crate::cells::repository::search) struct Prepared {
    pub epoch: Option<u64>,
    pub rules: Vec<BranchProtectionRecord>,
}
pub(in crate::cells::repository::search) fn prepare(
    context: &mut Transaction<'_>,
    input: Option<&Context>,
) -> Result<Prepared> {
    let input = input.ok_or(Error::Invalid(
        "Review status requires a captured Git source",
    ))?;
    crate::cells::repository::settings::validate_protections(&input.configured_rules)?;
    if input.source_token.as_ref().is_some_and(|token| {
        token.len() != 64
            || !token
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    }) {
        return Err(Error::Invalid("Review source token is invalid"));
    }
    let epoch = match (GetSourceState::execute(context, ())?, &input.source_token) {
        (None, None) => None,
        (Some(state), Some(token))
            if state.source_token.as_ref() == Some(token) && state.ready_epoch.is_some() =>
        {
            state.ready_epoch
        }
        _ => return Err(Error::StaleSearch),
    };
    // Saved rules, including an empty set, override configured initial policy.
    let rules = GetBranchProtections::execute(context, ())?
        .map_or_else(|| input.configured_rules.clone(), |value| value.rules);
    Ok(Prepared { epoch, rules })
}

pub(in crate::cells::repository::search) struct Bound {
    epoch: String,
    approvals: String,
}
impl Bound {
    pub(in crate::cells::repository::search) fn new(
        prepared: &Prepared,
        mut bind: impl FnMut(SqlValue) -> String,
    ) -> Result<Self> {
        let epoch = bind(
            prepared
                .epoch
                .map(crate::cells::repository::integer)
                .transpose()?
                .unwrap_or(SqlValue::Null),
        );
        let mut approvals = String::from("CASE s.base_ref");
        for rule in &prepared.rules {
            let branch = bind(SqlValue::Text(format!("refs/heads/{}", rule.branch)));
            let count = bind(SqlValue::Integer(i64::from(rule.required_approvals)));
            approvals.push_str(&format!(" WHEN {branch} THEN {count}"));
        }
        approvals.push_str(" ELSE 0 END");
        if prepared.rules.is_empty() {
            approvals = String::from("0");
        }
        Ok(Self { epoch, approvals })
    }
    pub(in crate::cells::repository::search) fn predicate(&self, status: Status) -> String {
        let head = "COALESCE(s.merged_head_oid,NULLIF(lower(hex(r.target_oid)),''))";
        let approved = format!(
            "(SELECT COUNT(*) FROM repository_pull_review_decisions v WHERE v.pull_number=d.number AND v.state=1 AND v.commit_oid={head})"
        );
        // A push invalidates approvals, but cannot resolve another reviewer’s
        // requested changes. Only a replacement decision or dismissal can do so.
        let changes = CHANGES_REQUESTED;
        let condition = match status {
            Status::None => return UNREVIEWED.into(),
            Status::Required => format!("NOT ({changes}) AND {approved}<({})", self.approvals),
            Status::Approved => format!(
                "NOT ({changes}) AND {approved}>0 AND {approved}>=({})",
                self.approvals
            ),
            Status::ChangesRequested => return CHANGES_REQUESTED.into(),
        };
        format!(
            "d.kind=1 AND EXISTS (SELECT 1 FROM discussion_index_pull_sources s LEFT JOIN git_projection_refs r ON r.epoch_id={} AND r.name=s.head_ref WHERE s.document=d.rowid AND ({condition}))",
            self.epoch
        )
    }
}

#[derive(Serialize)]
pub(crate) struct Menu {
    pub clear: navigation::Choice,
    pub items: Vec<Item>,
}
#[derive(Serialize)]
pub(crate) struct Item {
    pub label: &'static str,
    #[serde(flatten)]
    pub choice: navigation::Choice,
}
impl Query {
    pub(crate) fn review_menu(&self) -> Menu {
        fn review(expression: &Expression) -> bool {
            match expression {
                Expression::Term {
                    value: Term::Reviewer { .. } | Term::ReviewStatus(_),
                    ..
                } => true,
                Expression::Not(inner) => review(inner),
                _ => false,
            }
        }
        fn equal(left: &Expression, right: &Expression) -> bool {
            match (left, right) {
                (
                    Expression::Term {
                        value: Term::ReviewStatus(a),
                        ..
                    },
                    Expression::Term {
                        value: Term::ReviewStatus(b),
                        ..
                    },
                ) => a == b,
                (
                    Expression::Term {
                        value: Term::Reviewer { kind: a, name: x },
                        ..
                    },
                    Expression::Term {
                        value: Term::Reviewer { kind: b, name: y },
                        ..
                    },
                ) => a == b && x == y,
                (Expression::Not(a), Expression::Not(b)) => equal(a, b),
                _ => false,
            }
        }
        let (base, removed) = navigation::split_globals(self, review);
        let mut choices = vec![];
        for (status, label) in [
            (Status::None, "No reviews"),
            (Status::Required, "Review required"),
            (Status::Approved, "Approved review"),
            (Status::ChangesRequested, "Changes requested"),
        ] {
            choices.push((
                label,
                Expression::Term {
                    source: format!("review:{}", status.name()),
                    value: Term::ReviewStatus(status),
                },
            ));
        }
        for (label, kind, negative) in [
            ("Reviewed by you", super::Kind::ReviewedBy, false),
            ("Not reviewed by you", super::Kind::ReviewedBy, true),
            (
                "Awaiting review from you",
                super::Kind::UserReviewRequested,
                false,
            ),
        ] {
            let atom = Expression::Term {
                source: format!("{}:@me", kind.qualifier()),
                value: Term::Reviewer {
                    kind,
                    name: "@me".into(),
                },
            };
            choices.push((
                label,
                if negative {
                    Expression::Not(Box::new(atom))
                } else {
                    atom
                },
            ));
        }
        Menu {
            clear: navigation::Choice {
                query: navigation::destination(&base),
                selected: removed.is_empty(),
            },
            items: choices
                .into_iter()
                .map(|(label, atom)| {
                    let selected = matches!(removed.as_slice(),[current] if equal(current,&atom));
                    let expression = navigation::conjunction(base.clone(), atom);
                    Item {
                        label,
                        choice: navigation::Choice {
                            query: navigation::destination(&expression),
                            selected,
                        },
                    }
                })
                .collect(),
        }
    }
}

#[cfg(test)]
pub(in crate::cells::repository) mod tests;
