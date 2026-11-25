use std::{collections::HashSet, sync::Arc, time::Duration};

use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::{
    BranchProtection,
    app::{Error, Result},
    auth::Principal,
    cells::repository::rulesets::{
        ActiveRulesets, BypassContext, GetRefPolicy, ReviewDismissalActor, ReviewDismissalPolicy,
        Ruleset, RulesetId, Rulesets,
    },
    cells::repository::{
        BranchProtectionRecord, BranchProtectionSettings, GetBranchProtections,
        GetRepositoryLifecycle, ReplaceBranchProtections, ReplaceBranchProtectionsInput,
        ReplaceBranchProtectionsOutcome, ReplaceRepositoryLifecycle,
        ReplaceRepositoryLifecycleInput, ReplaceRepositoryLifecycleOutcome,
        RepositoryLifecycleRecord,
    },
    config::valid_branch_protections,
    repository::Repository,
    server::Server,
};

const MAX_NUMBER: u64 = 9_007_199_254_740_991;

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct BranchProtections {
    pub version: u64,
    pub rules: Vec<BranchProtection>,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RepositoryLifecycle {
    pub version: u64,
    pub archived: bool,
}

impl BranchProtections {
    pub(crate) fn configured(rules: &[BranchProtection]) -> Self {
        Self {
            version: 0,
            rules: rules.to_vec(),
        }
    }

    pub(crate) fn protection(&self, reference: &str) -> Option<&BranchProtection> {
        let branch = reference.strip_prefix("refs/heads/")?;
        self.rules.iter().find(|rule| rule.branch == branch)
    }
}

pub(crate) async fn load(server: &Server, repo: &Repository) -> Result<BranchProtections> {
    let routed = crate::app::repository_cell(server, repo)?;
    let stored = crate::app::query_output(
        routed
            .client
            .query::<GetBranchProtections>(&routed.target, None, ())
            .await,
    )?;
    stored.map_or_else(
        || {
            Ok(BranchProtections::configured(
                &repo.config.protected_branches,
            ))
        },
        branch_protections,
    )
}

pub(crate) fn configured_queue_required(repo: &Repository, reference: &str) -> bool {
    reference.strip_prefix("refs/heads/").is_some_and(|branch| {
        repo.config
            .protected_branches
            .iter()
            .any(|rule| rule.branch == branch && rule.require_merge_queue)
    })
}

pub(crate) async fn requires_merge_queue(
    server: &Server,
    repo: &Repository,
    reference: &str,
) -> Result<bool> {
    Ok(queue_policy(server, repo, reference).await?.required())
}

pub(crate) struct QueuePolicy {
    classic: bool,
    pub ruleset: bool,
}

impl QueuePolicy {
    pub(crate) fn required(&self) -> bool {
        self.classic || self.ruleset
    }
}

pub(crate) async fn queue_policy(
    server: &Server,
    repo: &Repository,
    reference: &str,
) -> Result<QueuePolicy> {
    let policy = load_ref_policy(server, repo).await?;
    let default_ref = policy.default_ref(repo).await?;
    Ok(QueuePolicy {
        classic: policy
            .protections
            .protection(reference)
            .is_some_and(|rule| rule.require_merge_queue),
        ruleset: policy
            .active()?
            .matches(reference, &default_ref)
            .any(|(_, rule)| rule.require_merge_queue),
    })
}

pub(crate) enum RulesetScope<'a> {
    Organization(uuid::Uuid),
    Repository(&'a Repository),
}

pub(crate) async fn with_ruleset_lease<T, E, F, Fut>(
    server: &Server,
    scope: RulesetScope<'_>,
    cancel: &CancellationToken,
    work: F,
) -> std::result::Result<T, E>
where
    E: From<Error> + From<crab_remote::publication::Error>,
    F: FnOnce(CancellationToken) -> Fut,
    Fut: std::future::Future<Output = std::result::Result<T, E>>,
{
    let layout = match scope {
        RulesetScope::Repository(repo) if repo.organization.is_none() => repo.layout.clone(),
        scope => {
            let organization = match scope {
                RulesetScope::Repository(repo) => repo.organization.ok_or(Error::NotFound)?,
                RulesetScope::Organization(id) => id,
            };
            let catalog = server.catalog().ok_or(Error::NotFound)?;
            catalog.root().organization_policy_layout(organization)
        }
    };
    // Patterns cover branches and repositories that do not exist yet. Final
    // authorization and policy writers share one scope, after any ref leases.
    use crab_remote::publication::Error as LeaseError;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    let mut work = Some(work);
    loop {
        let result = crab_remote::publication::with_internal_lease(
            layout.store(),
            &layout,
            crate::storage_root::POLICY_LEASE_RESOURCE,
            crate::storage_root::POLICY_LEASE_TTL,
            cancel,
            |cancel| {
                let work = work.take();
                async move {
                    let result = match work {
                        Some(work) => work(cancel).await,
                        None => {
                            Err(Error::CellContract("Ruleset lease replayed its callback").into())
                        }
                    };
                    Ok(result)
                }
            },
        )
        .await;
        match result {
            Ok(result) => return result,
            Err(LeaseError::Coordination(crab_coordination::CoordinationError::PushLockHeld {
                ..
            })) if tokio::time::Instant::now() < deadline => {
                tokio::select! {
                    () = cancel.cancelled() => return Err(LeaseError::Cancelled.into()),
                    () = tokio::time::sleep(Duration::from_millis(100)) => {},
                }
            }
            Err(error) => return Err(error.into()),
        }
    }
}

#[derive(Debug, thiserror::Error)]
enum OwnershipLeaseError {
    #[error(transparent)]
    Application(#[from] Error),
    #[error(transparent)]
    Lease(#[from] crab_remote::publication::Error),
}

impl OwnershipLeaseError {
    fn into_application(self) -> Error {
        match self {
            Self::Application(error) => error,
            Self::Lease(error) => Error::TransferPublication(error),
        }
    }
}

pub(crate) async fn with_ownership_lease<T, F, Fut>(
    server: &Server,
    repository: &Repository,
    work: F,
) -> Result<T>
where
    F: FnOnce(CancellationToken) -> Fut,
    Fut: std::future::Future<Output = Result<T>>,
{
    with_ruleset_lease(
        server,
        RulesetScope::Repository(repository),
        &server.cancellation,
        |cancel| async move { work(cancel).await.map_err(OwnershipLeaseError::Application) },
    )
    .await
    .map_err(OwnershipLeaseError::into_application)
}

pub(crate) async fn with_transfer_ownership_leases<T, F, Fut>(
    server: &Server,
    repository: &Repository,
    destination: Option<uuid::Uuid>,
    work: F,
) -> Result<T>
where
    F: FnOnce(CancellationToken) -> Fut,
    Fut: std::future::Future<Output = Result<T>>,
{
    let Some(destination) = destination.filter(|id| repository.organization != Some(*id)) else {
        return with_ownership_lease(server, repository, work).await;
    };
    let source = RulesetScope::Repository(repository);
    let target = RulesetScope::Organization(destination);
    // Distinct organization scopes are acquired in UUID order even for
    // opposing transfers. A personal repository scope always comes last.
    let (first, second) = if repository.organization.is_some_and(|id| id < destination) {
        (source, target)
    } else {
        (target, source)
    };
    with_ruleset_lease(server, first, &server.cancellation, |cancel| async move {
        with_ruleset_lease(server, second, &cancel, |nested| async move {
            work(nested).await.map_err(OwnershipLeaseError::Application)
        })
        .await
    })
    .await
    .map_err(OwnershipLeaseError::into_application)
}

pub(crate) async fn require_no_issue_reservations(
    server: &Server,
    repository: &Repository,
) -> Result<()> {
    use crate::{app, cells::repository::issues::transfer::publication::HasReservations};
    let cell = app::repository_cell(server, repository)?;
    let reserved = app::query_output(
        cell.client
            .query::<HasReservations>(cell.target, None, ())
            .await,
    )?;
    if reserved {
        return Err(Error::RepositoryIssueTransfersPending);
    }
    Ok(())
}

pub(crate) struct RefPolicy {
    pub protections: BranchProtections,
    rulesets: Rulesets,
    inherited: Rulesets,
    organization: Option<uuid::Uuid>,
}

pub(crate) async fn inherited_rulesets(server: &Server, repo: &Repository) -> Result<Rulesets> {
    use crate::cells::{
        Address,
        organization::rulesets::{Applicable, Repository as Selector},
    };
    let Some(organization) = repo.organization else {
        return Ok(Rulesets::default());
    };
    let cells = server.cells().ok_or(Error::CellUnavailable)?;
    crate::app::query_output(
        cells
            .query::<Applicable>(
                Address::Organization(organization),
                None,
                Selector {
                    id: repo.id,
                    name: repo.config.name.clone(),
                    ownership_epoch: repo.ownership_epoch,
                    property_seed: repo.property_seed,
                    fork: repo.forked_from.is_some(),
                },
            )
            .await,
    )
}

pub(crate) async fn check_rename(
    server: &Server,
    record: &crate::catalog::CatalogRecord,
    administrator: crate::cells::directory::IdentityKey,
) -> Result<()> {
    use crate::cells::{Address, organization::rulesets::NameProtection};

    let Some(organization) = record.organization_id() else {
        return Ok(());
    };
    let cells = server.cells().ok_or(Error::CellUnavailable)?;
    let protections = crate::app::query_output(
        cells
            .query::<NameProtection>(
                Address::Organization(organization),
                None,
                record.name.clone(),
            )
            .await,
    )?;
    if protections.is_empty() {
        return Ok(());
    }
    if protections.iter().any(Vec::is_empty) {
        return Err(Error::RepositoryNameProtected);
    }
    let grant = super::access::organization_grants_for_identity(
        server,
        administrator,
        [(organization, true)].into(),
    )
    .await?
    .remove(&organization)
    .unwrap_or_default();
    let context = BypassContext {
        role: Some(crate::RepositoryAccess::Admin),
        organization_owner: grant.owner,
        teams: &grant.bypass_teams,
        organization_teams: &grant.bypass_teams,
    };
    if protections
        .iter()
        .all(|grants| context.allows(grants, false))
    {
        Ok(())
    } else {
        Err(Error::RepositoryNameProtected)
    }
}

pub(crate) async fn load_ref_policy(server: &Server, repo: &Repository) -> Result<RefPolicy> {
    let routed = crate::app::repository_cell(server, repo)?;
    let ((classic, rulesets), inherited) = tokio::try_join!(
        async {
            crate::app::query_output(
                routed
                    .client
                    .query::<GetRefPolicy>(&routed.target, None, ())
                    .await,
            )
        },
        inherited_rulesets(server, repo),
    )?;
    Ok(RefPolicy {
        protections: match classic {
            Some(settings) => branch_protections(settings)?,
            None => BranchProtections::configured(&repo.config.protected_branches),
        },
        rulesets,
        inherited,
        organization: repo.organization,
    })
}

pub(crate) async fn import_ref_policy(server: &Server, repo: &Repository) -> Result<RefPolicy> {
    // The quarantine has no repository Cell until its refs have been verified.
    // Organization policy already exists and applies to the pinned destination.
    Ok(RefPolicy {
        protections: BranchProtections::configured(&[]),
        rulesets: Rulesets::default(),
        inherited: inherited_rulesets(server, repo).await?,
        organization: repo.organization,
    })
}

pub(crate) struct MatchingRulesets<'a> {
    local: ActiveRulesets<'a>,
    inherited: ActiveRulesets<'a>,
    organization: Option<uuid::Uuid>,
}

impl<'a> MatchingRulesets<'a> {
    pub(crate) fn matches(
        &self,
        reference: &str,
        default_ref: &str,
    ) -> impl Iterator<Item = (RulesetId, &'a Ruleset)> {
        self.local
            .matches(reference, default_ref)
            .map(|rule| (RulesetId::Repository(rule.id), rule))
            .chain(
                self.inherited
                    .matches(reference, default_ref)
                    .filter_map(move |rule| {
                        self.organization.map(|organization| {
                            (
                                RulesetId::Organization {
                                    organization,
                                    ruleset: rule.id,
                                },
                                rule,
                            )
                        })
                    }),
            )
    }
}
#[derive(Serialize)]
pub(crate) struct BypassOption {
    pub id: RulesetId,
    pub name: String,
}

pub(crate) struct MergePolicy {
    pub protection: Option<BranchProtection>,
    pub updates_restricted: bool,
    pub linear_history_required: bool,
    pub rulesets: Vec<String>,
    pub bypass_options: Vec<BypassOption>,
}

impl RefPolicy {
    pub(crate) async fn review_dismissal(
        &self,
        server: &Server,
        repo: &Repository,
        grant: &crate::app::Membership,
        actor: &crate::auth::Identity,
        reference: &str,
    ) -> Result<ReviewDismissalPolicy> {
        let default_ref = self.default_ref(repo).await?;
        let rules = self.active()?;
        let allows_user = rules.matches(reference, &default_ref).any(|(_, rule)| {
            rule.review_dismissal
                .iter()
                .flatten()
                .any(|entry| matches!(entry, ReviewDismissalActor::User { .. }))
        });
        let actor_id = if allows_user {
            super::access::actor_id(
                server,
                crate::cells::directory::IdentityKey {
                    issuer: actor.issuer.clone(),
                    subject: actor.subject.clone(),
                },
            )
            .await?
        } else {
            None
        };
        let organization = self.organization.filter(|_| {
            grant.teams.is_empty()
                && !grant.principal.is_local()
                && rules.matches(reference, &default_ref).any(|(id, rule)| {
                    matches!(id, RulesetId::Organization { .. })
                        && rule
                            .review_dismissal
                            .iter()
                            .flatten()
                            .any(|entry| matches!(entry, ReviewDismissalActor::Team { .. }))
                })
        });
        let teams = if let Some(organization) = organization {
            let mut grants = super::access::organization_grants(
                server,
                &grant.principal,
                [(organization, true)].into(),
            )
            .await?;
            Some(
                grants
                    .remove(&organization)
                    .unwrap_or_default()
                    .bypass_teams,
            )
        } else {
            None
        };
        let access = grant
            .bypass_context(repo.id)
            .organization_teams(teams.as_deref());
        Ok(rules.matches(reference, &default_ref).fold(
            ReviewDismissalPolicy::Unrestricted,
            |policy, (id, rule)| {
                policy.combine(rule.dismissal_policy(actor_id, access.for_rule(id).teams))
            },
        ))
    }

    pub(crate) fn active(&self) -> Result<MatchingRulesets<'_>> {
        Ok(MatchingRulesets {
            local: ActiveRulesets::new(&self.rulesets).map_err(Error::Cell)?,
            inherited: ActiveRulesets::new(&self.inherited).map_err(Error::Cell)?,
            organization: self.organization,
        })
    }

    pub(crate) async fn organization_teams(
        &self,
        server: &Server,
        grant: &crate::app::Membership,
    ) -> Result<Option<Vec<uuid::Uuid>>> {
        use crate::cells::repository::rulesets::BypassActor;
        let Some(organization) = self.organization else {
            return Ok(None);
        };
        // Repository ACL resolution already captures these memberships where
        // team grants exist. A rule can still name a team with no repo grant.
        if !grant.teams.is_empty()
            || grant.principal.is_local()
            || !self.inherited.rulesets.iter().any(|rule| {
                rule.bypass_actors
                    .iter()
                    .any(|entry| matches!(entry.actor, BypassActor::Team { .. }))
            })
        {
            return Ok(None);
        }
        if let Some(import) = grant.principal.import_grant() {
            return Ok(Some(import.policy_grant(server).await?.bypass_teams));
        }
        let mut access = crate::repository::access::organization_grants(
            server,
            &grant.principal,
            [(organization, true)].into(),
        )
        .await?;
        Ok(Some(
            access
                .remove(&organization)
                .unwrap_or_default()
                .bypass_teams,
        ))
    }

    pub(crate) async fn default_ref(&self, repo: &Repository) -> Result<String> {
        Ok(
            if self.rulesets.uses_default_branch() || self.inherited.uses_default_branch() {
                repo.open_ref_view().await?.head().to_owned()
            } else {
                String::new()
            },
        )
    }

    pub(crate) fn merge_protection(
        &self,
        reference: &str,
        default_ref: &str,
        actor: BypassContext<'_>,
        bypass: &[RulesetId],
    ) -> Result<MergePolicy> {
        let mut protection = self.protections.protection(reference).cloned();
        let mut linear_history_required = protection
            .as_ref()
            .is_some_and(|rule| rule.require_linear_history);
        let mut checks: HashSet<_> = protection
            .iter()
            .flat_map(|rule| rule.required_checks.iter().map(|name| name.to_lowercase()))
            .collect();
        let mut integrations: HashSet<_> = protection
            .iter()
            .flat_map(|rule| rule.required_integrations.iter().copied())
            .collect();
        let mut names = Vec::new();
        let mut bypass_options = Vec::new();
        let mut accepted = 0;
        let mut updates_restricted = false;
        let rules = self.active()?;
        for (id, ruleset) in rules.matches(reference, default_ref) {
            let name = match id {
                RulesetId::Repository(_) => ruleset.name.clone(),
                RulesetId::Organization { .. } => format!("{} (organization)", ruleset.name),
            };
            names.push(name.clone());
            if !ruleset.requires_pull()
                && !ruleset.restrict_updates
                && !ruleset.require_linear_history
            {
                continue;
            }
            if ruleset.allows_bypass(actor.for_rule(id), true) {
                if bypass.contains(&id) {
                    accepted += 1;
                    continue;
                }
                bypass_options.push(BypassOption { id, name });
            }
            updates_restricted |= ruleset.restrict_updates;
            linear_history_required |= ruleset.require_linear_history;
            if !ruleset.requires_pull() {
                continue;
            }
            let rule = protection.get_or_insert_with(|| BranchProtection {
                branch: reference
                    .strip_prefix("refs/heads/")
                    .unwrap_or(reference)
                    .to_owned(),
                required_approvals: 0,
                required_code_owner_reviews: false,
                require_conversation_resolution: false,
                require_up_to_date: false,
                require_merge_queue: false,
                require_linear_history: false,
                required_checks: Vec::new(),
                required_integrations: Vec::new(),
            });
            rule.required_approvals = rule.required_approvals.max(ruleset.required_approvals);
            rule.required_code_owner_reviews |= ruleset.required_code_owner_reviews;
            rule.require_conversation_resolution |= ruleset.require_conversation_resolution;
            rule.require_up_to_date |= ruleset.require_up_to_date;
            rule.require_merge_queue |= ruleset.require_merge_queue;
            rule.required_checks.extend(
                ruleset
                    .required_checks
                    .iter()
                    .filter(|check| checks.insert(check.to_lowercase()))
                    .cloned(),
            );
            rule.required_integrations.extend(
                ruleset
                    .required_integrations
                    .iter()
                    .filter(|id| integrations.insert(**id))
                    .copied(),
            );
        }
        if accepted != bypass.len() {
            return Err(Error::MergeBypassUnavailable);
        }
        bypass_options.sort_unstable_by_key(|option| option.id);
        Ok(MergePolicy {
            protection,
            updates_restricted,
            linear_history_required,
            rulesets: names,
            bypass_options,
        })
    }
}

pub(crate) async fn replace(
    server: &Arc<Server>,
    repo: &Arc<Repository>,
    principal: Principal,
    expected_version: u64,
    rules: Vec<BranchProtection>,
) -> Result<BranchProtections> {
    if expected_version >= MAX_NUMBER || !valid_branch_protections(&rules) {
        return Err(Error::Invalid(
            "Protection rules require at most 100 unique valid branches, 0–20 approvals, and at most 50 unique check names",
        ));
    }
    let current = load(server, repo).await?;
    if current.version != expected_version {
        return Err(Error::Conflict);
    }
    let required = |rules: &[BranchProtection]| -> HashSet<String> {
        rules
            .iter()
            .filter(|rule| rule.require_merge_queue)
            .map(|rule| format!("refs/heads/{}", rule.branch))
            .collect()
    };
    let names = required(&current.rules)
        .symmetric_difference(&required(&rules))
        .cloned()
        .collect::<Vec<_>>();
    if names.is_empty() {
        return replace_rules(server, repo, expected_version, rules).await;
    }

    let (send, reply) = tokio::sync::oneshot::channel();
    let server = Arc::clone(server);
    let repo = Arc::clone(repo);
    let tracker = server.receives.clone();
    tracker.spawn(async move {
        let server = server.as_ref();
        let repo = repo.as_ref();
        // The policy transition and protected ref publication share a lease.
        // Drain the Cell mutation and release it even if the HTTP caller leaves.
        let result = crab_remote::publication::with_leases(
            &repo.store,
            &repo.layout,
            names,
            Duration::from_secs(300),
            &server.cancellation,
            |_, cancel| async move {
                if cancel.is_cancelled() {
                    return Err(Error::MergeBusy);
                }
                let grant = crate::app::membership(server, &principal, repo).await?;
                if !grant.principal.can_admin(repo.id) {
                    return Err(Error::Forbidden);
                }
                replace_rules(server, repo, expected_version, rules).await
            },
        )
        .await;
        let _ = send.send(result);
    });
    reply.await.map_err(|_| Error::MergeBusy)?
}

async fn replace_rules(
    server: &Server,
    repo: &Repository,
    expected_version: u64,
    rules: Vec<BranchProtection>,
) -> Result<BranchProtections> {
    let routed = crate::app::repository_cell(server, repo)?;
    match crate::app::command_output(
        routed
            .client
            .command::<ReplaceBranchProtections>(
                &routed.target,
                crate::app::mutation_identity()?,
                ReplaceBranchProtectionsInput {
                    expected_version,
                    rules: rules.into_iter().map(protection_record).collect(),
                },
            )
            .await,
    )? {
        ReplaceBranchProtectionsOutcome::Updated(settings) => branch_protections(settings),
        ReplaceBranchProtectionsOutcome::Conflict => Err(Error::Conflict),
    }
}

pub(crate) async fn load_lifecycle(
    server: &Server,
    repo: &Repository,
) -> Result<RepositoryLifecycle> {
    let routed = crate::app::repository_cell(server, repo)?;
    repository_lifecycle(crate::app::query_output(
        routed
            .client
            .query::<GetRepositoryLifecycle>(&routed.target, None, ())
            .await,
    )?)
}

pub(crate) async fn replace_lifecycle(
    server: &Server,
    repo: &Repository,
    expected_version: u64,
    archived: bool,
) -> Result<RepositoryLifecycle> {
    if expected_version >= MAX_NUMBER {
        return Err(Error::Conflict);
    }
    let routed = crate::app::repository_cell(server, repo)?;
    match crate::app::command_output(
        routed
            .client
            .command::<ReplaceRepositoryLifecycle>(
                &routed.target,
                crate::app::mutation_identity()?,
                ReplaceRepositoryLifecycleInput {
                    expected_version,
                    archived,
                },
            )
            .await,
    )? {
        ReplaceRepositoryLifecycleOutcome::Updated(lifecycle) => repository_lifecycle(lifecycle),
        ReplaceRepositoryLifecycleOutcome::Conflict => Err(Error::Conflict),
        ReplaceRepositoryLifecycleOutcome::Unchanged => {
            Err(Error::Invalid("Repository lifecycle is unchanged"))
        }
    }
}

pub(crate) fn protection_record(rule: BranchProtection) -> BranchProtectionRecord {
    BranchProtectionRecord {
        branch: rule.branch,
        required_approvals: rule.required_approvals,
        required_code_owner_reviews: rule.required_code_owner_reviews,
        require_conversation_resolution: rule.require_conversation_resolution,
        require_up_to_date: rule.require_up_to_date,
        require_merge_queue: rule.require_merge_queue,
        require_linear_history: rule.require_linear_history,
        required_checks: rule.required_checks,
        required_integrations: rule.required_integrations,
    }
}

fn branch_protections(settings: BranchProtectionSettings) -> Result<BranchProtections> {
    let settings = BranchProtections {
        version: settings.version,
        rules: settings
            .rules
            .into_iter()
            .map(|rule| BranchProtection {
                branch: rule.branch,
                required_approvals: rule.required_approvals,
                required_code_owner_reviews: rule.required_code_owner_reviews,
                require_conversation_resolution: rule.require_conversation_resolution,
                require_up_to_date: rule.require_up_to_date,
                require_merge_queue: rule.require_merge_queue,
                require_linear_history: rule.require_linear_history,
                required_checks: rule.required_checks,
                required_integrations: rule.required_integrations,
            })
            .collect(),
    };
    if settings.version == 0
        || settings.version >= MAX_NUMBER
        || !valid_branch_protections(&settings.rules)
    {
        return Err(Error::CellContract(
            "Cell returned invalid branch protections",
        ));
    }
    Ok(settings)
}

fn repository_lifecycle(record: RepositoryLifecycleRecord) -> Result<RepositoryLifecycle> {
    if record.version >= MAX_NUMBER {
        return Err(Error::CellContract(
            "Cell returned an invalid repository lifecycle",
        ));
    }
    Ok(RepositoryLifecycle {
        version: record.version,
        archived: record.archived,
    })
}
