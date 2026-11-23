use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

use axum::{
    Extension, Json, Router,
    extract::{Path, State},
    routing::get,
};
use crab_remote_git::{
    ContentClassification, EntryKind, GitPath, OperationContext, OperationKind, RemoteGitSnapshot,
    Revision, TreeChange,
};
use serde::Serialize;

use crate::{
    RepositoryAccess,
    app::{self, Error, Membership, Result},
    auth::Principal,
    cells::{
        Address,
        directory::{IdentityKey, LookupAccounts, ResolveHandles, validate_handle},
        organization::teams::repository_access::{CodeOwnerTeamQuery, CodeOwnerTeams},
        repository::{
            MergeReviewers, RepositoryAuthor,
            review_requests::automatic::{MAX_RECIPIENTS, Plan, TeamRecipients},
        },
    },
    repository::Repository,
    server::Server,
};

use super::{
    git::PullGit,
    storage::{self, PullRequest, ReviewState},
};

mod file;
mod patterns;

const FILES: [&str; 3] = [".github/CODEOWNERS", "CODEOWNERS", "docs/CODEOWNERS"];
const MAX_BYTES: u64 = 3 * 1024 * 1024;
const MAX_GROUPS_SHOWN: usize = 30;
const MAX_EXAMPLES: usize = 4;

#[derive(Default, Serialize)]
pub(super) struct Report {
    path: Option<&'static str>,
    base_oid: Option<String>,
    head_oid: Option<String>,
    invalid_lines: usize,
    groups: Vec<Group>,
    affected_paths: usize,
    missing_paths: usize,
    problem: Option<&'static str>,
}

impl Report {
    pub(super) fn unavailable() -> Self {
        Self {
            problem: Some("Code owners could not be evaluated for this comparison."),
            ..Self::default()
        }
    }

    pub(super) fn satisfied(&self) -> bool {
        self.missing_paths == 0 && self.problem.is_none()
    }
}

#[derive(Serialize)]
struct Group {
    line: usize,
    owners: Vec<String>,
    paths: Vec<String>,
    affected: usize,
    approved: bool,
    has_eligible_owner: bool,
}

pub(super) fn routes() -> Router<Arc<Server>> {
    Router::new()
        .route(
            "/api/repos/{owner}/{name}/pulls/{number}/codeowners",
            get(get_owners),
        )
        .route("/api/repos/{owner}/{name}/file-codeowners", get(file::show))
}

async fn get_owners(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((owner, name, number)): Path<(String, String, u64)>,
) -> Result<Json<Report>> {
    let repo = app::repository(&server, &principal, &(owner, name))?;
    let pull = storage::pull(&server, &repo, app::number(number)?)
        .await?
        .ok_or(Error::NotFound)?;
    let git = PullGit::open(&server, &repo, pull.head_repository).await?;
    let tips = pull
        .merge
        .as_ref()
        .map(|merge| (merge.base_oid.clone(), merge.head_oid.clone()))
        .or_else(|| git.tips(&pull.base_ref, &pull.head_ref));
    let Some((base, head)) = tips else {
        return Ok(Json(Report::unavailable()));
    };
    let membership = app::membership(&server, &principal, &repo).await?;
    let eligible = membership.merge_reviewers(&server).await?;
    let report = evaluate(
        &server,
        &repo,
        &git,
        &membership,
        &eligible,
        &pull,
        (&base, &head),
    )
    .await?;
    app::membership(&server, &principal, &repo).await?;
    Ok(Json(report))
}

pub(super) async fn evaluate(
    server: &Server,
    repo: &Repository,
    git: &PullGit,
    membership: &Membership,
    eligibility: &MergeReviewers,
    pull: &PullRequest,
    (base, head): (&str, &str),
) -> Result<Report> {
    let (mut report, groups) = match_comparison(server, git, (base, head)).await?;
    let approvals: BTreeSet<_> = pull
        .review_decisions
        .iter()
        .filter_map(|decision| {
            (decision.state == ReviewState::Approved
                && decision.commit_oid == head
                && eligibility.contains(&decision.author.issuer, &decision.author.subject))
            .then_some(decision.author.subject.as_str())
        })
        .collect();
    let referenced = referenced_owners(&groups);
    let owners = resolve(
        server,
        repo,
        membership,
        eligibility,
        &referenced,
        &approvals,
    )
    .await?;
    for group in groups {
        let eligible = group
            .rule
            .owners
            .iter()
            .filter_map(|owner| owners.get(owner));
        let has_eligible_owner = eligible.clone().next().is_some();
        let approved = eligible.into_iter().any(|owner| owner.approved);
        report.affected_paths += group.affected;
        if !approved {
            report.missing_paths += group.affected;
        }
        report.groups.push(Group {
            line: group.rule.line,
            owners: group
                .rule
                .owners
                .into_iter()
                .take(8)
                .map(|owner| owner.chars().take(160).collect())
                .collect(),
            paths: group.paths,
            affected: group.affected,
            approved,
            has_eligible_owner,
        });
    }
    report
        .groups
        .sort_by_key(|group| (group.approved, group.line));
    report.groups.truncate(MAX_GROUPS_SHOWN);
    Ok(report)
}

pub(super) async fn requests(
    server: &Server,
    repo: &Repository,
    principal: &Principal,
    git: &PullGit,
    tips: (&str, &str),
) -> Result<Plan> {
    let (_, groups) = match_comparison(server, git, tips).await?;
    let mut plan = Plan::default();
    if groups.is_empty() {
        return Ok(plan);
    }
    let membership = app::membership(server, principal, repo).await?;
    let eligibility = membership.merge_reviewers(server).await?;
    let owners = resolve(
        server,
        repo,
        &membership,
        &eligibility,
        &referenced_owners(&groups),
        &BTreeSet::new(),
    )
    .await?;
    let mut subjects = BTreeSet::new();
    let mut teams = BTreeSet::new();
    for (handle, owner) in owners {
        match owner.identity {
            OwnerIdentity::Person(account) if subjects.insert(account.subject.clone()) => {
                plan.people.push(RepositoryAuthor {
                    issuer: account.issuer,
                    subject: account.subject,
                    name: handle.trim_start_matches('@').into(),
                });
            }
            OwnerIdentity::Team(team) => {
                teams.insert(team);
            }
            OwnerIdentity::Person(_) => {}
        }
    }
    if let Some(organization) = membership.organization {
        for team in teams {
            let recipients =
                crate::repository::access::team_review_recipients(server, organization, team)
                    .await?;
            subjects.extend(recipients.iter().cloned());
            plan.teams.push(TeamRecipients { team, recipients });
        }
    }
    if subjects.len() > MAX_RECIPIENTS || serde_json::to_vec(&plan)?.len() > 1024 * 1024 {
        return Err(Error::Invalid(
            "Automatic code owner requests exceed the delivery limit",
        ));
    }
    Ok(plan)
}

async fn match_comparison(
    server: &Server,
    git: &PullGit,
    (base, head): (&str, &str),
) -> Result<(Report, Vec<MatchedRule>)> {
    let cancel = server.cancellation.child_token();
    let _cancel_on_drop = cancel.clone().drop_guard();
    let operation = git
        .base
        .operation(OperationKind::Compare, &cancel)
        .await
        .map_err(crate::Error::from)?;
    let mut report = Report {
        base_oid: Some(base.into()),
        head_oid: Some(head.into()),
        ..Report::default()
    };
    let loaded: crab_remote_git::Result<_> = async {
        let base = git
            .base
            .snapshot(&Revision::parse(base)?, &operation)
            .await?;
        let Some(policy) = load_policy(&base, &operation).await? else {
            return Ok(None);
        };
        report.path = Some(policy.path);
        let text = match policy.text {
            Ok(text) => text,
            Err(problem) => {
                report.problem = Some(problem);
                return Ok(None);
            }
        };
        let revision = Revision::parse(head)?;
        if let Some(fork) = &git.fork {
            let source = fork.operation(OperationKind::Compare, &cancel).await?;
            let comparison = async {
                let head = fork.snapshot(&revision, &source).await?;
                let ancestor =
                    crate::api::query::comparison_ancestor(&head, &base, &source, &operation)
                        .await?;
                let ancestor = fork.snapshot(&Revision::Commit(ancestor), &source).await?;
                let delta = head.compare(&ancestor, &source).await?;
                Ok(Some((text, delta.changes)))
            }
            .await;
            return source.finish(comparison).await;
        }
        let head = git.base.snapshot(&revision, &operation).await?;
        let ancestor =
            crate::api::query::merge_base_snapshot(&git.base, &base, &head, &operation).await?;
        let delta = head.compare(&ancestor, &operation).await?;
        Ok(Some((text, delta.changes)))
    }
    .await;
    let Some((text, changes)) = operation.finish(loaded).await.map_err(crate::Error::from)? else {
        return Ok((report, Vec::new()));
    };
    let matched = tokio::task::spawn_blocking(move || match_rules(&text, &changes, &cancel))
        .await
        .map_err(crate::Error::Worker)?;
    let (invalid_lines, groups) = match matched {
        Ok(result) => result,
        Err(problem) => {
            report.problem = Some(problem);
            return Ok((report, Vec::new()));
        }
    };
    report.invalid_lines = invalid_lines;
    if referenced_owners(&groups).len() > 1024 {
        report.problem = Some("CODEOWNERS exceeds the limit of 1,024 owners in one comparison.");
        return Ok((report, Vec::new()));
    }
    Ok((report, groups))
}

struct PolicySource {
    path: &'static str,
    text: std::result::Result<String, &'static str>,
}

async fn load_policy(
    snapshot: &RemoteGitSnapshot,
    operation: &OperationContext,
) -> crab_remote_git::Result<Option<PolicySource>> {
    for file in FILES {
        let path = GitPath::new(file.as_bytes().to_vec())?;
        let entry = match snapshot.entry(&path, operation).await {
            Err(crab_remote_git::Error::PathComponentNotTree { .. }) => None,
            result => result?,
        };
        let Some(entry) = entry else {
            continue;
        };
        let text = if entry.kind != EntryKind::Blob {
            Err("CODEOWNERS must be a regular Git file.")
        } else if operation.read_object_metadata(entry.oid).await?.size >= MAX_BYTES {
            Err("CODEOWNERS exceeds the 3 MiB file limit.")
        } else {
            let blob = snapshot.read_blob(&path, operation).await?;
            if blob.metadata.classification != ContentClassification::OrdinaryGit
                || blob.bytes.contains(&0)
            {
                Err("CODEOWNERS must contain UTF-8 text.")
            } else {
                String::from_utf8(blob.bytes.into())
                    .map_err(|_| "CODEOWNERS must contain UTF-8 text.")
            }
        };
        // The first existing policy has precedence even when it is invalid.
        return Ok(Some(PolicySource { path: file, text }));
    }
    Ok(None)
}

fn referenced_owners(groups: &[MatchedRule]) -> BTreeSet<&str> {
    groups
        .iter()
        .flat_map(|group| group.rule.owners.iter().map(String::as_str))
        .collect()
}

struct MatchedRule {
    rule: patterns::Rule,
    affected: usize,
    paths: Vec<String>,
}

fn match_rules(
    text: &str,
    changes: &[TreeChange],
    cancel: &tokio_util::sync::CancellationToken,
) -> std::result::Result<(usize, Vec<MatchedRule>), &'static str> {
    let parsed = patterns::Rules::parse(text)?;
    let mut matches = BTreeMap::<usize, (usize, Vec<String>)>::new();
    for change in changes {
        if cancel.is_cancelled() {
            return Err("Code owner matching was cancelled.");
        }
        let Some(index) = parsed.matching(change.path.as_bytes()) else {
            continue;
        };
        if parsed.rules[index].owners.is_empty() {
            continue;
        }
        let (count, paths) = matches.entry(index).or_default();
        *count += 1;
        if paths.len() < MAX_EXAMPLES {
            paths.push(crate::api::display_path(change.path.as_bytes()));
        }
    }
    let groups = parsed
        .rules
        .into_iter()
        .enumerate()
        .filter_map(|(index, rule)| {
            matches.remove(&index).map(|(affected, paths)| MatchedRule {
                rule,
                affected,
                paths,
            })
        })
        .collect();
    Ok((parsed.invalid_lines, groups))
}

struct ResolvedOwner {
    identity: OwnerIdentity,
    approved: bool,
}

enum OwnerIdentity {
    Person(IdentityKey),
    Team(uuid::Uuid),
}

async fn resolve(
    server: &Server,
    repo: &Repository,
    membership: &Membership,
    eligibility: &MergeReviewers,
    owners: &BTreeSet<&str>,
    approvals: &BTreeSet<&str>,
) -> Result<BTreeMap<String, ResolvedOwner>> {
    let mut resolved = BTreeMap::new();
    let handles: Vec<_> = owners
        .iter()
        .filter_map(|owner| owner.strip_prefix('@'))
        .filter(|handle| validate_handle(handle).is_ok())
        .collect();
    if owners.is_empty() {
        return Ok(resolved);
    }
    let cells = server.cells().ok_or(Error::CellUnavailable)?;
    for chunk in handles.chunks(64) {
        let accounts = app::query_output(
            cells
                .query::<ResolveHandles>(
                    Address::Directory,
                    None,
                    chunk.iter().map(|handle| (*handle).into()).collect(),
                )
                .await,
        )?;
        if accounts.len() != chunk.len() {
            return Err(Error::CellContract(
                "Code owner handle lookup returned the wrong number of accounts",
            ));
        }
        for (handle, account) in chunk.iter().zip(accounts) {
            if let Some(account) =
                account.filter(|account| eligibility.contains(&account.issuer, &account.subject))
            {
                let approved = approvals.contains(account.subject.as_str());
                resolved.insert(
                    format!("@{handle}"),
                    ResolvedOwner {
                        identity: OwnerIdentity::Person(account),
                        approved,
                    },
                );
            }
        }
    }
    let Some(organization) = membership.organization else {
        return Ok(resolved);
    };
    let prefix = format!("@{}/", repo.config.owner);
    let teams: Vec<_> = membership
        .teams
        .iter()
        .filter(|team| team.access >= RepositoryAccess::Write)
        .map(|team| team.team)
        .collect();
    if teams.is_empty() || !owners.iter().any(|owner| owner.starts_with(&prefix)) {
        return Ok(resolved);
    }
    let approvers = if approvals.is_empty() {
        Vec::new()
    } else {
        let identities: Vec<_> = approvals
            .iter()
            .map(|subject| IdentityKey {
                issuer: eligibility.issuer.clone(),
                subject: (*subject).into(),
            })
            .collect();
        let ids = app::query_output(
            cells
                .query::<LookupAccounts>(Address::Directory, None, identities)
                .await,
        )?;
        if ids.len() != approvals.len() {
            return Err(Error::CellContract(
                "Code owner approval lookup returned the wrong number of accounts",
            ));
        }
        ids.into_iter().flatten().collect()
    };
    let team_names = app::query_output(
        cells
            .query::<CodeOwnerTeams>(
                Address::Organization(organization),
                None,
                CodeOwnerTeamQuery { teams, approvers },
            )
            .await,
    )?;
    let mut matches = BTreeMap::new();
    for team in team_names {
        let slug = team
            .name
            .split(|ch: char| !ch.is_ascii_alphanumeric())
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join("-")
            .to_ascii_lowercase();
        if slug.is_empty() {
            continue;
        }
        let handle = format!("{prefix}{slug}");
        if owners.contains(handle.as_str()) {
            // Display names can yield ambiguous slugs; neither team may satisfy that reference.
            matches
                .entry(handle)
                .and_modify(|value| *value = None)
                .or_insert(Some(ResolvedOwner {
                    identity: OwnerIdentity::Team(team.id),
                    approved: team.approved,
                }));
        }
    }
    resolved.extend(
        matches
            .into_iter()
            .filter_map(|(name, owner)| owner.map(|owner| (name, owner))),
    );
    Ok(resolved)
}
