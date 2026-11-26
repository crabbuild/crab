#![expect(
    clippy::result_large_err,
    reason = "handlers and their helpers return rejections as ready axum responses"
)]

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

use axum::{
    Extension, Json, Router,
    extract::{Path, State},
    response::{IntoResponse, Response},
    routing::get,
};
use crab_remote_git::{
    ContentClassification, EntryKind, GitPath, OperationContext, OperationKind, RemoteGitSnapshot,
    Revision, TreeChange,
};
use serde::Serialize;
use tokio_util::sync::CancellationToken;

use crate::{
    RepositoryAccess,
    api::read_admission,
    app::{self, Error, Membership, Result},
    auth::Principal,
    cells::{
        Address,
        directory::{IdentityKey, LookupAccounts, ResolveHandles, validate_handle},
        organization::teams::repository_access::{CodeOwnerTeamQuery, CodeOwnerTeams},
        repository::{
            MergeReviewers, RepositoryAuthor,
            review_requests::automatic::{MAX_RECIPIENTS, Plan},
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
mod saved;

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
    #[serde(skip_serializing_if = "Option::is_none")]
    missing_paths: Option<usize>,
    problem: Option<&'static str>,
}

impl Report {
    pub(super) fn unavailable() -> Self {
        Self {
            problem: Some("Code owners could not be evaluated for this comparison."),
            missing_paths: Some(0),
            ..Self::default()
        }
    }

    pub(super) fn satisfied(&self) -> bool {
        self.missing_paths == Some(0) && self.problem.is_none()
    }

    fn coverage(
        &mut self,
        groups: Vec<MatchedRule>,
        owners: Option<&BTreeMap<String, ResolvedOwner>>,
    ) {
        self.missing_paths = owners.map(|_| 0);
        for group in groups {
            let (approved, has_eligible_owner) = match owners {
                Some(owners) => {
                    let mut eligible = group
                        .rule
                        .owners
                        .iter()
                        .filter_map(|owner| owners.get(owner))
                        .peekable();
                    let has_owner = eligible.peek().is_some();
                    (Some(eligible.any(|owner| owner.approved)), Some(has_owner))
                }
                None => (None, None),
            };
            self.affected_paths += group.affected;
            if approved == Some(false)
                && let Some(missing) = &mut self.missing_paths
            {
                *missing += group.affected;
            }
            self.groups.push(Group {
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
        self.groups
            .sort_by_key(|group| (group.approved, group.line));
        self.groups.truncate(MAX_GROUPS_SHOWN);
    }
}

#[derive(Serialize)]
struct Group {
    line: usize,
    owners: Vec<String>,
    paths: Vec<String>,
    affected: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    approved: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    has_eligible_owner: Option<bool>,
}

pub(super) fn routes() -> Router<Arc<Server>> {
    Router::new()
        .route(
            "/api/repos/{owner}/{name}/pulls/{number}/codeowners",
            get(get_owners),
        )
        .route(
            "/api/repos/{owner}/{name}/pulls/{number}/codeowners/source",
            get(saved::source),
        )
        .route("/api/repos/{owner}/{name}/file-codeowners", get(file::show))
}

async fn get_owners(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path(address): Path<(String, String, u64)>,
) -> std::result::Result<Response, Response> {
    let admission = read_admission::acquire_bulk(&server).map_err(|_| read_admission::busy())?;
    let (repo, pull) = load_pull(&server, &principal, address)
        .await
        .map_err(IntoResponse::into_response)?;
    let report = if let Some(merge) = &pull.merge {
        saved::coverage(&server, &principal, &repo, &pull, merge).await?
    } else {
        live_report(&server, &principal, &repo, &pull)
            .await
            .map_err(IntoResponse::into_response)?
    };
    Ok(read_admission::retain(
        Json(report).into_response(),
        admission,
    ))
}

async fn load_pull(
    server: &Server,
    principal: &Principal,
    (owner, name, number): (String, String, u64),
) -> Result<(Arc<Repository>, PullRequest)> {
    let repo = app::repository(server, principal, &(owner, name))?;
    let pull = storage::pull(server, &repo, app::number(number)?)
        .await?
        .ok_or(Error::NotFound)?;
    Ok((repo, pull))
}

async fn live_report(
    server: &Server,
    principal: &Principal,
    repo: &Repository,
    pull: &PullRequest,
) -> Result<Report> {
    let git = PullGit::open(server, repo, pull.head_repository).await?;
    let Some((base, head)) = git.tips(&pull.base_ref, &pull.head_ref) else {
        return Ok(Report::unavailable());
    };
    let membership = app::membership(server, principal, repo).await?;
    let eligible = membership.merge_reviewers(server).await?;
    let report = evaluate(
        server,
        repo,
        &git,
        &membership,
        &eligible,
        pull,
        (&base, &head),
    )
    .await?;
    app::membership(server, principal, repo).await?;
    Ok(report)
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
    report.coverage(groups, Some(&owners));
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
    for team in teams {
        let plan_for_team = crate::repository::access::team_review_plan(
            server,
            &membership,
            team,
            Some(&eligibility),
        )
        .await?;
        subjects.extend(plan_for_team.recipients.iter().cloned());
        if let Some(routing) = &plan_for_team.routing {
            subjects.extend(
                routing
                    .candidates
                    .iter()
                    .map(|reviewer| reviewer.subject.clone()),
            );
        }
        plan.teams.push(plan_for_team);
    }
    if subjects.len() <= MAX_RECIPIENTS {
        crate::repository::access::add_review_history(server, repo, &mut plan.teams).await;
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
        let Some(text) = report_policy(&mut report, load_policy(&base, &operation).await?) else {
            return Ok(None);
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
    let loaded = operation.finish(loaded).await.map_err(crate::Error::from)?;
    match_loaded(report, loaded, cancel).await
}

fn report_policy(report: &mut Report, policy: Option<PolicySource>) -> Option<String> {
    let policy = policy?;
    report.path = Some(policy.path);
    match policy.text {
        Ok(text) => Some(text),
        Err(problem) => {
            report.problem = Some(problem);
            None
        }
    }
}

async fn match_loaded(
    mut report: Report,
    loaded: Option<(String, Vec<TreeChange>)>,
    cancel: CancellationToken,
) -> Result<(Report, Vec<MatchedRule>)> {
    let Some((text, changes)) = loaded else {
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
    cancel: &CancellationToken,
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
