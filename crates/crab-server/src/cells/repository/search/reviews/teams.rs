use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{Kind, Query, Term};
use crate::cells::{
    Result,
    application::{SqlStatement, SqlValue, Transaction},
    repository::{integer, review_requests, statement},
};

pub(crate) const QUALIFIER: &str = "team-review-requested";

pub(in crate::cells::repository::search) fn name(
    value: &str,
) -> std::result::Result<String, &'static str> {
    match value.split_once('/') {
        Some((owner, team))
            if crate::config::valid_repository_component(owner)
                && !team.is_empty()
                && team.chars().count() <= 100
                && team == team.trim() =>
        {
            Ok(value.to_lowercase())
        }
        _ => Err("Team review search requires a repository owner and team name"),
    }
}

impl Query {
    pub(crate) fn review_team_names(&self) -> BTreeSet<String> {
        self.names(|term| match term {
            Term::TeamReviewRequested(name) => Some(name),
            _ => None,
        })
    }

    pub(crate) fn includes_my_team_requests(&self) -> bool {
        self.names(|term| match term {
            Term::Reviewer {
                kind: Kind::ReviewRequested,
                name,
            } if name == "@me" => Some(name),
            _ => None,
        })
        .contains("@me")
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct Bindings {
    pub named: Vec<(String, Option<Uuid>)>,
    pub mine: Vec<Uuid>,
}

impl Bindings {
    pub(in crate::cells::repository::search) fn validate(
        &self,
        query: &Query,
    ) -> std::result::Result<(), &'static str> {
        if !self
            .named
            .iter()
            .map(|(name, _)| name)
            .eq(query.review_team_names().iter())
            || self
                .named
                .iter()
                .any(|(_, id)| id.is_some_and(|id| id.is_nil()))
            || (!self.mine.is_empty() && !query.includes_my_team_requests())
            || self.mine.len()
                > crate::cells::organization::teams::repository_access::MAX_TEAM_ACCESS_ROWS
            || self.mine.windows(2).any(|pair| pair[0] >= pair[1])
            || self.mine.iter().any(Uuid::is_nil)
        {
            return Err("Team review filters require ordered, resolved organization bindings");
        }
        Ok(())
    }

    pub(in crate::cells::repository::search) fn named(&self, name: &str) -> Option<Uuid> {
        self.named.iter().find(|(key, _)| key == name)?.1
    }
}

pub(in crate::cells::repository::search) fn project(
    context: &Transaction<'_>,
    number: u64,
    document: u64,
    statements: &mut Vec<SqlStatement>,
) -> Result<()> {
    // Use the current request record. Permanent notification markers also
    // survive cancellation and fulfillment and cannot answer this query.
    for request in review_requests::teams::load_teams(context, number)? {
        if request.state == review_requests::State::Requested {
            statements.push(statement(
                "INSERT INTO discussion_index_team_requests(document, team) VALUES (?, ?)",
                vec![integer(document)?, SqlValue::Text(request.team.to_string())],
            ));
        }
    }
    Ok(())
}
