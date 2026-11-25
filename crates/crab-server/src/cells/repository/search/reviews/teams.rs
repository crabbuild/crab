use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{Query, Term};
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

    pub(crate) fn review_team_users(&self) -> BTreeSet<String> {
        self.names(|term| match term {
            Term::Reviewer { kind, name } if kind.includes_teams() => Some(name),
            _ => None,
        })
    }

    pub(in crate::cells::repository::search) fn uses_review_teams(&self) -> bool {
        self.expression.has_term(|term| match term {
            Term::Reviewer { kind, .. } => kind.includes_teams(),
            Term::TeamReviewRequested(_) => true,
            _ => false,
        })
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct Bindings {
    pub named: Vec<(String, Option<Uuid>)>,
    pub mine: Vec<Uuid>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub users: Vec<(String, Vec<Uuid>)>,
}

impl Bindings {
    pub(in crate::cells::repository::search) fn validate(
        &self,
        query: &Query,
    ) -> std::result::Result<(), &'static str> {
        let users = query.review_team_users();
        let ordered_ids = |ids: &[Uuid]| {
            ids.windows(2).all(|pair| pair[0] < pair[1]) && !ids.iter().any(Uuid::is_nil)
        };
        if !self
            .named
            .iter()
            .map(|(name, _)| name)
            .eq(query.review_team_names().iter())
            || self
                .named
                .iter()
                .any(|(_, id)| id.is_some_and(|id| id.is_nil()))
            || !self
                .users
                .iter()
                .map(|(name, _)| name)
                .eq(users.iter().filter(|name| name.as_str() != "@me"))
            || (!self.mine.is_empty() && !users.contains("@me"))
            || !ordered_ids(&self.mine)
            || self.users.iter().any(|(_, ids)| !ordered_ids(ids))
            || self.mine.len()
                > crate::cells::organization::teams::repository_access::MAX_TEAM_ACCESS_ROWS
            || self.users.iter().map(|(_, ids)| ids.len()).sum::<usize>()
                > crate::cells::organization::teams::repository_access::MAX_TEAM_ACCESS_ROWS
        {
            return Err("Team review filters require ordered, resolved organization bindings");
        }
        Ok(())
    }

    pub(in crate::cells::repository::search) fn named(&self, name: &str) -> Option<Uuid> {
        self.named.iter().find(|(key, _)| key == name)?.1
    }

    pub(in crate::cells::repository::search) fn for_user(&self, name: &str) -> &[Uuid] {
        if name == "@me" {
            return &self.mine;
        }
        self.users
            .iter()
            .find(|(key, _)| key == name)
            .map_or(&[], |(_, ids)| ids.as_slice())
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
